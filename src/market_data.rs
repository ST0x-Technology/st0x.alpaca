//! Alpaca market-data lookups (latest stock trade price).
//!
//! The market data API lives on a different host than the Broker API and
//! authenticates with the `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` headers
//! only (no HTTP Basic auth), so this surface takes a plain
//! [`reqwest::Client`] carrying those default headers -- build one with
//! [`market_data_http_client`] -- instead of the Broker-API transport in
//! [`crate::core`].
//!
//! Prices use the shared [`st0x_finance::Usd`] domain type while preserving
//! Alpaca's JSON number-or-string encoding. This module is telemetry-free:
//! consumers wrap calls with their own instrumentation.

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use st0x_finance::{HasZero, Usd};
use std::time::Duration;
use thiserror::Error;

use crate::core::AlpacaError;
pub use crate::core::Symbol;

/// Errors from the market-data surface.
///
/// Transport, parse, and API-status failures are carried by the shared
/// [`AlpacaError`] taxonomy; the remaining variants are market-data
/// invariants detected client-side.
#[derive(Debug, Error)]
pub enum AlpacaMarketDataError {
    #[error(transparent)]
    Alpaca(#[from] AlpacaError),

    #[error("invalid Alpaca credential header value")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    #[error("latest trade response for {symbol} did not include a price")]
    MissingPrice { symbol: Symbol },

    #[error("latest trade response for {symbol} returned non-positive price {price}")]
    NonPositivePrice { symbol: Symbol, price: Usd },
}

/// Builds an HTTP client with the `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY`
/// default headers the market data API expects on every request.
///
/// # Errors
///
/// Returns [`AlpacaMarketDataError::InvalidHeader`] if a credential is not
/// a valid header value, or [`AlpacaError::Reqwest`] if the underlying HTTP
/// client cannot be constructed.
pub fn market_data_http_client(
    api_key: &str,
    api_secret: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<reqwest::Client, AlpacaMarketDataError> {
    let headers = HeaderMap::from_iter([
        (
            HeaderName::from_static("apca-api-key-id"),
            HeaderValue::from_str(api_key)?,
        ),
        (
            HeaderName::from_static("apca-api-secret-key"),
            HeaderValue::from_str(api_secret)?,
        ),
        (CONTENT_TYPE, HeaderValue::from_static("application/json")),
    ]);

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .map_err(AlpacaError::from)?;

    Ok(client)
}

/// Fetches the latest trade price for a stock symbol.
///
/// `client` must carry the market-data auth headers (see
/// [`market_data_http_client`]); `market_data_base_url` is e.g.
/// `https://data.alpaca.markets` (or the sandbox equivalent).
///
/// # Errors
///
/// Returns an error if the request fails, the response cannot be parsed,
/// the response contains no trade, or the reported price is not positive.
pub async fn fetch_latest_trade_price(
    client: &reqwest::Client,
    market_data_base_url: &str,
    symbol: &Symbol,
) -> Result<Usd, AlpacaMarketDataError> {
    let url = format!("{market_data_base_url}/v2/stocks/{symbol}/trades/latest");

    let response = client.get(url).send().await.map_err(AlpacaError::from)?;
    let status = response.status();
    // Read raw bytes and parse successful responses with `from_slice` so
    // invalid UTF-8 fails fast rather than being silently replaced by lossy
    // decoding before parse. Lossy decoding is fine for the error body only.
    let bytes = response.bytes().await.map_err(AlpacaError::from)?;

    if !status.is_success() {
        return Err(AlpacaError::Api {
            status_code: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
        .into());
    }

    let envelope: LatestTradeEnvelope =
        serde_json::from_slice(&bytes).map_err(|source| AlpacaError::Parse {
            body: String::from_utf8_lossy(&bytes).into_owned(),
            source,
        })?;

    let Some(trade) = envelope.trade else {
        return Err(AlpacaMarketDataError::MissingPrice {
            symbol: symbol.clone(),
        });
    };

    if trade.price <= Usd::ZERO {
        return Err(AlpacaMarketDataError::NonPositivePrice {
            symbol: symbol.clone(),
            price: trade.price,
        });
    }

    Ok(trade.price)
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

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use std::str::FromStr;

    use super::*;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap_or_else(|error| panic!("invalid test symbol: {error}"))
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_rejects_zero_price() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "trade": {
                        "p": "0"
                    }
                }));
        });

        let error = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::NonPositivePrice {
                symbol: error_symbol,
                price
            } if error_symbol == symbol && price == Usd::ZERO
        ));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_rejects_negative_price() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "trade": {
                        "p": "-5"
                    }
                }));
        });

        let error = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::NonPositivePrice { price, .. }
                if price == Usd::from_str("-5").unwrap()
        ));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_returns_positive_price() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "trade": {
                        "p": "123.45"
                    }
                }));
        });

        let price = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap();

        assert_eq!(price, Usd::from_str("123.45").unwrap());
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_accepts_numeric_price() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "trade": {
                        "p": 123.45
                    }
                }));
        });

        let price = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap();

        assert_eq!(price, Usd::from_str("123.45").unwrap());
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_reports_missing_trade() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "trade": null }));
        });

        let error = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::MissingPrice { symbol: error_symbol }
                if error_symbol == symbol
        ));
    }

    #[tokio::test]
    async fn fetch_latest_trade_price_maps_api_error() {
        let server = MockServer::start();
        let client = reqwest::Client::new();
        let symbol = symbol("AAPL");

        server.mock(|when, then| {
            when.method(GET).path("/v2/stocks/AAPL/trades/latest");
            then.status(429)
                .header("content-type", "application/json")
                .json_body(json!({
                    "message": "rate limited"
                }));
        });

        let error = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaMarketDataError::Alpaca(AlpacaError::Api {
                status_code: 429,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn market_data_http_client_sends_apca_headers() {
        let server = MockServer::start();
        let symbol = symbol("AAPL");

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v2/stocks/AAPL/trades/latest")
                .header("APCA-API-KEY-ID", "test_key_id")
                .header("APCA-API-SECRET-KEY", "test_secret_key");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "trade": {
                        "p": "1.5"
                    }
                }));
        });

        let client = market_data_http_client(
            "test_key_id",
            "test_secret_key",
            Duration::from_secs(10),
            Duration::from_secs(30),
        )
        .unwrap();

        let price = fetch_latest_trade_price(&client, &server.base_url(), &symbol)
            .await
            .unwrap();

        assert_eq!(price, Usd::from_str("1.5").unwrap());
        mock.assert();
    }
}
