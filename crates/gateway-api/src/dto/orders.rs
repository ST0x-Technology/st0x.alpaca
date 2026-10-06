//! `orders.*` and `conversions.*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{
    AlpacaAmount, AlpacaBrokerApiError, AlpacaLimitOrder, AlpacaLimitPrice, BrokerOrderStatus,
    CancellationOutcome, ClientOrderId, ConversionOrder, CryptoOrderResponse, Direction,
    ExecutorOrderId, LimitOrder, MarketOrder, OrderFailureTerminality, OrderPlacement, OrderState,
    RecoveredOrderPlacement,
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
    #[serde(deserialize_with = "super::symbol")]
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

impl MarketOrderRequest {
    /// The request for `order`. `reason` is required on a human tier, which
    /// refuses a mutation without one.
    #[must_use]
    pub fn new(order: MarketOrder, reason: Option<String>) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            client_order_id: order.client_order_id,
            reason,
        }
    }
}

/// `orders.place_limit` request: the automated limit order, whose quantity
/// the gateway truncates to the asset's precision for the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitOrderRequest {
    #[serde(deserialize_with = "super::symbol")]
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

impl LimitOrderRequest {
    /// The request for `order`. `reason` is required on a human tier, which
    /// refuses a mutation without one.
    #[must_use]
    pub fn new(order: LimitOrder, reason: Option<String>) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            limit_price: order.limit_price,
            extended_hours: order.extended_hours,
            client_order_id: order.client_order_id,
            reason,
        }
    }
}

/// `orders.place_exact_limit` request: an operator limit order placed with
/// exactly the quantity given, without the fractionability truncation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExactLimitOrderRequest {
    #[serde(deserialize_with = "super::symbol")]
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

impl ExactLimitOrderRequest {
    /// The request for `order`. The operation is served only on the write
    /// tier, which refuses it without a non blank `reason`.
    #[must_use]
    pub fn new(order: AlpacaLimitOrder, reason: impl Into<String>) -> Self {
        Self {
            symbol: order.symbol,
            shares: order.shares,
            direction: order.direction,
            limit_price: order.limit_price.into_inner(),
            extended_hours: order.extended_hours,
            client_order_id: order.client_order_id,
            reason: Some(reason.into()),
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
    #[serde(deserialize_with = "super::symbol")]
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

/// `conversions.submit` and `conversions.get`: a USDC/USD order exactly as
/// Alpaca reported it, so the caller rebuilds the library's
/// `CryptoOrderResponse` and classifies it with the library's own rule. Size
/// downstream steps from the filled quantity and price, never from the
/// amount requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionOrderResponse {
    pub id: Uuid,
    pub symbol: String,
    /// USDC requested, as Alpaca reported it (up to nine decimals); absent
    /// on an order placed by notional.
    pub quantity: Option<AlpacaAmount>,
    /// USD requested; present only on an order placed by notional.
    pub notional: Option<Usd>,
    /// Alpaca's order status. An unknown status fails to decode, as it does
    /// on the direct path.
    pub status: BrokerOrderStatus,
    pub filled_average_price: Option<Usd>,
    /// USDC filled, as Alpaca reported it (up to nine decimals).
    pub filled_quantity: Option<AlpacaAmount>,
    pub created_at: DateTime<Utc>,
}

impl From<CryptoOrderResponse> for ConversionOrderResponse {
    fn from(order: CryptoOrderResponse) -> Self {
        Self {
            id: order.id,
            symbol: order.symbol,
            quantity: order.quantity,
            notional: order.notional.map(Usd::new),
            status: order.status,
            filled_average_price: order.filled_average_price.map(Usd::new),
            filled_quantity: order.filled_quantity,
            created_at: order.created_at,
        }
    }
}

impl From<ConversionOrderResponse> for CryptoOrderResponse {
    fn from(order: ConversionOrderResponse) -> Self {
        Self {
            id: order.id,
            symbol: order.symbol,
            quantity: order.quantity,
            notional: order.notional.map(Usd::inner),
            status: order.status,
            filled_average_price: order.filled_average_price.map(Usd::inner),
            filled_quantity: order.filled_quantity,
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use st0x_alpaca::broker::CryptoOrderOutcome;

    use super::super::through_wire;
    use super::*;

    /// A notional conversion as Alpaca reports it, filled with nine
    /// decimals: more than USDC's six.
    fn alpaca_conversion(status: &str) -> CryptoOrderResponse {
        serde_json::from_value(json!({
            "id": "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f",
            "symbol": "USDCUSD",
            "qty": null,
            "notional": "100.25",
            "status": status,
            "filled_avg_price": "0.9998",
            "filled_qty": "100.270054011",
            "created_at": "2026-10-06T10:15:29Z"
        }))
        .unwrap()
    }

    #[test]
    fn a_conversion_comes_back_from_the_wire_as_alpaca_reported_it() {
        for status in ["filled", "partially_filled", "canceled", "done_for_day"] {
            let direct = alpaca_conversion(status);
            let relayed = CryptoOrderResponse::from(through_wire(&ConversionOrderResponse::from(
                direct.clone(),
            )));

            assert_eq!(relayed.classify(), direct.classify(), "{status}");
            assert_eq!(relayed.status(), direct.status(), "{status}");
            assert_eq!(relayed.id, direct.id);
            assert_eq!(relayed.created_at, direct.created_at);
            // The raw amount survives, not only the six decimal one: cash
            // valuation reads the raw amount.
            let price = direct.filled_average_price.unwrap();
            assert_eq!(
                relayed
                    .filled_quantity
                    .unwrap()
                    .cash_value_at(price)
                    .unwrap(),
                direct
                    .filled_quantity
                    .unwrap()
                    .cash_value_at(price)
                    .unwrap()
            );
            assert_eq!(
                serde_json::to_value(relayed.filled_quantity).unwrap(),
                json!("100.270054011")
            );
            assert!(
                relayed
                    .notional
                    .unwrap()
                    .eq(direct.notional.unwrap())
                    .unwrap()
            );
            assert!(relayed.filled_average_price.unwrap().eq(price).unwrap());
        }
        assert_eq!(
            CryptoOrderResponse::from(through_wire(&ConversionOrderResponse::from(
                alpaca_conversion("filled")
            )))
            .classify(),
            CryptoOrderOutcome::Filled
        );
    }

    #[test]
    fn a_conversion_status_the_library_does_not_know_fails_to_decode() {
        let mut wire =
            serde_json::to_value(ConversionOrderResponse::from(alpaca_conversion("filled")))
                .unwrap();
        wire["status"] = json!("held_for_review");

        assert!(serde_json::from_value::<ConversionOrderResponse>(wire).is_err());
    }

    #[test]
    fn every_order_state_comes_back_from_the_wire_unchanged() {
        let order_id = ExecutorOrderId::new("7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f");
        let at: DateTime<Utc> = "2026-10-06T10:15:29Z".parse().unwrap();
        let shares: FractionalShares = serde_json::from_value(json!("2.5")).unwrap();
        let price: Usd = serde_json::from_value(json!("101.25")).unwrap();
        for state in [
            OrderState::Pending,
            OrderState::Submitted {
                order_id: order_id.clone(),
            },
            OrderState::PartiallyFilled {
                order_id: order_id.clone(),
                shares_filled: shares,
                avg_price: Some(price),
                partially_filled_at: at,
            },
            OrderState::Filled {
                executed_at: at,
                order_id: order_id.clone(),
                shares_filled: Positive::new(shares).unwrap(),
                price,
            },
            OrderState::Cancelled {
                cancelled_at: at,
                order_id: order_id.clone(),
                shares_filled: shares,
                avg_price: None,
            },
            OrderState::Failed {
                failed_at: at,
                error_reason: Some("rejected".to_string()),
                shares_filled: Some(shares),
                avg_price: Some(price),
                terminality: OrderFailureTerminality::Terminal,
            },
        ] {
            let relayed = OrderState::from(through_wire(&OrderStateResponse::from(state.clone())));
            assert_eq!(relayed, state);
        }
    }

    fn placement_wire() -> serde_json::Value {
        json!({
            "orderId": "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f",
            "symbol": "BRK.B",
            "shares": "3.5",
            "direction": "sell",
            "placedAt": "2026-10-06T10:15:29Z",
            "extendedHours": true,
            "limitPrice": "412.5"
        })
    }

    #[test]
    fn placements_and_recovered_placements_come_back_unchanged() {
        let placement: PlacementResponse = serde_json::from_value(placement_wire()).unwrap();
        let relayed = PlacementResponse::from(OrderPlacement::from(through_wire(&placement)));
        assert_eq!(relayed, placement);

        for extended_hours in [Some(false), None] {
            let mut wire = placement_wire();
            wire["extendedHours"] = json!(extended_hours);
            wire["limitPrice"] = json!(null);
            let recovered: RecoveredPlacement = serde_json::from_value(wire).unwrap();
            let relayed =
                RecoveredPlacement::from(RecoveredOrderPlacement::from(through_wire(&recovered)));
            assert_eq!(relayed, recovered);
        }
    }

    #[test]
    fn order_requests_carry_every_order_term() {
        let market: MarketOrderRequest = serde_json::from_value(json!({
            "symbol": "AAPL",
            "shares": "7",
            "direction": "buy",
            "clientOrderId": "66666666-6666-4666-8666-666666666666",
            "reason": "rebalance"
        }))
        .unwrap();
        assert_eq!(
            MarketOrderRequest::new(
                MarketOrder::from(through_wire(&market)),
                Some("rebalance".to_string())
            ),
            market
        );
        let recover = RecoverOrderRequest::from(MarketOrder::from(market.clone()));
        assert_eq!(
            RecoverOrderRequest::from(MarketOrder::from(through_wire(&recover))),
            recover
        );

        let limit: LimitOrderRequest = serde_json::from_value(json!({
            "symbol": "AAPL",
            "shares": "7",
            "direction": "sell",
            "limitPrice": "190.5",
            "extendedHours": true,
            "clientOrderId": "66666666-6666-4666-8666-666666666666"
        }))
        .unwrap();
        assert_eq!(
            LimitOrderRequest::new(LimitOrder::from(through_wire(&limit)), None),
            limit
        );

        let exact: ExactLimitOrderRequest = serde_json::from_value(json!({
            "symbol": "AAPL",
            "shares": "0.123456789",
            "direction": "buy",
            "limitPrice": "190.55",
            "extendedHours": false,
            "clientOrderId": "cli-66666666-6666-4666-8666-666666666666",
            "reason": "manual hedge"
        }))
        .unwrap();
        let order = AlpacaLimitOrder::try_from(through_wire(&exact)).unwrap();
        assert_eq!(ExactLimitOrderRequest::new(order, "manual hedge"), exact);
    }

    #[test]
    fn conversions_and_cancel_outcomes_keep_their_meaning() {
        for conversion in [
            json!({ "direction": "sell_usdc", "quantity": "250.5" }),
            json!({ "direction": "buy_with_usd", "notional": "99.99" }),
        ] {
            let conversion: Conversion = serde_json::from_value(conversion).unwrap();
            assert_eq!(
                Conversion::from(ConversionOrder::from(through_wire(&conversion))),
                conversion
            );
        }
        for outcome in [CancelOutcome::Requested, CancelOutcome::OrderNotFound] {
            assert_eq!(
                CancelOutcome::from(CancellationOutcome::from(through_wire(&outcome))),
                outcome
            );
        }
    }
}
