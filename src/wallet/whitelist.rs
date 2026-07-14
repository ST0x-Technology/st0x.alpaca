//! Alpaca Broker API trusted address whitelist management for crypto
//! withdrawals.
//!
//! Addresses must be whitelisted and approved (24h waiting period) before
//! withdrawals can target them.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::transfer::{Network, TokenSymbol};
use super::{AlpacaWalletError, delete, get_json, patch, post_json};
use crate::core::AlpacaClient;

/// Travel Rule beneficiary info for Alpaca whitelist creation requests.
///
/// Alpaca requires this on all `POST /whitelists` calls effective
/// 2026-03-27. Only self-hosted wallets are supported, so
/// `is_self_hosted` is always `true`.
///
/// Field names use `serde(rename)` to match the Alpaca API's
/// `beneficiary_*` JSON keys while avoiding the `struct_field_names` lint.
#[derive(Debug, Clone, Serialize)]
pub struct TravelRuleInfo {
    #[serde(rename = "beneficiary_is_self_hosted")]
    is_self_hosted: bool,

    #[serde(rename = "beneficiary_entity_name")]
    entity_name: String,
}

impl TravelRuleInfo {
    #[must_use]
    pub fn new(beneficiary_entity_name: String) -> Self {
        Self {
            is_self_hosted: true,
            entity_name: beneficiary_entity_name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WhitelistStatus {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WhitelistEntry {
    pub id: String,
    pub address: Address,
    pub asset: TokenSymbol,
    pub chain: Network,
    pub status: WhitelistStatus,
    pub created_at: DateTime<Utc>,
}

pub(super) async fn get_whitelisted_addresses(
    client: &AlpacaClient,
) -> Result<Vec<WhitelistEntry>, AlpacaWalletError> {
    let url = format!(
        "{}/v1/accounts/{}/wallets/whitelists",
        client.base_url(),
        client.account_id()
    );

    get_json(client, &url).await
}

pub(super) async fn is_address_whitelisted_and_approved(
    client: &AlpacaClient,
    address: &Address,
    asset: &TokenSymbol,
    _network: &Network,
) -> Result<bool, AlpacaWalletError> {
    let entries = get_whitelisted_addresses(client).await?;

    // NOTE: Chain comparison disabled due to Alpaca API inconsistency.
    // Request uses "ethereum" but response returns "ETH".
    // TODO: Re-enable once Alpaca fixes the chain field or we normalize
    // values.
    Ok(entries.iter().any(|entry| {
        entry.address == *address
            && entry.asset == *asset
            && entry.status == WhitelistStatus::Approved
    }))
}

/// Creates a whitelist entry for a withdrawal address.
///
/// The address will be in PENDING status initially and must be approved
/// before withdrawals can be made (typically within 24 hours).
/// Alpaca requires `travel_rule_info` on all whitelist creation requests,
/// effective 2026-03-27.
pub(super) async fn create_whitelist_entry(
    client: &AlpacaClient,
    address: &Address,
    asset: &TokenSymbol,
    _network: &Network,
    travel_rule_info: &TravelRuleInfo,
) -> Result<WhitelistEntry, AlpacaWalletError> {
    #[derive(Serialize)]
    struct Request<'a> {
        address: String,
        asset: &'a str,
        travel_rule_info: &'a TravelRuleInfo,
    }

    let url = format!(
        "{}/v1/accounts/{}/wallets/whitelists",
        client.base_url(),
        client.account_id()
    );

    let request = Request {
        // None = standard EIP-55 checksum (no chain-specific EIP-1191
        // encoding). Fine for now since this system only handles Ethereum
        // mainnet.
        address: address.to_checksum(None),
        asset: asset.as_ref(),
        travel_rule_info,
    };

    post_json(client, &url, &request).await
}

pub(super) async fn delete_whitelist_entry(
    client: &AlpacaClient,
    whitelist_id: &str,
) -> Result<(), AlpacaWalletError> {
    let url = format!(
        "{}/v1/accounts/{}/wallets/whitelists/{}",
        client.base_url(),
        client.account_id(),
        whitelist_id
    );

    delete(client, &url).await
}

/// Updates travel rule info on an existing whitelisted address.
///
/// Uses the PATCH endpoint added by Alpaca for the March 2026 travel rule
/// requirement. Existing whitelists that were created without travel rule
/// info must be patched before they can be used for withdrawals.
pub(super) async fn patch_whitelist_travel_rule(
    client: &AlpacaClient,
    whitelist_id: &str,
    travel_rule_info: &TravelRuleInfo,
) -> Result<(), AlpacaWalletError> {
    #[derive(Serialize)]
    struct Request<'a> {
        travel_rule_info: &'a TravelRuleInfo,
    }

    let url = format!(
        "{}/v1/accounts/{}/wallets/whitelists/{}/travel-rule-info",
        client.base_url(),
        client.account_id(),
        whitelist_id
    );

    patch(client, &url, &Request { travel_rule_info }).await
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::wallet::{TEST_ACCOUNT_ID, test_client};

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid test token symbol: {error}"))
    }

    #[tokio::test]
    async fn get_whitelisted_addresses_parses_entries() {
        let server = MockServer::start();
        let test_address1 = "0x1234567890abcdef1234567890abcdef12345678";
        let test_address2 = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "whitelist-123",
                        "address": test_address1,
                        "asset": "USDC",
                        "chain": "Ethereum",
                        "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    },
                    {
                        "id": "whitelist-456",
                        "address": test_address2,
                        "asset": "USDC",
                        "chain": "Polygon",
                        "status": "PENDING",
                        "created_at": "2024-01-02T00:00:00Z"
                    }
                ]));
        });

        let client = test_client(server.base_url());

        let result = get_whitelisted_addresses(&client).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "whitelist-123");
        assert_eq!(result[0].status, WhitelistStatus::Approved);
        assert_eq!(result[1].id, "whitelist-456");
        assert_eq!(result[1].status, WhitelistStatus::Pending);

        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn is_address_whitelisted_and_approved_true() {
        let server = MockServer::start();
        let address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "whitelist-123",
                        "address": address,
                        "asset": "USDC",
                        "chain": "Ethereum",
                        "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let asset = token_symbol("USDC");
        let network = Network::new("Ethereum");

        let result = is_address_whitelisted_and_approved(&client, &address, &asset, &network)
            .await
            .unwrap();

        assert!(result);

        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn is_address_whitelisted_and_approved_pending() {
        let server = MockServer::start();
        let address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "whitelist-123",
                        "address": address,
                        "asset": "USDC",
                        "chain": "Ethereum",
                        "status": "PENDING",
                        "created_at": "2024-01-01T00:00:00Z"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let asset = token_symbol("USDC");
        let network = Network::new("Ethereum");

        let result = is_address_whitelisted_and_approved(&client, &address, &asset, &network)
            .await
            .unwrap();

        assert!(!result);

        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn is_address_whitelisted_and_approved_rejected() {
        let server = MockServer::start();
        let address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "whitelist-123",
                        "address": address,
                        "asset": "USDC",
                        "chain": "Ethereum",
                        "status": "REJECTED",
                        "created_at": "2024-01-01T00:00:00Z"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let asset = token_symbol("USDC");
        let network = Network::new("Ethereum");

        let result = is_address_whitelisted_and_approved(&client, &address, &asset, &network)
            .await
            .unwrap();

        assert!(!result);

        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn is_address_whitelisted_and_approved_not_found() {
        let server = MockServer::start();
        let other_address = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "whitelist-123",
                        "address": other_address,
                        "asset": "USDC",
                        "chain": "Ethereum",
                        "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let address = address!("0x1234567890abcdef1234567890abcdef12345678");
        let asset = token_symbol("USDC");
        let network = Network::new("Ethereum");

        let result = is_address_whitelisted_and_approved(&client, &address, &asset, &network)
            .await
            .unwrap();

        assert!(!result);

        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn create_whitelist_entry_sends_travel_rule_info() {
        let server = MockServer::start();
        let target = address!("0x1234567890abcdef1234567890abcdef12345678");

        let travel_rule = TravelRuleInfo::new("T0 TRADE (BVI) LTD".to_string());

        let checksummed = target.to_checksum(None);

        let create_mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"))
                .json_body(json!({
                    "address": checksummed,
                    "asset": "USDC",
                    "travel_rule_info": {
                        "beneficiary_is_self_hosted": true,
                        "beneficiary_entity_name": "T0 TRADE (BVI) LTD"
                    }
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "whitelist-new",
                    "address": target.to_string(),
                    "asset": "USDC",
                    "chain": "Ethereum",
                    "status": "PENDING",
                    "created_at": "2024-01-01T00:00:00Z"
                }));
        });

        let client = test_client(server.base_url());
        let asset = token_symbol("USDC");
        let network = Network::new("Ethereum");

        let entry = create_whitelist_entry(&client, &target, &asset, &network, &travel_rule)
            .await
            .unwrap();

        assert_eq!(entry.id, "whitelist-new");
        assert_eq!(entry.status, WhitelistStatus::Pending);

        // httpmock asserts the full JSON body matched, confirming
        // travel_rule_info was serialized correctly.
        create_mock.assert();
    }

    #[tokio::test]
    async fn patch_whitelist_travel_rule_sends_expected_body() {
        let server = MockServer::start();

        let travel_rule = TravelRuleInfo::new("Acme Corp".to_string());

        let whitelist_id = "wl-abc-123";

        let patch_mock = server.mock(|when, then| {
            when.method(PATCH)
                .path(format!(
                    "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists/{whitelist_id}/travel-rule-info"
                ))
                .header("APCA-API-KEY-ID", "test_key_id")
                .header("APCA-API-SECRET-KEY", "test_secret_key")
                .json_body(json!({
                    "travel_rule_info": {
                        "beneficiary_is_self_hosted": true,
                        "beneficiary_entity_name": "Acme Corp"
                    }
                }));
            then.status(204);
        });

        let client = test_client(server.base_url());

        patch_whitelist_travel_rule(&client, whitelist_id, &travel_rule)
            .await
            .unwrap();

        patch_mock.assert();
    }
}
