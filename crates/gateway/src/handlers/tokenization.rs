//! `tokenization.*`.

use alloy_primitives::TxHash;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use st0x_alpaca::tokenization::{
    AlpacaTokenizationService, IssuerRequestId, TokenizationRequestId,
};
use st0x_alpaca_gateway_api::dto::tokenization::{
    LookupResponse, MintRequest, NetworkQuery, RequestsQuery, RequestsResponse,
};
use st0x_alpaca_gateway_api::{ErrorCode, Operation};

use crate::answer::{Failure, Sent, tokenization};
use crate::extract::{Body, Params, Query};
use crate::state::{AppState, Call, Done, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::TokenizationMint => post(mint),
        Operation::TokenizationRequests => get(requests),
        Operation::TokenizationRequest => get(request),
        Operation::TokenizationFindMint => get(find_mint),
        Operation::TokenizationFindRedemption => get(find_redemption),
        _ => return None,
    })
}

/// A client for the account wide reads, which Alpaca answers the same
/// through any network's client.
fn account_wide(state: &AppState) -> Result<&AlpacaTokenizationService, Failure> {
    let network = state.config.tokenization.networks.first().ok_or_else(|| {
        Failure::new(
            ErrorCode::CapabilityDisabled,
            "no tokenization network is configured on this deployment",
        )
    })?;
    state.tokenizer(*network)
}

async fn mint(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<MintRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.issuer_request_id)
        .reason(request.reason.as_deref())
        .note("symbol", &request.symbol)
        .note("quantity", &request.quantity)
        .note("network", &request.network)
        .note("recipient", &request.wallet_address);

    state
        .run(call, intent, move |state, _| async move {
            if !state
                .config
                .tokenization
                .mint_recipients
                .contains(&request.wallet_address)
            {
                return Err(Failure::destination_not_allowed(format!(
                    "{} is not a pinned mint recipient of this deployment",
                    request.wallet_address
                )));
            }
            let tokenizer = state.tokenizer(request.network)?;

            let minted = tokenizer
                .request_mint(
                    request.symbol,
                    request.quantity.inner(),
                    request.wallet_address,
                    request.issuer_request_id,
                )
                .await
                .map_err(|error| tokenization(&error, Sent::Mutation))?;

            let object_id = minted.id.clone();
            Ok(Done::new(minted, &object_id))
        })
        .await
}

async fn requests(
    State(state): State<AppState>,
    call: Call,
    Query(query): Query<RequestsQuery>,
) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            let tokenizer = account_wide(&state)?;
            let listed = if query.pending_only.unwrap_or(false) {
                tokenizer.list_pending_requests().await
            } else {
                tokenizer.list_requests().await
            };

            listed
                .map(|requests| RequestsResponse { requests })
                .map_err(|error| tokenization(&error, Sent::Read))
        })
        .await
}

async fn request(
    State(state): State<AppState>,
    call: Call,
    Params(request_id): Params<TokenizationRequestId>,
    Query(query): Query<NetworkQuery>,
) -> Response {
    let intent = Intent::default()
        .key(&request_id)
        .note("network", &query.network);
    state
        .read(call, intent, |state| async move {
            state
                .tokenizer(query.network)?
                .get_request(&request_id)
                .await
                .map_err(|error| tokenization(&error, Sent::Read))
        })
        .await
}

async fn find_mint(
    State(state): State<AppState>,
    call: Call,
    Params(issuer_request_id): Params<IssuerRequestId>,
) -> Response {
    let intent = Intent::default().key(&issuer_request_id);
    state
        .read(call, intent, |state| async move {
            account_wide(&state)?
                .find_mint_by_issuer_request_id(&issuer_request_id)
                .await
                .map(|request| LookupResponse { request })
                .map_err(|error| tokenization(&error, Sent::Read))
        })
        .await
}

async fn find_redemption(
    State(state): State<AppState>,
    call: Call,
    Params(tx_hash): Params<TxHash>,
    Query(query): Query<NetworkQuery>,
) -> Response {
    let intent = Intent::default()
        .key(&tx_hash)
        .note("network", &query.network);
    state
        .read(call, intent, |state| async move {
            state
                .tokenizer(query.network)?
                .find_redemption_by_tx(&tx_hash)
                .await
                .map(|request| LookupResponse { request })
                .map_err(|error| tokenization(&error, Sent::Read))
        })
        .await
}
