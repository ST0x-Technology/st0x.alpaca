//! Position listing for the Alpaca Broker API.

use rust_decimal::Decimal;
use serde::Deserialize;
use st0x_finance::Usd;

use super::{BrokerApiError, Symbol, get_json};
use crate::core::AlpacaClient;

/// Position response from the Alpaca Broker API.
#[derive(Debug, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub asset_class: Option<String>,
    pub exchange: Option<String>,
    /// Tradeable quantity: excludes quantity locked by open orders or
    /// pending transfers.
    #[serde(rename = "qty_available")]
    pub quantity: Decimal,
    /// Total position size including quantity locked by open orders or
    /// pending transfers. Relevant for Alpaca-held USDC, which stays at
    /// Alpaca until a withdrawal settles even while the withdrawal locks
    /// it; equity consumers typically use `quantity` (the tradeable
    /// amount) instead.
    #[serde(rename = "qty")]
    pub total_quantity: Option<Decimal>,
    pub market_value: Option<Usd>,
}

impl Position {
    /// True when the position is not a US equity: crypto pairs, options,
    /// and Alpaca's `USDCUSD` USDC/USD pair (special-cased by symbol so it
    /// is excluded from equity handling even if Alpaca omits the
    /// `asset_class`/`exchange` metadata on the position).
    #[must_use]
    pub fn is_non_equity(&self) -> bool {
        if self.symbol.as_str() == "USDCUSD" {
            return true;
        }

        if let Some(asset_class) = &self.asset_class {
            return !asset_class.eq_ignore_ascii_case("us_equity");
        }

        self.exchange
            .as_deref()
            .is_some_and(|exchange| exchange.eq_ignore_ascii_case("CRYPTO"))
    }
}

/// Lists all open positions on the client's account.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses, and unparseable response bodies.
pub async fn list_positions(client: &AlpacaClient) -> Result<Vec<Position>, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/positions",
        client.base_url(),
        client.account_id()
    );

    get_json(client, &url).await
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::super::{TEST_ACCOUNT_ID, test_client};
    use super::*;
    use crate::core::AlpacaError;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap_or_else(|error| panic!("invalid test symbol: {error}"))
    }

    fn decimal(value: &str) -> Decimal {
        value.parse().unwrap()
    }

    fn usd(value: &str) -> Usd {
        value.parse().unwrap()
    }

    #[tokio::test]
    async fn list_positions_parses_string_values() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/positions"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "symbol": "AAPL",
                        "qty_available": "10.5",
                        "market_value": "1575.005"
                    },
                    {
                        "symbol": "GOOGL",
                        "qty_available": "5.0",
                        "market_value": "750.00"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let positions = list_positions(&client).await.unwrap();

        mock.assert();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, symbol("AAPL"));
        assert_eq!(positions[0].quantity, decimal("10.5"));
        assert_eq!(positions[0].market_value, Some(usd("1575.005")));
        assert_eq!(positions[0].total_quantity, None);
    }

    #[tokio::test]
    async fn list_positions_accepts_numeric_json_values() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/positions"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "symbol": "AAPL",
                        "qty_available": 10.5,
                        "market_value": 1575.00
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let positions = list_positions(&client).await.unwrap();

        mock.assert();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].quantity, decimal("10.5"));
        assert_eq!(positions[0].market_value, Some(usd("1575")));
    }

    #[tokio::test]
    async fn list_positions_rejects_blank_external_symbol() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/positions"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "symbol": "   ",
                    "qty_available": "10.5"
                }]));
        });

        let client = test_client(server.base_url());
        let error = list_positions(&client).await.unwrap_err();

        mock.assert();
        assert!(matches!(
            error,
            BrokerApiError::Alpaca(AlpacaError::Parse { .. })
        ));
    }

    #[tokio::test]
    async fn list_positions_handles_empty_response() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/positions"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let client = test_client(server.base_url());
        let positions = list_positions(&client).await.unwrap();

        mock.assert();
        assert!(positions.is_empty());
    }

    #[tokio::test]
    async fn list_positions_parses_real_usdcusd_position_shape() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/positions"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "asset_class": "crypto",
                        "avg_entry_price": "0.999912891",
                        "cost_basis": "0.788445",
                        "current_price": "0.9995",
                        "exchange": "CRYPTO",
                        "market_value": "0.78812",
                        "qty": "0.788514",
                        "qty_available": "0.788514",
                        "side": "long",
                        "symbol": "USDCUSD"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let positions = list_positions(&client).await.unwrap();

        mock.assert();
        assert_eq!(positions.len(), 1);
        assert!(positions[0].is_non_equity());
        assert_eq!(positions[0].total_quantity, Some(decimal("0.788514")));
    }

    fn position(json: serde_json::Value) -> Position {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn is_non_equity_classifies_usdcusd_without_metadata() {
        let usdc = position(json!({
            "symbol": "USDCUSD",
            "qty": "5000.0",
            "qty_available": "5000.0"
        }));

        assert!(usdc.is_non_equity());
    }

    #[test]
    fn is_non_equity_classifies_crypto_by_exchange_without_asset_class() {
        let bitcoin = position(json!({
            "symbol": "BTC/USD",
            "exchange": "CRYPTO",
            "qty_available": "0.00001"
        }));

        assert!(bitcoin.is_non_equity());
    }

    #[test]
    fn is_non_equity_classifies_options_by_asset_class() {
        let option_position = position(json!({
            "symbol": "AAPL250117C00150000",
            "asset_class": "option",
            "exchange": "OPRA",
            "qty_available": "1"
        }));

        assert!(option_position.is_non_equity());
    }

    #[test]
    fn is_non_equity_keeps_us_equity_positions() {
        let equity = position(json!({
            "symbol": "AAPL",
            "asset_class": "us_equity",
            "qty_available": "10.0"
        }));

        assert!(!equity.is_non_equity());
    }

    #[test]
    fn is_non_equity_keeps_bare_positions_without_metadata() {
        let bare = position(json!({
            "symbol": "RKLB",
            "qty_available": "3.0"
        }));

        assert!(!bare.is_non_equity());
    }
}
