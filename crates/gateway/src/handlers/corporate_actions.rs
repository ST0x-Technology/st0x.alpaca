//! `corporate_actions.stream`.

use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use st0x_alpaca::corporate_actions::CorporateActionReplay;
use st0x_alpaca_gateway_api::Operation;
use st0x_alpaca_gateway_api::dto::issuer::ReplayQuery;

use crate::answer::{Failure, corporate_actions};
use crate::extract::Query;
use crate::state::{AppState, Call, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::CorporateActionsStream => get(stream),
        _ => return None,
    })
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
            let response = open_state
                .issuer()?
                .corporate_actions
                .connect_raw(&replay)
                .await
                .map_err(|error| corporate_actions(&error))?;

            let body = axum::body::Body::new(reqwest::Body::from(response));
            let content_type = [(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))];
            Ok((content_type, body).into_response())
        })
        .await
}
