//! `corporate_actions.stream`.

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use futures_util::{StreamExt as _, stream};
use st0x_alpaca::corporate_actions::CorporateActionReplay;
use st0x_alpaca_gateway_api::Operation;
use st0x_alpaca_gateway_api::dto::issuer::ReplayQuery;
use tokio_util::sync::CancellationToken;

use crate::answer::{Failure, corporate_actions};
use crate::extract::Query;
use crate::state::{AppState, Call, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::CorporateActionsStream => get(stream),
        _ => return None,
    })
}

/// The Alpaca body. Shutdown interrupts it with a transport error so a
/// bounded replay cannot mistake a cut response for a complete window.
fn until_shutdown(body: Body, shutdown: CancellationToken) -> Body {
    let body = stream::unfold(
        (body.into_data_stream(), shutdown, false),
        |(mut body, shutdown, finished)| async move {
            if finished {
                return None;
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => Some((
                    Err(axum::Error::new(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "gateway shutdown interrupted corporate action stream",
                    ))),
                    (body, shutdown, true),
                )),
                item = body.next() => {
                    item.map(|item| (item, (body, shutdown, false)))
                }
            }
        },
    );
    Body::from_stream(body)
}

/// Relays Alpaca's event stream from the replay position, byte for byte.
async fn stream(
    State(state): State<AppState>,
    call: Call,
    Query(query): Query<ReplayQuery>,
) -> Response {
    let intent = Intent::default();
    let open_state = state.clone();
    state
        .relay(call, intent, async move {
            let replay = CorporateActionReplay::try_from(query)
                .map_err(|error| Failure::invalid(error.to_string()))?;
            let shutdown = open_state.shutdown.clone();
            let response = open_state
                .issuer()?
                .corporate_actions
                .connect_raw(&replay)
                .await
                .map_err(|error| corporate_actions(&error))?;

            let body = Body::new(reqwest::Body::from(response));
            let body = until_shutdown(body, shutdown);
            let content_type = [(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))];
            Ok((content_type, body).into_response())
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use axum::body::{Bytes, to_bytes};

    use super::*;

    #[tokio::test]
    async fn the_relay_body_fails_when_shutdown_starts() {
        let shutdown = CancellationToken::new();
        let upstream = Body::from_stream(stream::pending::<Result<Bytes, Infallible>>());
        let body = until_shutdown(upstream, shutdown.clone());
        let mut read = tokio::spawn(to_bytes(body, usize::MAX));

        tokio::task::yield_now().await;
        assert!(!read.is_finished());
        shutdown.cancel();

        let error = tokio::time::timeout(Duration::from_secs(1), &mut read)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "gateway shutdown interrupted corporate action stream"
        );
    }
}
