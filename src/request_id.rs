//! Alpaca request ids seen while running a future, and the gate that decides
//! whether its requests may still leave.
//!
//! [`collect`] returns a future's output with the request id of every Alpaca
//! answer and the status of the last Alpaca API answer. [`gated`] runs a
//! future under a [`SendGate`]: once [`SendGate::close`] returned, no Alpaca
//! request and no credential mint of that future starts. Outside a scope
//! nothing is recorded and every request may leave.

use std::cell::RefCell;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Response, StatusCode};

/// Header Alpaca puts its request id in.
pub const ALPACA_REQUEST_ID_HEADER: &str = "x-request-id";

/// The Alpaca traffic of one [`collect`] scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Traffic {
    /// The request id of every Alpaca answer that carried one, in order,
    /// credential mint answers included.
    pub request_ids: Vec<String>,
    /// The status of the last Alpaca API answer. A credential mint answer
    /// never sets it.
    pub last_status: Option<u16>,
}

/// Whether the requests of a [`gated`] scope may still leave. Every Alpaca
/// request asks it right before it leaves and holds its lock until the
/// request is in the HTTP client's hands; a credential mint asks it before
/// it starts. A closed gate holds the request back with [`GateClosed`].
/// Clones share one gate.
#[derive(Clone)]
pub struct SendGate(Arc<Gate<dyn Fn() -> bool + Send + Sync>>);

struct Gate<IsOpen: ?Sized> {
    closed: Mutex<bool>,
    is_open: IsOpen,
}

impl SendGate {
    /// A gate that is open until [`Self::close`] and while `is_open` answers
    /// true. `is_open` runs under the gate's lock, so it must be cheap, and
    /// it must never answer true after it answered false: every client
    /// classifies a held back request as permanent.
    pub fn new(is_open: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(Gate {
            closed: Mutex::new(false),
            is_open,
        }))
    }

    /// Closes the gate for good, after a request that already found it open
    /// is in the HTTP client's hands.
    pub fn close(&self) {
        *self.lock() = true;
    }

    fn hold(&self) -> Result<MutexGuard<'_, bool>, GateClosed> {
        let closed = self.lock();
        if *closed || !(self.0.is_open)() {
            Err(GateClosed)
        } else {
            Ok(closed)
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        // The flag is only ever set, so a lock poisoned by a panicking
        // `is_open` still guards a whole flag.
        self.0.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for SendGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SendGate").finish_non_exhaustive()
    }
}

/// A request a closed [`SendGate`] held back. It never left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the request was not sent: its send gate was closed")]
pub struct GateClosed;

/// The failure of [`send`]: the HTTP client's own, or a closed gate.
#[derive(Debug)]
pub(crate) enum SendError {
    Http(reqwest::Error),
    NotSent(GateClosed),
}

tokio::task_local! {
    static TRAFFIC: RefCell<Traffic>;
    static GATE: SendGate;
}

/// Runs `future` and returns its output with the Alpaca traffic it made.
pub async fn collect<Output>(future: impl Future<Output = Output>) -> (Output, Traffic) {
    TRAFFIC
        .scope(RefCell::default(), async {
            let output = future.await;
            (output, TRAFFIC.with(RefCell::take))
        })
        .await
}

/// Runs `future` with every Alpaca request and credential mint it starts
/// asking `gate` first. In nested scopes the innermost gate decides.
pub async fn gated<Output>(gate: SendGate, future: impl Future<Output = Output>) -> Output {
    GATE.scope(gate, future).await
}

/// Sends one Alpaca API request unless the active [`SendGate`] holds it
/// back. Callers send only once everything the request needs is in hand.
pub(crate) async fn send(request: RequestBuilder) -> Result<Response, SendError> {
    start(request)?.await.map_err(SendError::Http)
}

/// Asks the active gate and, under its lock, hands the request to the HTTP
/// client, which builds the request's future without awaiting.
fn start(
    request: RequestBuilder,
) -> Result<impl Future<Output = reqwest::Result<Response>>, SendError> {
    let gate = GATE.try_with(SendGate::clone).ok();
    let _open = gate
        .as_ref()
        .map(SendGate::hold)
        .transpose()
        .map_err(SendError::NotSent)?;
    Ok(request.send())
}

/// [`GateClosed`] when the active [`gated`] scope's gate is closed. A
/// credential mint asks it before it starts.
pub(crate) fn ensure_open() -> Result<(), GateClosed> {
    GATE.try_with(|gate| gate.hold().map(drop))
        .unwrap_or(Ok(()))
}

/// Records one Alpaca API answer: its status and its request id. Every
/// client calls it right after its send returns, before reading the body.
pub(crate) fn record(status: StatusCode, headers: &HeaderMap) {
    update(|traffic| traffic.last_status = Some(status.as_u16()));
    record_id(headers);
}

/// Records an answer's request id only, as a credential mint answer does:
/// its status is not Alpaca's answer to the operation.
pub(crate) fn record_id(headers: &HeaderMap) {
    if let Some(id) = headers
        .get(ALPACA_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        update(|traffic| traffic.request_ids.push(id.to_string()));
    }
}

fn update(change: impl FnOnce(&mut Traffic)) {
    let _ = TRAFFIC.try_with(|traffic| change(&mut traffic.borrow_mut()));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use httpmock::prelude::*;
    use reqwest::header::HeaderValue;
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn a_scope_keeps_every_request_id_and_the_last_api_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/answered");
            then.status(503).header(ALPACA_REQUEST_ID_HEADER, "b");
        });
        server.mock(|when, then| {
            when.method(GET).path("/missing");
            then.status(404);
        });
        let client = reqwest::Client::new();
        let mut mint = HeaderMap::new();
        mint.insert(ALPACA_REQUEST_ID_HEADER, HeaderValue::from_static("m"));

        let ((), traffic) = collect(async {
            let answered = send(client.get(server.url("/answered"))).await.unwrap();
            record(answered.status(), answered.headers());
            let missing = send(client.get(server.url("/missing"))).await.unwrap();
            record(missing.status(), missing.headers());
            record_id(&mint);
        })
        .await;

        assert_eq!(
            traffic,
            Traffic {
                request_ids: vec!["b".to_string(), "m".to_string()],
                last_status: Some(404),
            }
        );
    }

    /// The gate is asked by each request as it would leave, not once for
    /// the scope: a request sent while it is open leaves, and one sent after
    /// a clone of it closed never reaches the server.
    #[tokio::test]
    async fn a_send_gate_holds_back_every_request_after_it_closes() {
        let server = MockServer::start_async().await;
        let answered = server.mock(|when, then| {
            when.method(GET).path("/answered");
            then.status(200);
        });
        let client = reqwest::Client::new();
        let gate = SendGate::new(|| true);

        gated(gate.clone(), async {
            send(client.get(server.url("/answered"))).await.unwrap();
            gate.close();
            let held = send(client.post(server.url("/answered")))
                .await
                .unwrap_err();
            assert!(matches!(held, SendError::NotSent(GateClosed)), "{held:?}");
        })
        .await;

        answered.assert_calls_async(1).await;
    }

    /// A close racing a send on another worker: the sender found the gate
    /// open, so the close returns only once that request is handed over, and
    /// the send after the close is held back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_waits_for_a_send_that_found_the_gate_open() {
        let server = MockServer::start_async().await;
        let written = server.mock(|when, then| {
            when.method(POST).path("/write");
            then.status(200);
        });
        let client = reqwest::Client::new();
        let (asking, asked) = std::sync::mpsc::channel();
        let answered = Arc::new(AtomicBool::new(false));
        let gate = SendGate::new({
            let answered = Arc::clone(&answered);
            move || {
                // Long enough for the close to land before this answer,
                // unless the gate's lock holds it back.
                let _ = asking.send(());
                std::thread::sleep(Duration::from_millis(200));
                answered.store(true, Ordering::SeqCst);
                true
            }
        });
        let (closed, on_close) = oneshot::channel::<()>();
        let url = server.url("/write");
        let sender = tokio::spawn(gated(gate.clone(), async move {
            let first = send(client.post(&url)).await;
            on_close.await.unwrap();
            let second = send(client.post(&url)).await;
            (first, second)
        }));

        let open_at_close = tokio::task::spawn_blocking({
            let gate = gate.clone();
            move || {
                asked.recv().unwrap();
                gate.close();
                answered.load(Ordering::SeqCst)
            }
        })
        .await
        .unwrap();
        closed.send(()).unwrap();
        let (first, second) = sender.await.unwrap();

        assert!(open_at_close);
        assert_eq!(first.unwrap().status(), StatusCode::OK);
        assert!(
            matches!(second, Err(SendError::NotSent(GateClosed))),
            "{second:?}"
        );
        written.assert_calls_async(1).await;
    }
}
