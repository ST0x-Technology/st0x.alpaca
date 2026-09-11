//! Alpaca Broker API wallet deposit address lookup.
//!
//! Covers wallet asset inspection without touching transfer or whitelist
//! behavior.

use alloy_primitives::Address;
use serde::Deserialize;

use super::transfer::{Network, TokenSymbol};
use super::{AlpacaWalletError, get_json};
use crate::core::AlpacaClient;

/// Gets or creates a wallet deposit address for a specific asset and
/// network.
///
/// Uses `GET /v1/accounts/{account_id}/wallets?asset=...&network=...` per
/// the Alpaca Broker API documentation.
pub(super) async fn get_wallet_address(
    client: &AlpacaClient,
    asset: &TokenSymbol,
    network: &Network,
) -> Result<Address, AlpacaWalletError> {
    #[derive(Deserialize)]
    struct WalletAddressResponse {
        address: Address,
    }

    let url = format!(
        "{}/v1/accounts/{}/wallets?asset={}&network={}",
        client.base_url(),
        client.account_id(),
        asset.as_ref(),
        network.as_ref()
    );

    let response: WalletAddressResponse = get_json(client, &url).await?;

    Ok(response.address)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::core::AlpacaError;
    use crate::wallet::{TEST_ACCOUNT_ID, test_client};

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid test token symbol: {error}"))
    }

    #[tokio::test]
    async fn get_wallet_address_success() {
        let server = MockServer::start();
        let expected_address = address!("0x42a76C83014e886e639768D84EAF3573b1876844");

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets"))
                .query_param("asset", "USDC")
                .query_param("network", "ethereum");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "asset_id": "5d0de74f-827b-41a7-9f74-9c07c08fe55f",
                    "address": expected_address.to_string(),
                    "created_at": "2025-08-07T08:52:40.656166Z"
                }));
        });

        let client = test_client(server.base_url());

        let result = get_wallet_address(&client, &token_symbol("USDC"), &Network::new("ethereum"))
            .await
            .unwrap();

        assert_eq!(result, expected_address);
        mock.assert();
    }

    #[tokio::test]
    async fn get_wallet_address_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets"))
                .query_param("asset", "INVALID")
                .query_param("network", "ethereum");
            then.status(400)
                .header("content-type", "application/json")
                .json_body(json!({
                    "message": "Invalid asset or network"
                }));
        });

        let client = test_client(server.base_url());

        assert!(matches!(
            get_wallet_address(&client, &token_symbol("INVALID"), &Network::new("ethereum"),)
                .await
                .unwrap_err(),
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 400,
                ..
            })
        ));
        mock.assert();
    }

    #[tokio::test]
    async fn get_wallet_address_empty_response() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets"))
                .query_param("asset", "USDC")
                .query_param("network", "ethereum");
            then.status(200)
                .header("content-type", "application/json")
                .body("");
        });

        let client = test_client(server.base_url());

        assert!(matches!(
            get_wallet_address(&client, &token_symbol("USDC"), &Network::new("ethereum"),)
                .await
                .unwrap_err(),
            AlpacaWalletError::Alpaca(AlpacaError::Parse { .. })
        ));
        mock.assert();
    }
}
