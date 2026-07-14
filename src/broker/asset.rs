//! Asset lookup for tradability and status checks.

use serde::Deserialize;

use super::{BrokerApiError, Symbol, get_json};
use crate::core::AlpacaClient;

/// Asset status from the Alpaca Broker API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetStatus {
    Active,
    Inactive,
}

/// Response from the asset endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub status: AssetStatus,
    pub tradable: bool,
}

/// Fetches asset information for `symbol`.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses (404 for an unknown symbol), and unparseable response bodies.
pub async fn get_asset(client: &AlpacaClient, symbol: &Symbol) -> Result<Asset, BrokerApiError> {
    let url = format!("{}/v1/assets/{symbol}", client.base_url());

    get_json(client, &url).await
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::super::test_client;
    use super::*;
    use crate::core::AlpacaError;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap_or_else(|error| panic!("invalid test symbol: {error}"))
    }

    #[tokio::test]
    async fn get_asset_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/assets/AAPL");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "AAPL",
                    "status": "active",
                    "tradable": true
                }));
        });

        let client = test_client(server.base_url());
        let asset = get_asset(&client, &symbol("AAPL")).await.unwrap();

        mock.assert();
        assert_eq!(asset.status, AssetStatus::Active);
        assert!(asset.tradable);
    }

    #[tokio::test]
    async fn get_asset_not_found() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/assets/INVALID");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({
                    "code": 40_410_000,
                    "message": "asset not found for INVALID"
                }));
        });

        let client = test_client(server.base_url());
        let error = get_asset(&client, &symbol("INVALID")).await.unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, .. }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 404);
    }
}
