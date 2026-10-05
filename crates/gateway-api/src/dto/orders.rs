//! `orders.*` and `conversions.*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{
    AlpacaBrokerApiError, AlpacaLimitOrder, AlpacaLimitPrice, CancellationOutcome, ClientOrderId,
    ConversionOrder, CryptoOrderResponse, Direction, ExecutorOrderId, LimitOrder, MarketOrder,
    OrderFailureTerminality, OrderPlacement, OrderState, RecoveredOrderPlacement,
};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usd, Usdc};
use uuid::Uuid;

/// Path parameter of the routes keyed by an Alpaca order id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderIdPath {
    pub order_id: Uuid,
}

/// Path parameter of the routes keyed by a client order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOrderIdPath {
    pub client_order_id: ClientOrderId,
}

/// `orders.place_market` request. The bot sends the bare UUID key, a human
/// writer the `cli-` prefixed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketOrderRequest {
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub client_order_id: ClientOrderId,
    pub reason: Option<String>,
}

impl From<MarketOrderRequest> for MarketOrder {
    fn from(request: MarketOrderRequest) -> Self {
        Self {
            symbol: request.symbol,
            shares: request.shares,
            direction: request.direction,
            client_order_id: request.client_order_id,
        }
    }
}

impl From<MarketOrder> for MarketOrderRequest {
    fn from(order: MarketOrder) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            client_order_id: order.client_order_id,
            reason: None,
        }
    }
}

/// `orders.place_limit` request: the automated limit order, whose quantity
/// the gateway truncates to the asset's precision for the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitOrderRequest {
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub limit_price: Positive<Usd>,
    pub extended_hours: bool,
    pub client_order_id: ClientOrderId,
    pub reason: Option<String>,
}

impl From<LimitOrderRequest> for LimitOrder {
    fn from(request: LimitOrderRequest) -> Self {
        Self {
            symbol: request.symbol,
            shares: request.shares,
            direction: request.direction,
            limit_price: request.limit_price,
            extended_hours: request.extended_hours,
            client_order_id: request.client_order_id,
        }
    }
}

impl From<LimitOrder> for LimitOrderRequest {
    fn from(order: LimitOrder) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            limit_price: order.limit_price,
            extended_hours: order.extended_hours,
            client_order_id: order.client_order_id,
            reason: None,
        }
    }
}

/// `orders.place_exact_limit` request: an operator limit order placed with
/// exactly the quantity given, without the fractionability truncation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExactLimitOrderRequest {
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub limit_price: Positive<Usd>,
    pub extended_hours: bool,
    pub client_order_id: ClientOrderId,
    pub reason: Option<String>,
}

impl TryFrom<ExactLimitOrderRequest> for AlpacaLimitOrder {
    type Error = AlpacaBrokerApiError;

    /// Fails when the limit price has more decimals than Alpaca accepts.
    fn try_from(request: ExactLimitOrderRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            symbol: request.symbol,
            shares: request.shares,
            direction: request.direction,
            limit_price: AlpacaLimitPrice::try_new(request.limit_price)?,
            extended_hours: request.extended_hours,
            client_order_id: request.client_order_id,
        })
    }
}

impl From<AlpacaLimitOrder> for ExactLimitOrderRequest {
    fn from(order: AlpacaLimitOrder) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            limit_price: order.limit_price.into_inner(),
            extended_hours: order.extended_hours,
            client_order_id: order.client_order_id,
            reason: None,
        }
    }
}

/// `orders.place_market`, `orders.place_limit` and
/// `orders.place_exact_limit`: the order Alpaca holds under the key. After a
/// duplicate key it is the order a prior attempt placed, with its terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementResponse {
    pub order_id: String,
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub placed_at: DateTime<Utc>,
    pub extended_hours: bool,
    pub limit_price: Option<Positive<Usd>>,
}

impl From<OrderPlacement<String>> for PlacementResponse {
    fn from(placement: OrderPlacement<String>) -> Self {
        Self {
            order_id: placement.order_id,
            symbol: placement.symbol,
            shares: placement.shares,
            direction: placement.direction,
            placed_at: placement.placed_at,
            extended_hours: placement.extended_hours,
            limit_price: placement.limit_price,
        }
    }
}

impl From<PlacementResponse> for OrderPlacement<String> {
    fn from(placement: PlacementResponse) -> Self {
        Self {
            order_id: placement.order_id,
            symbol: placement.symbol,
            shares: placement.shares,
            direction: placement.direction,
            placed_at: placement.placed_at,
            extended_hours: placement.extended_hours,
            limit_price: placement.limit_price,
        }
    }
}

/// `orders.recover` request: the original market order. Nothing is
/// submitted; the order is only looked up by its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoverOrderRequest {
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub client_order_id: ClientOrderId,
}

impl From<RecoverOrderRequest> for MarketOrder {
    fn from(request: RecoverOrderRequest) -> Self {
        Self {
            symbol: request.symbol,
            shares: request.shares,
            direction: request.direction,
            client_order_id: request.client_order_id,
        }
    }
}

impl From<MarketOrder> for RecoverOrderRequest {
    fn from(order: MarketOrder) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            client_order_id: order.client_order_id,
        }
    }
}

/// `orders.recover`: `None` when Alpaca holds no order under the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverOrderResponse {
    pub order: Option<PlacementResponse>,
}

/// An order found by its client order id. The session terms are `None` when
/// Alpaca omitted them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveredPlacement {
    pub order_id: String,
    pub symbol: Symbol,
    pub shares: Positive<FractionalShares>,
    pub direction: Direction,
    pub placed_at: DateTime<Utc>,
    pub extended_hours: Option<bool>,
    pub limit_price: Option<Positive<Usd>>,
}

impl From<RecoveredOrderPlacement<String>> for RecoveredPlacement {
    fn from(placement: RecoveredOrderPlacement<String>) -> Self {
        Self {
            order_id: placement.order_id,
            symbol: placement.symbol,
            shares: placement.shares,
            direction: placement.direction,
            placed_at: placement.placed_at,
            extended_hours: placement.extended_hours,
            limit_price: placement.limit_price,
        }
    }
}

impl From<RecoveredPlacement> for RecoveredOrderPlacement<String> {
    fn from(placement: RecoveredPlacement) -> Self {
        Self {
            order_id: placement.order_id,
            symbol: placement.symbol,
            shares: placement.shares,
            direction: placement.direction,
            placed_at: placement.placed_at,
            extended_hours: placement.extended_hours,
            limit_price: placement.limit_price,
        }
    }
}

/// `orders.find`: `None` when Alpaca currently reports no order under the
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindOrderResponse {
    pub order: Option<RecoveredPlacement>,
}

/// `orders.get`: the order's lifecycle state, tagged by `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum OrderStateResponse {
    Pending,
    Submitted {
        order_id: ExecutorOrderId,
    },
    PartiallyFilled {
        order_id: ExecutorOrderId,
        shares_filled: FractionalShares,
        avg_price: Option<Usd>,
        partially_filled_at: DateTime<Utc>,
    },
    Filled {
        executed_at: DateTime<Utc>,
        order_id: ExecutorOrderId,
        shares_filled: Positive<FractionalShares>,
        price: Usd,
    },
    Cancelled {
        cancelled_at: DateTime<Utc>,
        order_id: ExecutorOrderId,
        shares_filled: FractionalShares,
        avg_price: Option<Usd>,
    },
    Failed {
        failed_at: DateTime<Utc>,
        error_reason: Option<String>,
        shares_filled: Option<FractionalShares>,
        avg_price: Option<Usd>,
        terminality: OrderFailureTerminality,
    },
}

impl From<OrderState> for OrderStateResponse {
    fn from(state: OrderState) -> Self {
        match state {
            OrderState::Pending => Self::Pending,
            OrderState::Submitted { order_id } => Self::Submitted { order_id },
            OrderState::PartiallyFilled {
                order_id,
                shares_filled,
                avg_price,
                partially_filled_at,
            } => Self::PartiallyFilled {
                order_id,
                shares_filled,
                avg_price,
                partially_filled_at,
            },
            OrderState::Filled {
                executed_at,
                order_id,
                shares_filled,
                price,
            } => Self::Filled {
                executed_at,
                order_id,
                shares_filled,
                price,
            },
            OrderState::Cancelled {
                cancelled_at,
                order_id,
                shares_filled,
                avg_price,
            } => Self::Cancelled {
                cancelled_at,
                order_id,
                shares_filled,
                avg_price,
            },
            OrderState::Failed {
                failed_at,
                error_reason,
                shares_filled,
                avg_price,
                terminality,
            } => Self::Failed {
                failed_at,
                error_reason,
                shares_filled,
                avg_price,
                terminality,
            },
        }
    }
}

impl From<OrderStateResponse> for OrderState {
    fn from(state: OrderStateResponse) -> Self {
        match state {
            OrderStateResponse::Pending => Self::Pending,
            OrderStateResponse::Submitted { order_id } => Self::Submitted { order_id },
            OrderStateResponse::PartiallyFilled {
                order_id,
                shares_filled,
                avg_price,
                partially_filled_at,
            } => Self::PartiallyFilled {
                order_id,
                shares_filled,
                avg_price,
                partially_filled_at,
            },
            OrderStateResponse::Filled {
                executed_at,
                order_id,
                shares_filled,
                price,
            } => Self::Filled {
                executed_at,
                order_id,
                shares_filled,
                price,
            },
            OrderStateResponse::Cancelled {
                cancelled_at,
                order_id,
                shares_filled,
                avg_price,
            } => Self::Cancelled {
                cancelled_at,
                order_id,
                shares_filled,
                avg_price,
            },
            OrderStateResponse::Failed {
                failed_at,
                error_reason,
                shares_filled,
                avg_price,
                terminality,
            } => Self::Failed {
                failed_at,
                error_reason,
                shares_filled,
                avg_price,
                terminality,
            },
        }
    }
}

/// `orders.cancel` request. The order comes from the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelOrderRequest {
    pub reason: Option<String>,
}

/// How Alpaca answered a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Alpaca accepted the cancel; read the order for its final state.
    Requested,
    /// Alpaca does not know the order, so no further fills can occur.
    OrderNotFound,
}

impl From<CancellationOutcome> for CancelOutcome {
    fn from(outcome: CancellationOutcome) -> Self {
        match outcome {
            CancellationOutcome::Requested => Self::Requested,
            CancellationOutcome::OrderNotFound => Self::OrderNotFound,
        }
    }
}

impl From<CancelOutcome> for CancellationOutcome {
    fn from(outcome: CancelOutcome) -> Self {
        match outcome {
            CancelOutcome::Requested => Self::Requested,
            CancelOutcome::OrderNotFound => Self::OrderNotFound,
        }
    }
}

/// `orders.cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelOrderResponse {
    pub outcome: CancelOutcome,
}

/// A USDC/USD conversion, carrying its amount in the unit its direction is
/// denominated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "direction",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Conversion {
    /// Sell this much USDC for USD buying power.
    SellUsdc { quantity: Positive<Usdc> },
    /// Spend this many dollars buying USDC.
    BuyWithUsd { notional: Positive<Usd> },
}

impl From<Conversion> for ConversionOrder {
    fn from(conversion: Conversion) -> Self {
        match conversion {
            Conversion::SellUsdc { quantity } => Self::SellUsdc(quantity),
            Conversion::BuyWithUsd { notional } => Self::BuyWithUsd(notional),
        }
    }
}

impl From<ConversionOrder> for Conversion {
    fn from(order: ConversionOrder) -> Self {
        match order {
            ConversionOrder::SellUsdc(quantity) => Self::SellUsdc { quantity },
            ConversionOrder::BuyWithUsd(notional) => Self::BuyWithUsd { notional },
        }
    }
}

/// `conversions.submit` request. Answers once Alpaca accepts the order; the
/// caller polls `conversions.get` for the fill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversionRequest {
    pub client_order_id: ClientOrderId,
    pub conversion: Conversion,
    pub reason: Option<String>,
}

/// `conversions.submit` and `conversions.get`: a USDC/USD order. Size
/// downstream steps from the filled quantity and price, never from the
/// amount requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionOrderResponse {
    pub id: Uuid,
    pub symbol: String,
    /// USDC requested; absent on an order placed by notional.
    pub quantity: Option<Usdc>,
    /// USD requested; present only on an order placed by notional.
    pub notional: Option<Usd>,
    /// Alpaca's order status, for example `new`, `filled` or `canceled`.
    pub status: String,
    pub filled_average_price: Option<Usd>,
    pub filled_quantity: Option<Usdc>,
    pub created_at: DateTime<Utc>,
}

impl From<CryptoOrderResponse> for ConversionOrderResponse {
    fn from(order: CryptoOrderResponse) -> Self {
        Self {
            id: order.id,
            status: order.status_display().to_string(),
            symbol: order.symbol,
            quantity: order
                .quantity
                .map(|quantity| Usdc::new(quantity.into_normalized())),
            notional: order.notional.map(Usd::new),
            filled_average_price: order.filled_average_price.map(Usd::new),
            filled_quantity: order
                .filled_quantity
                .map(|quantity| Usdc::new(quantity.into_normalized())),
            created_at: order.created_at,
        }
    }
}

/// `conversions.find`: `None` when the order never reached Alpaca.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindConversionResponse {
    pub order: Option<ConversionOrderResponse>,
}
