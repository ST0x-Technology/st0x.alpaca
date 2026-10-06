//! `market.*` and `assets.*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{
    AssetDetails, AssetStatus, IndicativeQuote, LatestQuote, LatestQuoteError, MarketSession,
    MarketSessionStatus, PostCloseGap, PreparedShares,
};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Usd};

/// `market.is_open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsOpenResponse {
    pub open: bool,
}

/// `market.session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    pub session: MarketSession,
}

/// Closure after the current extended session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseGap {
    OrdinaryOvernight,
    MultiDayClosure,
    Unknown,
    Unavailable,
}

impl From<PostCloseGap> for CloseGap {
    fn from(gap: PostCloseGap) -> Self {
        match gap {
            PostCloseGap::OrdinaryOvernight => Self::OrdinaryOvernight,
            PostCloseGap::MultiDayClosure => Self::MultiDayClosure,
            PostCloseGap::Unknown => Self::Unknown,
            PostCloseGap::Unavailable => Self::Unavailable,
        }
    }
}

impl From<CloseGap> for PostCloseGap {
    fn from(gap: CloseGap) -> Self {
        match gap {
            CloseGap::OrdinaryOvernight => Self::OrdinaryOvernight,
            CloseGap::MultiDayClosure => Self::MultiDayClosure,
            CloseGap::Unknown => Self::Unknown,
            CloseGap::Unavailable => Self::Unavailable,
        }
    }
}

/// `market.session_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusResponse {
    pub session: MarketSession,
    pub session_opens_at: Option<DateTime<Utc>>,
    pub regular_session_closes_at: Option<DateTime<Utc>>,
    pub extended_session_closes_at: Option<DateTime<Utc>>,
    pub post_close_gap: CloseGap,
}

impl From<MarketSessionStatus> for SessionStatusResponse {
    fn from(status: MarketSessionStatus) -> Self {
        Self {
            session: status.session,
            session_opens_at: status.session_opens_at,
            regular_session_closes_at: status.regular_session_closes_at,
            extended_session_closes_at: status.extended_session_closes_at,
            post_close_gap: status.post_close_gap.into(),
        }
    }
}

impl From<SessionStatusResponse> for MarketSessionStatus {
    fn from(status: SessionStatusResponse) -> Self {
        Self {
            session: status.session,
            session_opens_at: status.session_opens_at,
            regular_session_closes_at: status.regular_session_closes_at,
            extended_session_closes_at: status.extended_session_closes_at,
            post_close_gap: status.post_close_gap.into(),
        }
    }
}

/// `market.latest_trade`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestTradeResponse {
    pub price: Positive<Usd>,
}

/// `market.latest_quote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub bid: Positive<Usd>,
    pub ask: Positive<Usd>,
}

impl From<LatestQuote> for QuoteResponse {
    fn from(quote: LatestQuote) -> Self {
        Self {
            bid: quote.bid(),
            ask: quote.ask(),
        }
    }
}

impl TryFrom<QuoteResponse> for LatestQuote {
    type Error = LatestQuoteError;

    /// Revalidates the quote as the direct path does: a crossed quote is
    /// refused.
    fn try_from(quote: QuoteResponse) -> Result<Self, Self::Error> {
        Self::new(quote.bid, quote.ask)
    }
}

/// `market.latest_overnight_quote`: indicative, with its broker timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvernightQuoteResponse {
    pub bid: Positive<Usd>,
    pub ask: Positive<Usd>,
    pub at: DateTime<Utc>,
}

impl From<IndicativeQuote> for OvernightQuoteResponse {
    fn from(quote: IndicativeQuote) -> Self {
        Self {
            bid: quote.quote.bid(),
            ask: quote.quote.ask(),
            at: quote.at,
        }
    }
}

impl TryFrom<OvernightQuoteResponse> for IndicativeQuote {
    type Error = LatestQuoteError;

    /// Revalidates the quote as the direct path does: a crossed quote is
    /// refused.
    fn try_from(quote: OvernightQuoteResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            quote: LatestQuote::new(quote.bid, quote.ask)?,
            at: quote.at,
        })
    }
}

/// Alpaca asset status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetState {
    Active,
    Inactive,
}

impl From<AssetStatus> for AssetState {
    fn from(status: AssetStatus) -> Self {
        match status {
            AssetStatus::Active => Self::Active,
            AssetStatus::Inactive => Self::Inactive,
        }
    }
}

impl From<AssetState> for AssetStatus {
    fn from(state: AssetState) -> Self {
        match state {
            AssetState::Active => Self::Active,
            AssetState::Inactive => Self::Inactive,
        }
    }
}

/// `assets.get`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetResponse {
    pub status: AssetState,
    pub tradable: bool,
    pub fractionable: Option<bool>,
    pub fractional_eh_enabled: Option<bool>,
    pub overnight_tradable: Option<bool>,
    pub overnight_halted: Option<bool>,
}

impl From<AssetDetails> for AssetResponse {
    fn from(asset: AssetDetails) -> Self {
        Self {
            status: asset.status.into(),
            tradable: asset.tradable,
            fractionable: asset.fractionable,
            fractional_eh_enabled: asset.fractional_eh_enabled,
            overnight_tradable: asset.overnight_tradable,
            overnight_halted: asset.overnight_halted,
        }
    }
}

impl From<AssetResponse> for AssetDetails {
    fn from(asset: AssetResponse) -> Self {
        Self {
            status: asset.status.into(),
            tradable: asset.tradable,
            fractionable: asset.fractionable,
            fractional_eh_enabled: asset.fractional_eh_enabled,
            overnight_tradable: asset.overnight_tradable,
            overnight_halted: asset.overnight_halted,
        }
    }
}

/// `assets.counter_trade_shares` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CounterTradeSharesRequest {
    pub shares: Positive<FractionalShares>,
    pub extended_hours: bool,
}

/// `assets.counter_trade_shares`: `shares` is `None` when a non fractionable
/// quantity truncates below one share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterTradeSharesResponse {
    pub shares: Option<Positive<FractionalShares>>,
    pub fractional_orders_supported: bool,
    pub quantity_decimals: u8,
}

impl From<PreparedShares> for CounterTradeSharesResponse {
    fn from(prepared: PreparedShares) -> Self {
        Self {
            shares: prepared.shares,
            fractional_orders_supported: prepared.fractional_orders_supported,
            quantity_decimals: prepared.quantity_decimals,
        }
    }
}

impl From<CounterTradeSharesResponse> for PreparedShares {
    fn from(prepared: CounterTradeSharesResponse) -> Self {
        Self {
            shares: prepared.shares,
            fractional_orders_supported: prepared.fractional_orders_supported,
            quantity_decimals: prepared.quantity_decimals,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::through_wire;
    use super::*;

    #[test]
    fn a_crossed_quote_is_refused_as_the_direct_path_refuses_it() {
        let crossed: QuoteResponse =
            serde_json::from_value(json!({ "bid": "101", "ask": "100" })).unwrap();
        assert!(matches!(
            LatestQuote::try_from(crossed),
            Err(LatestQuoteError::Crossed { .. })
        ));

        let overnight: OvernightQuoteResponse = serde_json::from_value(
            json!({ "bid": "101", "ask": "100", "at": "2026-10-06T02:00:00Z" }),
        )
        .unwrap();
        assert!(matches!(
            IndicativeQuote::try_from(overnight),
            Err(LatestQuoteError::Crossed { .. })
        ));
    }

    #[test]
    fn quotes_come_back_from_the_wire_unchanged() {
        let quote: QuoteResponse =
            serde_json::from_value(json!({ "bid": "100.01", "ask": "100.02" })).unwrap();
        let latest = LatestQuote::try_from(through_wire(&quote)).unwrap();
        assert_eq!(QuoteResponse::from(latest), quote);

        let overnight: OvernightQuoteResponse = serde_json::from_value(
            json!({ "bid": "100", "ask": "100", "at": "2026-10-06T02:00:00Z" }),
        )
        .unwrap();
        let indicative = IndicativeQuote::try_from(through_wire(&overnight)).unwrap();
        assert_eq!(OvernightQuoteResponse::from(indicative), overnight);
    }

    #[test]
    fn session_status_and_prepared_shares_come_back_unchanged() {
        for gap in [
            "ordinary_overnight",
            "multi_day_closure",
            "unknown",
            "unavailable",
        ] {
            let status: SessionStatusResponse = serde_json::from_value(json!({
                "session": "Extended",
                "sessionOpensAt": "2026-10-06T08:00:00Z",
                "regularSessionClosesAt": "2026-10-06T20:00:00Z",
                "extendedSessionClosesAt": null,
                "postCloseGap": gap
            }))
            .unwrap();
            let relayed = MarketSessionStatus::from(through_wire(&status));
            assert_eq!(SessionStatusResponse::from(relayed), status);
        }

        for shares in [json!("1.5"), json!(null)] {
            let prepared: CounterTradeSharesResponse = serde_json::from_value(json!({
                "shares": shares,
                "fractionalOrdersSupported": false,
                "quantityDecimals": 0
            }))
            .unwrap();
            let relayed = PreparedShares::from(through_wire(&prepared));
            assert_eq!(CounterTradeSharesResponse::from(relayed), prepared);
        }
    }

    #[test]
    fn asset_details_come_back_unchanged() {
        let asset: AssetResponse = serde_json::from_value(json!({
            "status": "inactive",
            "tradable": true,
            "fractionable": false,
            "fractionalEhEnabled": true,
            "overnightTradable": null,
            "overnightHalted": false
        }))
        .unwrap();
        let relayed = AssetDetails::from(through_wire(&asset));
        assert_eq!(relayed.status, AssetStatus::Inactive);
        assert_eq!(AssetResponse::from(relayed), asset);
    }
}
