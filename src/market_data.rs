//! Telemetry-free Alpaca market-data lookups used by liquidity consumers.

use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;
use st0x_finance::{FloatError, NotPositive, Positive, Usd};
use thiserror::Error;

pub use crate::core::Symbol;
use crate::core::{AlpacaAuth, AlpacaClient, AlpacaError, Backpressure, Permanence};
use crate::rate_limit::retry_after_from_response_headers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteFeed {
    DelayedSip,
    Overnight,
}

impl QuoteFeed {
    const fn as_query_value(self) -> &'static str {
        match self {
            Self::DelayedSip => "delayed_sip",
            Self::Overnight => "overnight",
        }
    }
}

/// Validated best bid and ask for a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestQuote {
    bid: Positive<Usd>,
    ask: Positive<Usd>,
}

impl LatestQuote {
    /// Creates a validated, non-crossed quote.
    ///
    /// # Errors
    ///
    /// Returns [`LatestQuoteError::Crossed`] when the bid exceeds the ask.
    pub fn new(bid: Positive<Usd>, ask: Positive<Usd>) -> Result<Self, LatestQuoteError> {
        if ask.inner().lt(&bid.inner())? {
            return Err(LatestQuoteError::Crossed { bid, ask });
        }
        Ok(Self { bid, ask })
    }

    #[must_use]
    pub const fn bid(self) -> Positive<Usd> {
        self.bid
    }

    #[must_use]
    pub const fn ask(self) -> Positive<Usd> {
        self.ask
    }
}

#[derive(Debug, Error)]
pub enum LatestQuoteError {
    #[error("quote comparison failed: {0}")]
    Float(#[from] FloatError),
    #[error("crossed quote: bid {bid} exceeds ask {ask}")]
    Crossed {
        bid: Positive<Usd>,
        ask: Positive<Usd>,
    },
}

/// Indicative overnight quote and its broker timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndicativeQuote {
    pub quote: LatestQuote,
    pub at: DateTime<Utc>,
}

/// Authenticated market-data capability.
#[derive(Clone)]
pub struct AlpacaMarketDataClient {
    client: AlpacaClient,
    base_url: String,
}

impl std::fmt::Debug for AlpacaMarketDataClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlpacaMarketDataClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AlpacaMarketDataClient {
    /// Creates a market-data client using Alpaca's legacy API-key pair.
    ///
    /// # Errors
    ///
    /// Returns an error when the credentials or HTTP client are invalid.
    pub fn new(
        base_url: String,
        api_key: &str,
        api_secret: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AlpacaMarketDataError> {
        Self::with_auth(
            base_url,
            AlpacaAuth::Basic {
                api_key: api_key.to_string(),
                api_secret: api_secret.to_string(),
            },
            "",
            connect_timeout,
            request_timeout,
        )
    }

    /// Creates a market-data client with any supported authentication mode.
    ///
    /// # Errors
    ///
    /// Returns an error when the credentials or HTTP client are invalid.
    pub fn with_auth(
        base_url: String,
        auth: AlpacaAuth,
        token_url: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AlpacaMarketDataError> {
        let client = AlpacaClient::with_auth(
            base_url.clone(),
            String::new(),
            auth,
            token_url,
            connect_timeout,
            request_timeout,
        )?;
        Ok(Self { client, base_url })
    }

    /// Fetches the latest trade price.
    ///
    /// # Errors
    ///
    /// Returns an error for request, response, or price-validation failures.
    pub async fn latest_trade_price(
        &self,
        symbol: &Symbol,
    ) -> Result<Positive<Usd>, AlpacaMarketDataError> {
        fetch_latest_trade_price(&self.client, &self.base_url, symbol).await
    }

    /// Fetches the latest delayed SIP quote.
    ///
    /// # Errors
    ///
    /// Returns an error for request, response, or quote-validation failures.
    pub async fn latest_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<LatestQuote, AlpacaMarketDataError> {
        fetch_quote_and_timestamp(&self.client, &self.base_url, symbol, QuoteFeed::DelayedSip)
            .await
            .map(|(quote, _)| quote)
    }

    /// Fetches an overnight indicative quote with its broker timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error for request, response, timestamp, or quote-validation
    /// failures.
    pub async fn latest_overnight_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<IndicativeQuote, AlpacaMarketDataError> {
        let (quote, at) =
            fetch_quote_and_timestamp(&self.client, &self.base_url, symbol, QuoteFeed::Overnight)
                .await?;
        let at = at.ok_or_else(|| AlpacaMarketDataError::MissingQuoteTimestamp {
            symbol: symbol.clone(),
        })?;
        Ok(IndicativeQuote { quote, at })
    }
}

#[derive(Debug, Error)]
pub enum AlpacaMarketDataError {
    #[error(transparent)]
    Alpaca(#[from] AlpacaError),
    #[error("market-data entitlement failure (status {status_code}): {body}")]
    Entitlement { status_code: u16, body: String },
    #[error("latest trade response for {symbol} did not include a price")]
    MissingPrice { symbol: Symbol },
    #[error("latest trade response for {symbol} returned non-positive price {price}")]
    NonPositivePrice { symbol: Symbol, price: Usd },
    #[error("latest trade price for {symbol} could not be compared with zero: {source}")]
    PriceComparison {
        symbol: Symbol,
        #[source]
        source: FloatError,
    },
    #[error("latest quote response for {symbol} did not include a quote")]
    MissingQuote { symbol: Symbol },
    #[error("latest quote response for {symbol} did not include a bid")]
    MissingBid { symbol: Symbol },
    #[error("latest quote response for {symbol} did not include an ask")]
    MissingAsk { symbol: Symbol },
    #[error("latest quote response for {symbol} did not include a timestamp")]
    MissingQuoteTimestamp { symbol: Symbol },
    #[error("latest quote endpoint returned {returned} when {requested} was requested")]
    SymbolMismatch { requested: Symbol, returned: Symbol },
    #[error("latest quote response for {symbol} returned non-positive bid {bid}")]
    NonPositiveBid { symbol: Symbol, bid: Usd },
    #[error("latest quote bid for {symbol} could not be compared with zero: {source}")]
    BidComparison {
        symbol: Symbol,
        #[source]
        source: FloatError,
    },
    #[error("latest quote response for {symbol} returned non-positive ask {ask}")]
    NonPositiveAsk { symbol: Symbol, ask: Usd },
    #[error("latest quote ask for {symbol} could not be compared with zero: {source}")]
    AskComparison {
        symbol: Symbol,
        #[source]
        source: FloatError,
    },
    #[error("latest quote response for {symbol} is invalid")]
    InvalidQuote {
        symbol: Symbol,
        #[source]
        source: LatestQuoteError,
    },
}

impl AlpacaMarketDataError {
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match self {
            Self::Alpaca(error) => error.backpressure(),
            _ => None,
        }
    }

    #[must_use]
    pub fn permanence(&self) -> Permanence {
        match self {
            Self::Alpaca(error) => error.permanence(),
            Self::Entitlement { .. }
            | Self::MissingPrice { .. }
            | Self::NonPositivePrice { .. }
            | Self::PriceComparison { .. } => Permanence::Permanent,
            Self::MissingQuote { .. }
            | Self::MissingBid { .. }
            | Self::MissingAsk { .. }
            | Self::MissingQuoteTimestamp { .. }
            | Self::SymbolMismatch { .. }
            | Self::NonPositiveBid { .. }
            | Self::BidComparison { .. }
            | Self::NonPositiveAsk { .. }
            | Self::AskComparison { .. }
            | Self::InvalidQuote { .. } => Permanence::Transient,
        }
    }
}

#[derive(Debug, Deserialize)]
struct LatestTradeEnvelope {
    trade: Option<LatestTrade>,
}

#[derive(Debug, Deserialize)]
struct LatestTrade {
    #[serde(rename = "p")]
    price: Usd,
}

#[derive(Debug, Deserialize)]
struct LatestQuoteEnvelope {
    symbol: Symbol,
    quote: Option<LatestQuotePayload>,
}

#[derive(Debug, Deserialize)]
struct LatestQuotePayload {
    #[serde(rename = "bp", default)]
    bid: Option<Usd>,
    #[serde(rename = "ap", default)]
    ask: Option<Usd>,
    #[serde(rename = "t", default)]
    at: Option<DateTime<Utc>>,
}

async fn get_market_data_bytes(request: RequestBuilder) -> Result<Vec<u8>, AlpacaMarketDataError> {
    let response = request.send().await.map_err(AlpacaError::from)?;
    let status = response.status();
    let retry_after = retry_after_from_response_headers(response.headers());
    let bytes = response.bytes().await.map_err(AlpacaError::from)?;
    let body = String::from_utf8_lossy(&bytes).into_owned();

    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(AlpacaMarketDataError::Entitlement {
            status_code: status.as_u16(),
            body,
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AlpacaError::RateLimited { body, retry_after }.into());
    }
    if !status.is_success() {
        return Err(AlpacaError::Api {
            status_code: status.as_u16(),
            body,
        }
        .into());
    }

    Ok(bytes.into())
}

async fn fetch_latest_trade_price(
    client: &AlpacaClient,
    base_url: &str,
    symbol: &Symbol,
) -> Result<Positive<Usd>, AlpacaMarketDataError> {
    let url = format!("{base_url}/v2/stocks/{symbol}/trades/latest");
    let bytes = get_market_data_bytes(client.market_data_get(&url).await?).await?;
    let response: LatestTradeEnvelope =
        serde_json::from_slice(&bytes).map_err(|source| AlpacaError::Parse {
            body: String::from_utf8_lossy(&bytes).into_owned(),
            source,
        })?;
    let price = response
        .trade
        .ok_or_else(|| AlpacaMarketDataError::MissingPrice {
            symbol: symbol.clone(),
        })?
        .price;
    Positive::new(price).map_err(|error| match error {
        NotPositive::Constraint { value } => AlpacaMarketDataError::NonPositivePrice {
            symbol: symbol.clone(),
            price: value,
        },
        NotPositive::Comparison { source, .. } => AlpacaMarketDataError::PriceComparison {
            symbol: symbol.clone(),
            source,
        },
    })
}

async fn fetch_quote_and_timestamp(
    client: &AlpacaClient,
    base_url: &str,
    symbol: &Symbol,
    feed: QuoteFeed,
) -> Result<(LatestQuote, Option<DateTime<Utc>>), AlpacaMarketDataError> {
    let url = format!("{base_url}/v2/stocks/{symbol}/quotes/latest");
    let request = client
        .market_data_get(&url)
        .await?
        .query(&[("feed", feed.as_query_value())]);
    let bytes = get_market_data_bytes(request).await?;
    let response: LatestQuoteEnvelope =
        serde_json::from_slice(&bytes).map_err(|source| AlpacaError::Parse {
            body: String::from_utf8_lossy(&bytes).into_owned(),
            source,
        })?;
    if response.symbol != *symbol {
        return Err(AlpacaMarketDataError::SymbolMismatch {
            requested: symbol.clone(),
            returned: response.symbol,
        });
    }
    let quote = response
        .quote
        .ok_or_else(|| AlpacaMarketDataError::MissingQuote {
            symbol: symbol.clone(),
        })?;
    let bid = quote.bid.ok_or_else(|| AlpacaMarketDataError::MissingBid {
        symbol: symbol.clone(),
    })?;
    let ask = quote.ask.ok_or_else(|| AlpacaMarketDataError::MissingAsk {
        symbol: symbol.clone(),
    })?;
    let bid = Positive::new(bid).map_err(|error| match error {
        NotPositive::Constraint { value } => AlpacaMarketDataError::NonPositiveBid {
            symbol: symbol.clone(),
            bid: value,
        },
        NotPositive::Comparison { source, .. } => AlpacaMarketDataError::BidComparison {
            symbol: symbol.clone(),
            source,
        },
    })?;
    let ask = Positive::new(ask).map_err(|error| match error {
        NotPositive::Constraint { value } => AlpacaMarketDataError::NonPositiveAsk {
            symbol: symbol.clone(),
            ask: value,
        },
        NotPositive::Comparison { source, .. } => AlpacaMarketDataError::AskComparison {
            symbol: symbol.clone(),
            source,
        },
    })?;
    let quote_value =
        LatestQuote::new(bid, ask).map_err(|source| AlpacaMarketDataError::InvalidQuote {
            symbol: symbol.clone(),
            source,
        })?;
    Ok((quote_value, quote.at))
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;

    fn client(server: &MockServer) -> AlpacaMarketDataClient {
        AlpacaMarketDataClient::new(
            server.base_url(),
            "key",
            "secret",
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    fn symbol() -> Symbol {
        Symbol::new("AAPL").unwrap()
    }

    #[tokio::test]
    async fn fetches_delayed_quote() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v2/stocks/AAPL/quotes/latest")
                .query_param("feed", "delayed_sip");
            then.status(200).json_body(json!({
                "symbol": "AAPL",
                "quote": {"bp": "100", "ap": "101"}
            }));
        });
        let quote = client(&server).latest_quote(&symbol()).await.unwrap();
        mock.assert();
        assert_eq!(quote.bid().inner().to_string(), "100");
        assert_eq!(quote.ask().inner().to_string(), "101");
    }

    #[tokio::test]
    async fn overnight_quote_requires_and_preserves_timestamp() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v2/stocks/AAPL/quotes/latest")
                .query_param("feed", "overnight");
            then.status(200).json_body(json!({
                "symbol": "AAPL",
                "quote": {"bp": "100", "ap": "101", "t": "2026-09-11T01:02:03Z"}
            }));
        });
        let quote = client(&server)
            .latest_overnight_quote(&symbol())
            .await
            .unwrap();
        mock.assert();
        assert_eq!(quote.at.to_rfc3339(), "2026-09-11T01:02:03+00:00");
    }

    #[tokio::test]
    async fn rate_limit_preserves_retry_after() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/quotes/latest");
            then.status(429)
                .header("retry-after", "30")
                .body("slow down");
        });
        let error = client(&server).latest_quote(&symbol()).await.unwrap_err();
        assert_eq!(
            error.backpressure(),
            Some(Backpressure {
                retry_after: Some(Duration::from_secs(30))
            })
        );
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_rejects_zero_price() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200).json_body(json!({"trade": {"p": "0"}}));
        });

        let error = client(&server)
            .latest_trade_price(&symbol())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::NonPositivePrice { price, .. } if price.to_string() == "0"
        ));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_rejects_negative_price() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200).json_body(json!({"trade": {"p": "-5"}}));
        });

        let error = client(&server)
            .latest_trade_price(&symbol())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::NonPositivePrice { price, .. } if price.to_string() == "-5"
        ));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_returns_positive_price() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .json_body(json!({"trade": {"p": "123.45"}}));
        });

        let price = client(&server).latest_trade_price(&symbol()).await.unwrap();

        assert_eq!(price.inner().to_string(), "123.45");
    }

    #[test]
    fn market_data_client_debug_redacts_credentials() {
        let api_key = "debug-test-api-key";
        let api_secret = "debug-test-api-secret";
        let client = AlpacaMarketDataClient::new(
            "https://data.example.com".to_string(),
            api_key,
            api_secret,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();

        let debug = format!("{client:?}");
        assert!(!debug.contains(api_key));
        assert!(!debug.contains(api_secret));
        assert!(debug.contains("https://data.example.com"));
    }

    #[tokio::test]
    async fn market_data_client_returns_positive_price_without_exposing_transport() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .json_body(json!({"trade": {"p": "123.45"}}));
        });

        let price = client(&server).latest_trade_price(&symbol()).await.unwrap();

        assert_eq!(price.inner().to_string(), "123.45");
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_accepts_numeric_price() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200).json_body(json!({"trade": {"p": 123.45}}));
        });

        let price = client(&server).latest_trade_price(&symbol()).await.unwrap();

        assert_eq!(price.inner().to_string(), "123.45");
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_reports_missing_trade() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200).json_body(json!({"trade": null}));
        });

        let error = client(&server)
            .latest_trade_price(&symbol())
            .await
            .unwrap_err();

        assert!(matches!(error, AlpacaMarketDataError::MissingPrice { .. }));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_maps_api_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(503).body("unavailable");
        });

        let error = client(&server)
            .latest_trade_price(&symbol())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::Alpaca(AlpacaError::Api {
                status_code: 503,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn market_data_http_client_sends_apca_headers() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v2/stocks/AAPL/trades/latest")
                .header("APCA-API-KEY-ID", "key")
                .header("APCA-API-SECRET-KEY", "secret");
            then.status(200).json_body(json!({"trade": {"p": "1.5"}}));
        });

        let price = client(&server).latest_trade_price(&symbol()).await.unwrap();

        assert_eq!(price.inner().to_string(), "1.5");
        mock.assert();
    }
}
