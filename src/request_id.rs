//! Alpaca request traffic seen while running a future.
//!
//! A caller that audits and budgets its Alpaca traffic (the gateway) wraps
//! one operation in [`collect`] and gets back what the operation sent and
//! what Alpaca answered, without any method signature carrying it: how many
//! Alpaca API requests went out, the `X-Request-ID` of every answer, and the
//! status of the last answer. A caller that must see that traffic even when
//! the operation's future is dropped before it finishes (its own caller went
//! away) runs it under [`collect_into`] with a [`TrafficHandle`] it keeps.
//! Outside a scope recording is a no op.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Response, StatusCode};

/// Header Alpaca puts its request id in.
pub const ALPACA_REQUEST_ID_HEADER: &str = "x-request-id";

/// The Alpaca traffic of one [`collect`] or [`collect_into`] scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Traffic {
    /// The request id of every Alpaca answer that carried one, in order,
    /// token mint answers included.
    pub request_ids: Vec<String>,
    /// Alpaca API requests that may have reached Alpaca, answered or not. A
    /// send that failed before its request could leave (the request never
    /// built, or no connection was made) is not counted, and token mints are
    /// not API requests.
    pub requests_sent: u32,
    /// The status of the last Alpaca answer, token mint answers included.
    pub last_status: Option<u16>,
}

/// Shared view of one [`collect_into`] scope's traffic. Every clone sees the
/// same traffic, and it stays readable after the future recording into it is
/// dropped.
#[derive(Debug, Clone, Default)]
pub struct TrafficHandle(Arc<Mutex<Traffic>>);

impl TrafficHandle {
    /// The traffic recorded so far.
    #[must_use]
    pub fn snapshot(&self) -> Traffic {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Traffic> {
        // Recording never panics while it holds the lock, so a poisoned
        // lock still guards whole traffic.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

tokio::task_local! {
    static TRAFFIC: TrafficHandle;
}

/// Runs `future` and returns its output with the Alpaca traffic it made.
pub async fn collect<Output>(future: impl Future<Output = Output>) -> (Output, Traffic) {
    let handle = TrafficHandle::default();
    let output = collect_into(handle.clone(), future).await;
    let traffic = std::mem::take(&mut *handle.lock());
    (output, traffic)
}

/// Runs `future`, recording its Alpaca traffic into `handle` as it happens,
/// so traffic recorded before the returned future is dropped stays readable
/// through any clone of `handle`.
pub async fn collect_into<Output>(
    handle: TrafficHandle,
    future: impl Future<Output = Output>,
) -> Output {
    TRAFFIC.scope(handle, future).await
}

/// Sends one Alpaca API request, counting it in the active scope. It is
/// counted before it goes out, so a request whose answer never arrives, or
/// whose send is dropped half way, still counts. The count is taken back only
/// when the send failed before the request could leave: it never built, or no
/// connection was made (the rule an order POST uses to call a failure
/// unwritten).
pub(crate) async fn send(request: RequestBuilder) -> reqwest::Result<Response> {
    update(|traffic| traffic.requests_sent = traffic.requests_sent.saturating_add(1));
    let sent = request.send().await;
    if sent
        .as_ref()
        .is_err_and(|error| error.is_builder() || error.is_connect())
    {
        update(|traffic| traffic.requests_sent = traffic.requests_sent.saturating_sub(1));
    }
    sent
}

/// Records one Alpaca answer, when a scope is active: its status and its
/// request id. Every client calls this for every answer, success or error,
/// before reading the body.
pub(crate) fn record(status: StatusCode, headers: &HeaderMap) {
    let id = headers
        .get(ALPACA_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    update(|traffic| {
        traffic.last_status = Some(status.as_u16());
        if let Some(id) = id {
            traffic.request_ids.push(id.to_string());
        }
    });
}

fn update(change: impl FnOnce(&mut Traffic)) {
    let _ = TRAFFIC.try_with(|handle| change(&mut handle.lock()));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use httpmock::prelude::*;
    use reqwest::header::HeaderValue;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;

    /// A server that accepts one connection and never answers it: a
    /// request sent to it has left, and its answer never comes. The
    /// receiver fires once the connection is accepted.
    async fn silent_server() -> (String, oneshot::Receiver<()>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/silent", listener.local_addr().unwrap());
        let (accepted, on_accept) = oneshot::channel();
        let holder = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            let _ = accepted.send(());
            std::future::pending::<()>().await;
        });
        (url, on_accept, holder)
    }

    #[tokio::test]
    async fn a_scope_counts_only_requests_that_may_have_left_and_keeps_the_last_answer() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/answered");
            then.status(503).header(ALPACA_REQUEST_ID_HEADER, "b");
        });
        server.mock(|when, then| {
            when.method(GET).path("/missing");
            then.status(404);
        });
        let (silent_url, _, holder) = silent_server().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();

        let ((), traffic) = collect(async {
            let answered = send(client.get(server.url("/answered"))).await.unwrap();
            record(answered.status(), answered.headers());

            let refused = send(client.get("http://127.0.0.1:1/refused"))
                .await
                .unwrap_err();
            assert!(refused.is_connect(), "{refused:?}");

            let unbuilt = send(client.get("not a url")).await.unwrap_err();
            assert!(unbuilt.is_builder(), "{unbuilt:?}");

            let unanswered = send(client.get(&silent_url)).await.unwrap_err();
            assert!(unanswered.is_timeout(), "{unanswered:?}");

            let missing = send(client.get(server.url("/missing"))).await.unwrap();
            record(missing.status(), missing.headers());
        })
        .await;
        holder.abort();

        assert_eq!(
            traffic,
            Traffic {
                request_ids: vec!["b".to_string()],
                requests_sent: 3,
                last_status: Some(404),
            }
        );
    }

    #[tokio::test]
    async fn a_dropped_operation_leaves_its_traffic_in_the_handle() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/answered");
            then.status(200).header(ALPACA_REQUEST_ID_HEADER, "a");
        });
        let (silent_url, on_accept, holder) = silent_server().await;
        let client = reqwest::Client::new();
        let handle = TrafficHandle::default();

        let operation = collect_into(handle.clone(), async {
            let answered = send(client.get(server.url("/answered"))).await.unwrap();
            record(answered.status(), answered.headers());
            let _ = send(client.get(&silent_url)).await;
            unreachable!("the silent server never answers");
        });
        // The caller goes away while the second request waits for its
        // answer, dropping the operation.
        tokio::select! {
            () = operation => {}
            accepted = on_accept => accepted.unwrap(),
        }
        holder.abort();

        assert_eq!(
            handle.snapshot(),
            Traffic {
                request_ids: vec!["a".to_string()],
                requests_sent: 2,
                last_status: Some(200),
            }
        );
    }

    #[test]
    fn recording_outside_a_scope_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(ALPACA_REQUEST_ID_HEADER, HeaderValue::from_static("a"));
        update(|traffic| traffic.requests_sent += 1);
        record(StatusCode::OK, &headers);
    }
}
