//! `market.*` and `assets.*`.

use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use st0x_alpaca_gateway_api::Operation;
use st0x_alpaca_gateway_api::dto::SymbolPath;
use st0x_alpaca_gateway_api::dto::market::{
    CounterTradeSharesRequest, IsOpenResponse, LatestTradeResponse, OvernightQuoteResponse,
    QuoteResponse, SessionResponse,
};

use crate::answer::{Sent, broker};
use crate::extract::{Body, Params};
use crate::state::{AppState, Call, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::MarketIsOpen => get(is_open),
        Operation::MarketSession => get(session),
        Operation::MarketSessionStatus => get(session_status),
        Operation::MarketLatestTrade => get(latest_trade),
        Operation::MarketLatestQuote => get(latest_quote),
        Operation::MarketLatestOvernightQuote => get(latest_overnight_quote),
        Operation::AssetsGet => get(asset),
        Operation::AssetsCounterTradeShares => post(counter_trade_shares),
        _ => return None,
    })
}

async fn is_open(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .is_market_open()
                .await
                .map(|open| IsOpenResponse { open })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn session(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .market_session()
                .await
                .map(|session| SessionResponse { session })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn session_status(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .market_session_status()
                .await
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn latest_trade(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .fetch_latest_trade_price(&path.symbol)
                .await
                .map(|price| LatestTradeResponse { price })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn latest_quote(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .fetch_latest_quote(&path.symbol)
                .await
                .map(QuoteResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn latest_overnight_quote(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .fetch_latest_overnight_quote(&path.symbol)
                .await
                .map(OvernightQuoteResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn asset(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .get_asset_details(&path.symbol)
                .await
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn counter_trade_shares(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
    Body(request): Body<CounterTradeSharesRequest>,
) -> Response {
    let intent = Intent::default()
        .key(&path.symbol)
        .note("quantity", &request.shares);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .prepare_counter_trade_shares(&path.symbol, request.shares, request.extended_hours)
                .await
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}
