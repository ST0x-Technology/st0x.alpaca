//! `orders.*` and `conversions.*`.

use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{
    AlpacaBrokerApiError, AlpacaLimitOrder, AlpacaLimitPrice, CancellationOutcome, ClientOrderId,
    ConversionOrder, CryptoOrderResponse, Direction, LimitOrder, MarketOrder, OrderPlacement,
    RecoveredOrderPlacement,
};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usd, Usdc};

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

/// `orders.place_limit` and `orders.place_exact_limit` request. The first
/// truncates the quantity to the asset's precision for the session; the
/// second places exactly the quantity given.
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

impl TryFrom<LimitOrderRequest> for AlpacaLimitOrder {
    type Error = AlpacaBrokerApiError;

    /// Fails when the limit price has more decimals than Alpaca accepts.
    fn try_from(request: LimitOrderRequest) -> Result<Self, Self::Error> {
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

impl LimitOrderRequest {
    /// The `orders.place_limit` request for `order`. `reason` is required on
    /// a human tier, which refuses a mutation without one.
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

    /// The `orders.place_exact_limit` request for `order`. The operation is
    /// served only on the write tier, which refuses it without a non blank
    /// `reason`.
    #[must_use]
    pub fn exact(order: AlpacaLimitOrder, reason: impl Into<String>) -> Self {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverOrderResponse {
    pub order: Option<OrderPlacement<String>>,
}

/// `orders.find`: `None` when Alpaca currently reports no order under the
/// key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindOrderResponse {
    pub order: Option<RecoveredOrderPlacement<String>>,
}

/// `orders.cancel` request. The order comes from the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelOrderRequest {
    pub reason: Option<String>,
}

/// `orders.cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelOrderResponse {
    pub outcome: CancellationOutcome,
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

/// `conversions.find`: `None` when the order never reached Alpaca.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindConversionResponse {
    pub order: Option<CryptoOrderResponse>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::through_wire;
    use super::*;

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

        let exact: LimitOrderRequest = serde_json::from_value(json!({
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
        assert_eq!(LimitOrderRequest::exact(order, "manual hedge"), exact);
    }

    #[test]
    fn conversions_keep_their_meaning() {
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
    }
}
