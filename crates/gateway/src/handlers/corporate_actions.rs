//! `corporate_actions.stream`.

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use futures_util::StreamExt as _;
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

/// The Alpaca body, ending as soon as shutdown starts so the connection
/// drain leaves the grace period for detached work.
fn until_shutdown(body: Body, shutdown: CancellationToken) -> Body {
    Body::from_stream(
        body.into_data_stream()
            .take_until(shutdown.cancelled_owned()),
    )
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
    use futures_util::stream;

    use super::*;

    #[tokio::test]
    async fn the_relay_body_ends_when_shutdown_starts() {
        let shutdown = CancellationToken::new();
        let upstream = Body::from_stream(stream::pending::<Result<Bytes, Infallible>>());
        let body = until_shutdown(upstream, shutdown.clone());
        let mut read = tokio::spawn(to_bytes(body, usize::MAX));

        tokio::task::yield_now().await;
        assert!(!read.is_finished());
        shutdown.cancel();

        let bytes = tokio::time::timeout(Duration::from_secs(1), &mut read)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(bytes.is_empty());
    }
}
