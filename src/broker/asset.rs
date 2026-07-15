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
    #[serde(default)]
    pub fractionable: Option<bool>,
    #[serde(default)]
    pub attributes: Option<Vec<String>>,
}

impl Asset {
    /// Reports whether Alpaca supplied `name` in the asset attributes.
    ///
    /// Returns `None` when the response omitted the attributes field, which
    /// preserves the distinction between "not enabled" and "unknown".
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<bool> {
        self.attributes
            .as_ref()
            .map(|attributes| attributes.iter().any(|attribute| attribute == name))
    }
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
                    "tradable": true,
                    "fractionable": true,
                    "attributes": ["fractional_eh_enabled", "overnight_tradable"]
                }));
        });

        let client = test_client(server.base_url());
        let asset = get_asset(&client, &symbol("AAPL")).await.unwrap();

        mock.assert();
        assert_eq!(asset.status, AssetStatus::Active);
        assert!(asset.tradable);
        assert_eq!(asset.fractionable, Some(true));
        assert_eq!(asset.attribute("fractional_eh_enabled"), Some(true));
        assert_eq!(asset.attribute("overnight_halted"), Some(false));
    }

    #[tokio::test]
    async fn get_asset_preserves_missing_optional_capabilities() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/assets/AAPL");
            then.status(200).json_body(json!({
                "status": "active",
                "tradable": true
            }));
        });

        let asset = get_asset(&test_client(server.base_url()), &symbol("AAPL"))
            .await
            .unwrap();

        assert_eq!(asset.fractionable, None);
        assert_eq!(asset.attribute("overnight_tradable"), None);
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

    #[tokio::test]
    async fn get_asset_preserves_rate_limit_backpressure() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/assets/AAPL");
            then.status(429)
                .header("retry-after", "60")
                .body("slow down");
        });

        let error = get_asset(&test_client(server.base_url()), &symbol("AAPL"))
            .await
            .unwrap_err();

        assert_eq!(
            error.backpressure(),
            Some(crate::Backpressure {
                retry_after: Some(std::time::Duration::from_mins(1)),
            })
        );
        assert_eq!(error.permanence(), crate::Permanence::Transient);
    }
}
