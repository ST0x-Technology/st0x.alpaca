//! `orders.*` and `conversions.*`.

use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use st0x_alpaca::broker::{AlpacaLimitOrder, ClientOrderId, ConversionOrder};
use st0x_alpaca_gateway_api::dto::orders::{
    CancelOrderRequest, CancelOrderResponse, ClientOrderIdPath, Conversion,
    ConversionOrderResponse, ConversionRequest, ExactLimitOrderRequest, FindConversionResponse,
    FindOrderResponse, LimitOrderRequest, MarketOrderRequest, OrderIdPath, OrderStateResponse,
    PlacementResponse, RecoverOrderRequest, RecoverOrderResponse,
};
use st0x_alpaca_gateway_api::{Operation, Tier};

use crate::answer::{Failure, Sent, broker, placement};
use crate::extract::{Body, Params};
use crate::state::{AppState, Call, Done, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::OrdersPlaceMarket => post(place_market),
        Operation::OrdersPlaceLimit => post(place_limit),
        Operation::OrdersPlaceExactLimit => post(place_exact_limit),
        Operation::OrdersGet => get(order),
        Operation::OrdersFind => get(find_order),
        Operation::OrdersRecover => post(recover_order),
        Operation::OrdersCancel => post(cancel),
        Operation::ConversionsSubmit => post(submit_conversion),
        Operation::ConversionsGet => get(conversion),
        Operation::ConversionsFind => get(find_conversion),
        _ => return None,
    })
}

/// Refuses a key whose form does not match the tier: the bot keys with the
/// bare UUID, a human writer with `cli-{uuid}`, so the Alpaca dashboard tells
/// automated orders from manual ones and neither can collide with the other.
fn require_key_form(tier: Tier, key: &ClientOrderId) -> Result<(), Failure> {
    let expected = match tier {
        Tier::Bot => "a bare UUID",
        Tier::Write => "a cli- prefixed UUID",
        Tier::Read => return Err(Failure::invalid("readers place no orders")),
    };
    match (tier, key) {
        (Tier::Bot, ClientOrderId::Automated(_)) | (Tier::Write, ClientOrderId::Cli(_)) => Ok(()),
        _ => Err(Failure::invalid(format!(
            "client order id {key} must be {expected} on this tier"
        ))),
    }
}

/// Refuses a human key on a bot lookup: the bot adopts the order it finds
/// under a key as its own, so it may only look up the bare UUID keys it
/// places with. Humans may look up any key.
fn require_bot_lookup_key(tier: Tier, key: &ClientOrderId) -> Result<(), Failure> {
    match (tier, key) {
        (Tier::Bot, ClientOrderId::Cli(_)) => Err(Failure::invalid(format!(
            "client order id {key} belongs to a human order; the bot may only look up bare UUID \
             keys"
        ))),
        _ => Ok(()),
    }
}

async fn place_market(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<MarketOrderRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.client_order_id)
        .reason(request.reason.as_deref())
        .note("symbol", &request.symbol)
        .note("side", &request.direction)
        .note("quantity", &request.shares);
    let tier = call.principal.tier;
    let work_state = state.clone();
    state
        .mutate(call, intent, async move {
            require_key_form(tier, &request.client_order_id)?;
            let placed = work_state
                .broker
                .place_market_order_reporting(request.into())
                .await
                .map_err(|failure| placement(&failure))?;
            let order_id = placed.order_id.clone();
            Ok(Done::new(PlacementResponse::from(placed), &order_id))
        })
        .await
}

async fn place_limit(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<LimitOrderRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.client_order_id)
        .reason(request.reason.as_deref())
        .note("symbol", &request.symbol)
        .note("side", &request.direction)
        .note("quantity", &request.shares)
        .note("limitPrice", &request.limit_price);
    let tier = call.principal.tier;
    let work_state = state.clone();
    state
        .mutate(call, intent, async move {
            require_key_form(tier, &request.client_order_id)?;
            let placed = work_state
                .broker
                .place_limit_order_reporting(request.into())
                .await
                .map_err(|failure| placement(&failure))?;
            let order_id = placed.order_id.clone();
            Ok(Done::new(PlacementResponse::from(placed), &order_id))
        })
        .await
}

async fn place_exact_limit(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<ExactLimitOrderRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.client_order_id)
        .reason(request.reason.as_deref())
        .note("symbol", &request.symbol)
        .note("side", &request.direction)
        .note("quantity", &request.shares)
        .note("limitPrice", &request.limit_price);
    let tier = call.principal.tier;
    let work_state = state.clone();
    state
        .mutate(call, intent, async move {
            require_key_form(tier, &request.client_order_id)?;
            // Nothing is sent when the limit price does not convert.
            let order =
                AlpacaLimitOrder::try_from(request).map_err(|error| broker(&error, Sent::Read))?;
            let placed = work_state
                .broker
                .place_alpaca_limit_order_reporting(order)
                .await
                .map_err(|failure| placement(&failure))?;
            let order_id = placed.order_id.clone();
            Ok(Done::new(PlacementResponse::from(placed), &order_id))
        })
        .await
}

async fn order(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<OrderIdPath>,
) -> Response {
    let intent = Intent::default().key(&path.order_id);
    state
        .read(call, intent, async {
            state
                .broker
                .get_order_status(&path.order_id.to_string())
                .await
                .map(OrderStateResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn find_order(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<ClientOrderIdPath>,
) -> Response {
    let tier = call.principal.tier;
    let intent = Intent::default().key(&path.client_order_id);
    state
        .read(call, intent, async {
            require_bot_lookup_key(tier, &path.client_order_id)?;
            state
                .broker
                .get_order_by_client_order_id(&path.client_order_id)
                .await
                .map(|order| FindOrderResponse {
                    order: order.map(Into::into),
                })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn recover_order(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<RecoverOrderRequest>,
) -> Response {
    let tier = call.principal.tier;
    let intent = Intent::default().key(&request.client_order_id);
    state
        .read(call, intent, async {
            require_bot_lookup_key(tier, &request.client_order_id)?;
            state
                .broker
                .recover_order_by_client_id(&request.into())
                .await
                .map(|order| RecoverOrderResponse {
                    order: order.map(Into::into),
                })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn cancel(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<OrderIdPath>,
    Body(request): Body<CancelOrderRequest>,
) -> Response {
    let order_id = path.order_id.to_string();
    let intent = Intent::of(&request)
        .key(&order_id)
        .reason(request.reason.as_deref());
    let work_state = state.clone();
    state
        .mutate(call, intent, async move {
            let outcome = work_state
                .broker
                .cancel_order(&order_id)
                .await
                .map_err(|error| broker(&error, Sent::Mutation))?;
            Ok(Done::new(
                CancelOrderResponse {
                    outcome: outcome.into(),
                },
                &order_id,
            ))
        })
        .await
}

async fn submit_conversion(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<ConversionRequest>,
) -> Response {
    let (side, asset, amount) = match request.conversion {
        Conversion::SellUsdc { quantity } => ("SELL", "USDC", quantity.to_string()),
        Conversion::BuyWithUsd { notional } => ("BUY", "USD", notional.to_string()),
    };
    let intent = Intent::of(&request)
        .key(&request.client_order_id)
        .reason(request.reason.as_deref())
        .note("symbol", &"USDCUSD")
        .note("side", &side)
        .note("asset", &asset)
        .note("amount", &amount);
    let tier = call.principal.tier;
    let work_state = state.clone();
    state
        .mutate(call, intent, async move {
            require_key_form(tier, &request.client_order_id)?;
            let order = work_state
                .broker
                .submit_conversion(
                    ConversionOrder::from(request.conversion),
                    &request.client_order_id,
                )
                .await
                .map_err(|error| broker(&error, Sent::Mutation))?;
            let order_id = order.id;
            Ok(Done::new(ConversionOrderResponse::from(order), &order_id))
        })
        .await
}

async fn conversion(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<OrderIdPath>,
) -> Response {
    let intent = Intent::default().key(&path.order_id);
    state
        .read(call, intent, async {
            state
                .broker
                .get_conversion_order(path.order_id)
                .await
                .map(ConversionOrderResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn find_conversion(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<ClientOrderIdPath>,
) -> Response {
    let tier = call.principal.tier;
    let intent = Intent::default().key(&path.client_order_id);
    state
        .read(call, intent, async {
            require_bot_lookup_key(tier, &path.client_order_id)?;
            state
                .broker
                .find_conversion_order(&path.client_order_id)
                .await
                .map(|order| FindConversionResponse {
                    order: order.map(Into::into),
                })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}
