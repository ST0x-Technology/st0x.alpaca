//! `issuer.*`.

use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use st0x_alpaca_gateway_api::Operation;
use st0x_alpaca_gateway_api::dto::issuer::{MintCallbackRequest, RedeemRequest, RequestPath};

use crate::answer::issuer_call;
use crate::extract::{Body, Params};
use crate::state::{AppState, Call, Done, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::IssuerMintCallback => post(mint_callback),
        Operation::IssuerRedeem => post(redeem),
        Operation::IssuerRequest => get(request),
        _ => return None,
    })
}

async fn mint_callback(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<MintCallbackRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.tokenization_request_id)
        .note("network", &request.network)
        .note("recipient", &request.wallet_address);

    state
        .run(call, intent, move |state, _| async move {
            let request_id = request.tokenization_request_id.clone();
            state
                .issuer()?
                .mint_callbacks
                .send_mint_callback_reporting(request.into())
                .await
                .map(|()| Done::new((), &request_id))
                .map_err(|failure| issuer_call(&failure))
        })
        .await
}

async fn redeem(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<RedeemRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.issuer_request_id.0)
        .note("symbol", &request.underlying_symbol.0)
        .note("quantity", &request.quantity.as_str())
        .note("network", &request.network)
        .note("wallet", &request.wallet_address);

    state
        .run(call, intent, move |state, _| async move {
            let redeemed = state
                .issuer()?
                .redemptions
                .call_redeem_endpoint_reporting(request.into())
                .await
                .map_err(|failure| issuer_call(&failure))?;

            let object_id = redeemed.tokenization_request_id.clone();
            Ok(Done::new(redeemed, &object_id))
        })
        .await
}

async fn request(
    State(state): State<AppState>,
    call: Call,
    Params(RequestPath {
        tokenization_request_id: request_id,
    }): Params<RequestPath>,
) -> Response {
    let intent = Intent::default().key(&request_id);
    state
        .read(call, intent, |state| async move {
            state
                .issuer()?
                .requests
                .poll_request_status_reporting(&request_id)
                .await
                .map_err(|failure| issuer_call(&failure))
        })
        .await
}
