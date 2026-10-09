//! `market.*` and `assets.*`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{IndicativeQuote, LatestQuote, LatestQuoteError, MarketSession};
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

/// `assets.counter_trade_shares` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CounterTradeSharesRequest {
    pub shares: Positive<FractionalShares>,
    pub extended_hours: bool,
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
}
