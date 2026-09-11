//! Alpaca Broker API crypto wallet surface: USDC deposits and withdrawals.
//!
//! Integrates with the wallet endpoints of the Alpaca Broker API, supporting
//! deposit address lookup, withdrawals, transfer polling, and trusted-address
//! whitelist management.
//!
//! USDC amounts use the shared [`st0x_finance::Usdc`] domain type while
//! preserving Alpaca's JSON string-or-number encoding. This module is
//! telemetry-free: consumers wrap calls with their own instrumentation.
//!
//! # Whitelisting
//!
//! Alpaca requires addresses to be whitelisted before withdrawals. After
//! whitelisting, there is a 24-hour approval period before the address can
//! be used.
//!
//! # Transfer lifecycle
//!
//! Transfers progress through states: Pending -> Processing ->
//! Complete/Failed. Use
//! [`AlpacaWalletService::poll_transfer_until_complete`] to wait for a
//! transfer to reach a terminal state.

use alloy_primitives::{Address, TxHash};
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use st0x_finance::{Positive, Usdc};
use std::borrow::Cow;
use std::time::Duration;
use thiserror::Error;

use crate::core::{AlpacaClient, AlpacaError, Backpressure, Permanence};
use crate::rate_limit::retry_after_from_response_headers;

mod asset;
mod status;
mod transfer;
mod whitelist;

pub use status::PollingConfig;
pub use transfer::{
    AlpacaTransferId, Network, TokenSymbol, Transfer, TransferDirection, TransferStatus,
};
pub use whitelist::{TravelRuleInfo, WhitelistEntry, WhitelistStatus};

/// Service facade for Alpaca crypto wallet operations.
///
/// Provides a high-level API for deposit address lookup, withdrawals,
/// whitelist management, and transfer polling.
#[derive(Debug, Clone)]
pub struct AlpacaWalletService {
    pub client: AlpacaClient,
    pub polling_config: PollingConfig,
}

impl AlpacaWalletService {
    /// Initiates a withdrawal to a whitelisted address.
    ///
    /// The address must be whitelisted and approved before this call.
    ///
    /// # Errors
    ///
    /// Returns an error if the address is not whitelisted and approved, or
    /// if the API call fails.
    pub async fn initiate_withdrawal(
        &self,
        amount: Positive<Usdc>,
        asset: &TokenSymbol,
        to_address: &Address,
    ) -> Result<Transfer, AlpacaWalletError> {
        let network = Network::new("ethereum");

        if !whitelist::is_address_whitelisted_and_approved(
            &self.client,
            to_address,
            asset,
            &network,
        )
        .await?
        {
            return Err(AlpacaWalletError::AddressNotWhitelisted {
                address: *to_address,
                asset: asset.clone(),
                network,
            });
        }

        transfer::initiate_withdrawal(&self.client, amount, asset, to_address).await
    }

    /// Polls a transfer until it reaches a terminal state (Complete or
    /// Failed).
    ///
    /// Retries transient server errors and times out after the configured
    /// duration.
    ///
    /// # Errors
    ///
    /// Returns an error if the transfer times out, an invalid status
    /// regression is detected, or the API call fails persistently.
    pub async fn poll_transfer_until_complete(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<Transfer, AlpacaWalletError> {
        status::poll_transfer_status(&self.client, transfer_id, &self.polling_config).await
    }

    /// Polls for an incoming deposit by its on-chain transaction hash.
    ///
    /// Alpaca auto-detects incoming transfers to their funding wallet
    /// addresses. This method polls until the deposit is detected and
    /// reaches a terminal state.
    ///
    /// # Errors
    ///
    /// Returns an error if the deposit is not detected within the timeout or
    /// the API call fails persistently.
    pub async fn poll_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Transfer, AlpacaWalletError> {
        status::poll_deposit_by_tx_hash(&self.client, tx_hash, &self.polling_config).await
    }

    /// Looks up an incoming deposit by its on-chain transaction hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails or the transfer list cannot be
    /// parsed.
    pub async fn find_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<Transfer>, AlpacaWalletError> {
        transfer::find_deposit_by_tx_hash(&self.client, tx_hash).await
    }

    /// Gets or creates a wallet deposit address for an asset and network.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails or the response cannot be
    /// parsed.
    pub async fn get_wallet_address(
        &self,
        asset: &TokenSymbol,
        network: &Network,
    ) -> Result<Address, AlpacaWalletError> {
        asset::get_wallet_address(&self.client, asset, network).await
    }

    /// Creates a whitelist entry for a withdrawal address.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails or the response cannot be
    /// parsed.
    pub async fn create_whitelist_entry(
        &self,
        address: &Address,
        asset: &TokenSymbol,
        network: &Network,
        travel_rule_info: &TravelRuleInfo,
    ) -> Result<WhitelistEntry, AlpacaWalletError> {
        whitelist::create_whitelist_entry(&self.client, address, asset, network, travel_rule_info)
            .await
    }

    /// Removes all whitelist entries matching the given address.
    ///
    /// Returns the entries that were deleted.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaWalletError::NoWhitelistEntries`] if no entries match
    /// the address, or an error if any API call fails.
    pub async fn remove_whitelist_entries(
        &self,
        address: &Address,
    ) -> Result<Vec<WhitelistEntry>, AlpacaWalletError> {
        let entries = whitelist::get_whitelisted_addresses(&self.client).await?;

        let matching: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.address == *address)
            .collect();

        if matching.is_empty() {
            return Err(AlpacaWalletError::NoWhitelistEntries { address: *address });
        }

        for entry in &matching {
            whitelist::delete_whitelist_entry(&self.client, &entry.id).await?;
        }

        Ok(matching)
    }

    /// Patches travel rule info onto all existing whitelisted addresses.
    ///
    /// Returns all whitelist entries that were patched.
    ///
    /// # Errors
    ///
    /// Returns an error if listing the whitelist or any patch call fails.
    pub async fn patch_all_whitelist_travel_rules(
        &self,
        travel_rule_info: &TravelRuleInfo,
    ) -> Result<Vec<WhitelistEntry>, AlpacaWalletError> {
        let entries = whitelist::get_whitelisted_addresses(&self.client).await?;

        for entry in &entries {
            whitelist::patch_whitelist_travel_rule(&self.client, &entry.id, travel_rule_info)
                .await?;
        }

        Ok(entries)
    }

    /// Gets all whitelisted addresses for this account.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails or the response cannot be
    /// parsed.
    pub async fn get_whitelisted_addresses(
        &self,
    ) -> Result<Vec<WhitelistEntry>, AlpacaWalletError> {
        whitelist::get_whitelisted_addresses(&self.client).await
    }

    /// Lists all transfers for this account.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails or the response cannot be
    /// parsed.
    pub async fn list_all_transfers(&self) -> Result<Vec<Transfer>, AlpacaWalletError> {
        transfer::list_all_transfers(&self.client).await
    }
}

/// Errors from the crypto wallet surface.
///
/// Transport, parse, and API-status failures are carried by the shared
/// [`AlpacaError`] taxonomy; the remaining variants are wallet-surface
/// invariants detected client-side.
#[derive(Debug, Error)]
pub enum AlpacaWalletError {
    #[error(transparent)]
    Alpaca(#[from] AlpacaError),

    #[error("Transfer not found: {transfer_id}")]
    TransferNotFound { transfer_id: AlpacaTransferId },

    #[error("Transfer {transfer_id} timed out after {elapsed:?}")]
    TransferTimeout {
        transfer_id: AlpacaTransferId,
        elapsed: Duration,
    },

    #[error(
        "Invalid status transition for transfer {transfer_id}: \
         {previous:?} -> {next:?}"
    )]
    InvalidStatusTransition {
        transfer_id: AlpacaTransferId,
        previous: TransferStatus,
        next: TransferStatus,
    },

    #[error("Address {address} is not whitelisted for {asset} on {network}")]
    AddressNotWhitelisted {
        address: Address,
        asset: TokenSymbol,
        network: Network,
    },

    #[error("No whitelist entries found for address {address}")]
    NoWhitelistEntries { address: Address },

    #[error("Deposit with tx hash {tx_hash} not detected after {elapsed:?}")]
    DepositTimeout { tx_hash: TxHash, elapsed: Duration },

    #[error(
        "Invalid status transition for deposit {tx_hash}: \
         {previous:?} -> {next:?}"
    )]
    InvalidDepositTransition {
        tx_hash: TxHash,
        previous: TransferStatus,
        next: TransferStatus,
    },
}

impl AlpacaWalletError {
    /// Returns broker backpressure metadata when the underlying request was
    /// rate limited.
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match self {
            Self::Alpaca(error) => error.backpressure(),
            _ => None,
        }
    }

    /// Classifies whether retrying the same wallet operation can plausibly
    /// succeed.
    #[must_use]
    pub fn permanence(&self) -> Permanence {
        match self {
            Self::Alpaca(error) => error.permanence(),
            Self::TransferTimeout { .. }
            | Self::DepositTimeout { .. }
            | Self::TransferNotFound { .. } => Permanence::Transient,
            Self::InvalidStatusTransition { .. }
            | Self::AddressNotWhitelisted { .. }
            | Self::NoWhitelistEntries { .. }
            | Self::InvalidDepositTransition { .. } => Permanence::Permanent,
        }
    }
}

/// Sends an authenticated GET and parses the JSON response body.
async fn get_json<Response: DeserializeOwned>(
    client: &AlpacaClient,
    url: &str,
) -> Result<Response, AlpacaWalletError> {
    request_json(client.get(url).await?).await
}

/// Sends an authenticated POST with a JSON body and parses the JSON
/// response body.
async fn post_json<Response: DeserializeOwned, Body: Serialize + Sync>(
    client: &AlpacaClient,
    url: &str,
    body: &Body,
) -> Result<Response, AlpacaWalletError> {
    request_json(client.post(url).await?.json(body)).await
}

/// Sends an authenticated DELETE, expecting no response body.
async fn delete(client: &AlpacaClient, url: &str) -> Result<(), AlpacaWalletError> {
    let response = client
        .delete(url)
        .await?
        .send()
        .await
        .map_err(AlpacaError::from)?;

    read_empty_response(response).await
}

/// Sends an authenticated PATCH with a JSON body, expecting no response
/// body.
async fn patch<Body: Serialize + Sync>(
    client: &AlpacaClient,
    url: &str,
    body: &Body,
) -> Result<(), AlpacaWalletError> {
    let response = client
        .patch(url)
        .await?
        .json(body)
        .send()
        .await
        .map_err(AlpacaError::from)?;

    read_empty_response(response).await
}

async fn request_json<Response: DeserializeOwned>(
    builder: reqwest::RequestBuilder,
) -> Result<Response, AlpacaWalletError> {
    let response = builder.send().await.map_err(AlpacaError::from)?;
    let status = response.status();
    let retry_after = retry_after_from_response_headers(response.headers());

    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        // Preserve the HTTP status on a non-success response even if the
        // body stream fails to read, so the poll retry predicate (which only
        // retries 5xx API errors) still fires on a transient 5xx.
        Err(_) if !status.is_success() => {
            return Err(api_error(status, b"Unknown error", retry_after));
        }
        Err(error) => return Err(AlpacaError::from(error).into()),
    };

    if status.is_success() {
        // Parse with `from_slice` so invalid UTF-8 fails fast rather than
        // being silently replaced by lossy decoding before parse. The parse
        // error carries the (redacted) body for diagnostics.
        return serde_json::from_slice(&bytes).map_err(|source| {
            AlpacaWalletError::Alpaca(AlpacaError::Parse {
                body: redact_beneficiary(&String::from_utf8_lossy(&bytes)).into_owned(),
                source,
            })
        });
    }

    Err(api_error(status, &bytes, retry_after))
}

async fn read_empty_response(response: reqwest::Response) -> Result<(), AlpacaWalletError> {
    let status = response.status();
    let retry_after = retry_after_from_response_headers(response.headers());

    if status.is_success() {
        return Ok(());
    }

    let Ok(bytes) = response.bytes().await else {
        return Err(api_error(status, b"Unknown error", retry_after));
    };

    Err(api_error(status, &bytes, retry_after))
}

/// Maps a non-2xx response into the shared [`AlpacaError::Api`] variant.
///
/// The stored body is surfaced via `Display`/`Debug` and typically ends up
/// in consumer logs, so the Travel Rule beneficiary identity is scrubbed
/// before storing it.
fn api_error(
    status: StatusCode,
    bytes: &[u8],
    retry_after: Option<std::time::Duration>,
) -> AlpacaWalletError {
    let body = redact_beneficiary(&String::from_utf8_lossy(bytes)).into_owned();
    if status == StatusCode::TOO_MANY_REQUESTS {
        AlpacaWalletError::Alpaca(AlpacaError::RateLimited { body, retry_after })
    } else {
        AlpacaWalletError::Alpaca(AlpacaError::Api {
            status_code: status.as_u16(),
            body,
        })
    }
}

/// Redacts `beneficiary_entity_name` values from a response body destined
/// for error messages (and, through them, consumer logs). Whitelist
/// responses echo back the Travel Rule beneficiary identity, which is
/// deliberately kept out of logs. Successful parses still hand callers the
/// full data; only error/diagnostic bodies are scrubbed.
fn redact_beneficiary(body: &str) -> Cow<'_, str> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut value) => {
            redact_beneficiary_in_place(&mut value);
            match serde_json::to_string(&value) {
                Ok(redacted) => Cow::Owned(redacted),
                // Fail closed: never fall back to the raw (unredacted) body,
                // or a re-serialization failure would leak the beneficiary
                // name.
                Err(_) => Cow::Owned(format!("<redaction failed, body {} bytes>", body.len())),
            }
        }
        // Non-JSON bodies (e.g. plain-text gateway errors) carry no
        // structured travel-rule data, so they are normally safe to keep
        // verbatim. Fail closed if such a body nonetheless references the
        // sensitive field by name (e.g. a proxy echoing the request), rather
        // than leaking it.
        Err(_) if body.contains("beneficiary_entity_name") => Cow::Owned(format!(
            "<redaction failed, non-JSON body {} bytes>",
            body.len()
        )),
        Err(_) => Cow::Borrowed(body),
    }
}

fn redact_beneficiary_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "beneficiary_entity_name" {
                    *child = serde_json::Value::String("<redacted>".to_string());
                } else {
                    redact_beneficiary_in_place(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_beneficiary_in_place(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

#[cfg(test)]
pub(crate) const TEST_ACCOUNT_ID: &str = "904837e3-3b76-47ec-b432-046db621571b";

#[cfg(test)]
pub(crate) fn test_client(base_url: String) -> AlpacaClient {
    AlpacaClient::new(
        base_url,
        TEST_ACCOUNT_ID.to_string(),
        "test_key_id".to_string(),
        "test_secret_key".to_string(),
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid test token symbol: {error}"))
    }

    fn positive_usdc(value: &str) -> Positive<Usdc> {
        let amount = value
            .parse()
            .unwrap_or_else(|error| panic!("invalid test USDC amount: {error}"));
        Positive::new(amount)
            .unwrap_or_else(|error| panic!("non-positive test USDC amount: {error}"))
    }

    fn test_service(server: &MockServer) -> AlpacaWalletService {
        AlpacaWalletService {
            client: test_client(server.base_url()),
            polling_config: PollingConfig::default(),
        }
    }

    #[tokio::test]
    async fn get_json_sends_dual_auth_headers() {
        let server = MockServer::start();

        // Basic auth header:
        // base64("test_key_id:test_secret_key")
        // = "dGVzdF9rZXlfaWQ6dGVzdF9zZWNyZXRfa2V5"
        let test_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/test")
                .header(
                    "authorization",
                    "Basic dGVzdF9rZXlfaWQ6dGVzdF9zZWNyZXRfa2V5",
                )
                .header("APCA-API-KEY-ID", "test_key_id")
                .header("APCA-API-SECRET-KEY", "test_secret_key");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"success": true}));
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/test", client.base_url());

        let response: serde_json::Value = get_json(&client, &url).await.unwrap();

        assert_eq!(response, json!({"success": true}));
        test_mock.assert();
    }

    #[tokio::test]
    async fn get_json_maps_api_error() {
        let server = MockServer::start();

        let error_mock = server.mock(|when, then| {
            when.method(GET).path("/v1/error");
            then.status(401).json_body(json!({
                "message": "Invalid credentials"
            }));
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/error", client.base_url());

        let error = get_json::<serde_json::Value>(&client, &url)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 401,
                ..
            })
        ));
        error_mock.assert();
    }

    #[tokio::test]
    async fn get_json_preserves_rate_limit_backpressure() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/rate-limited");
            then.status(429)
                .header("retry-after", "30")
                .body("slow down");
        });

        let client = test_client(server.base_url());
        let error = get_json::<serde_json::Value>(
            &client,
            &format!("{}/v1/rate-limited", client.base_url()),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.backpressure(),
            Some(Backpressure {
                retry_after: Some(Duration::from_secs(30)),
            })
        );
        assert_eq!(error.permanence(), Permanence::Transient);
    }

    #[tokio::test]
    async fn get_json_maps_server_error() {
        let server = MockServer::start();

        let error_mock = server.mock(|when, then| {
            when.method(GET).path("/v1/server_error");
            then.status(500).body("Internal Server Error");
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/server_error", client.base_url());

        let error = get_json::<serde_json::Value>(&client, &url)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));
        error_mock.assert();
    }

    #[tokio::test]
    async fn post_json_sends_json_body() {
        let server = MockServer::start();

        let test_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/test")
                .header("APCA-API-KEY-ID", "test_key_id")
                .header("APCA-API-SECRET-KEY", "test_secret_key")
                .json_body(json!({"amount": "10.5", "asset": "USDC"}));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({"success": true}));
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/test", client.base_url());
        let body = json!({"amount": "10.5", "asset": "USDC"});

        let response: serde_json::Value = post_json(&client, &url, &body).await.unwrap();

        assert_eq!(response, json!({"success": true}));
        test_mock.assert();
    }

    #[tokio::test]
    async fn post_json_maps_api_error() {
        let server = MockServer::start();

        let error_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/error");
            then.status(400).json_body(json!({
                "message": "Invalid request"
            }));
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/error", client.base_url());
        let body = json!({"test": "data"});

        let error = post_json::<serde_json::Value, _>(&client, &url, &body)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 400,
                ..
            })
        ));
        error_mock.assert();
    }

    #[tokio::test]
    async fn non_utf8_success_body_is_parse_error() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/test");
            then.status(200)
                .header("content-type", "application/octet-stream")
                .body(b"\xFF\xFE not valid utf-8");
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/test", client.base_url());

        let error = get_json::<serde_json::Value>(&client, &url)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Parse { .. })
        ));
    }

    #[tokio::test]
    async fn api_error_body_redacts_beneficiary_entity_name() {
        let server = MockServer::start();

        // A non-2xx body echoing the Travel Rule beneficiary identity must
        // have that identity scrubbed from the stored error body, which is
        // surfaced via Display/Debug and re-logged downstream.
        server.mock(|when, then| {
            when.method(POST).path("/v1/whitelists");
            then.status(400)
                .header("content-type", "application/json")
                .json_body(json!({
                    "message": "invalid travel rule info",
                    "travel_rule_info": {
                        "beneficiary_is_self_hosted": true,
                        "beneficiary_entity_name": "T0 TRADE (BVI) LTD"
                    }
                }));
        });

        let client = test_client(server.base_url());
        let url = format!("{}/v1/whitelists", client.base_url());

        let AlpacaWalletError::Alpaca(AlpacaError::Api { status_code, body }) =
            post_json::<serde_json::Value, _>(&client, &url, &json!({"any": "body"}))
                .await
                .unwrap_err()
        else {
            panic!("expected AlpacaError::Api");
        };

        assert_eq!(status_code, 400);
        assert!(!body.contains("T0 TRADE (BVI) LTD"));
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed["travel_rule_info"]["beneficiary_entity_name"],
            "<redacted>"
        );
        // Non-sensitive fields must survive redaction for debuggability.
        assert_eq!(parsed["message"], "invalid travel rule info");
        assert_eq!(
            parsed["travel_rule_info"]["beneficiary_is_self_hosted"],
            true
        );
    }

    #[test]
    fn redact_passes_through_non_json_bodies() {
        // Gateway/plain-text error bodies (common during incidents) carry no
        // structured travel-rule data and are returned verbatim, borrowed.
        let body = "502 Bad Gateway";

        let redacted = redact_beneficiary(body);

        assert!(matches!(redacted, Cow::Borrowed(_)));
        assert_eq!(redacted, "502 Bad Gateway");
    }

    #[test]
    fn redact_scrubs_beneficiary_name_nested_in_arrays() {
        let body = json!({
            "entries": [
                {
                    "travel_rule_info": {
                        "beneficiary_entity_name": "T0 TRADE (BVI) LTD",
                        "beneficiary_is_self_hosted": true
                    }
                }
            ]
        })
        .to_string();

        let redacted = redact_beneficiary(&body);

        assert!(matches!(redacted, Cow::Owned(_)));
        let parsed: serde_json::Value = serde_json::from_str(&redacted).unwrap();
        // The key must be present with the redacted value (not deleted), the
        // non-sensitive sibling preserved, and the original value gone.
        assert_eq!(
            parsed["entries"][0]["travel_rule_info"]["beneficiary_entity_name"],
            "<redacted>"
        );
        assert_eq!(
            parsed["entries"][0]["travel_rule_info"]["beneficiary_is_self_hosted"],
            true
        );
        assert!(!redacted.contains("T0 TRADE (BVI) LTD"));
    }

    #[test]
    fn redact_fails_closed_on_non_json_body_referencing_beneficiary() {
        // A non-JSON body that echoes the sensitive field (e.g. a proxy
        // reflecting the request) must not be kept verbatim.
        let body = "error: beneficiary_entity_name 'T0 TRADE (BVI) LTD' rejected";

        let redacted = redact_beneficiary(body);

        assert!(matches!(redacted, Cow::Owned(_)));
        assert!(!redacted.contains("T0 TRADE (BVI) LTD"));
    }

    #[test]
    fn redact_leaves_json_without_beneficiary_field_unchanged() {
        let body = json!({ "id": "wl-1", "status": "approved" }).to_string();

        let redacted = redact_beneficiary(&body);

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&redacted).unwrap(),
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        );
    }

    #[tokio::test]
    async fn initiate_withdrawal_not_whitelisted() {
        let server = MockServer::start();
        let service = test_service(&server);

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let asset = token_symbol("USDC");
        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");
        let amount = positive_usdc("100");

        assert!(matches!(
            service
                .initiate_withdrawal(amount, &asset, &to_address)
                .await
                .unwrap_err(),
            AlpacaWalletError::AddressNotWhitelisted { .. }
        ));
        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn initiate_withdrawal_pending_whitelist() {
        let server = MockServer::start();
        let service = test_service(&server);

        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": "whitelist-123",
                    "address": to_address.to_string(),
                    "asset": "USDC",
                    "chain": "ethereum",
                    "status": "PENDING",
                    "created_at": "2024-01-01T00:00:00Z"
                }]));
        });

        let asset = token_symbol("USDC");
        let amount = positive_usdc("100");

        assert!(matches!(
            service
                .initiate_withdrawal(amount, &asset, &to_address)
                .await
                .unwrap_err(),
            AlpacaWalletError::AddressNotWhitelisted { .. }
        ));
        whitelist_mock.assert();
    }

    #[tokio::test]
    async fn initiate_withdrawal_approved() {
        let server = MockServer::start();
        let service = test_service(&server);

        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": "whitelist-123",
                    "address": to_address.to_string(),
                    "asset": "USDC",
                    "chain": "ethereum",
                    "status": "APPROVED",
                    "created_at": "2024-01-01T00:00:00Z"
                }]));
        });

        let transfer_mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "550e8400-e29b-41d4-a716-446655440000",
                    "direction": "OUTGOING",
                    "amount": "100",
                    "usd_value": "100",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": to_address.to_string(),
                    "status": "PENDING",
                    "tx_hash": null,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0",
                    "fees": "0"
                }));
        });

        let asset = token_symbol("USDC");
        let amount = positive_usdc("100");

        let result = service
            .initiate_withdrawal(amount, &asset, &to_address)
            .await
            .unwrap();

        assert_eq!(result.to, to_address);
        whitelist_mock.assert();
        transfer_mock.assert();
    }

    #[tokio::test]
    async fn poll_transfer_until_complete_returns_terminal_transfer() {
        let server = MockServer::start();

        let polling_config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let service = AlpacaWalletService {
            client: test_client(server.base_url()),
            polling_config,
        };

        let transfer_id = Uuid::new_v4();

        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": transfer_id,
                    "direction": "OUTGOING",
                    "amount": "100",
                    "usd_value": "100",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "COMPLETE",
                    "tx_hash": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let tid = AlpacaTransferId::from(transfer_id);
        let result = service.poll_transfer_until_complete(&tid).await.unwrap();

        assert_eq!(result.status, TransferStatus::Complete);
        status_mock.assert();
    }

    #[tokio::test]
    async fn remove_whitelist_entries_found() {
        let server = MockServer::start();
        let service = test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": "wl-111",
                    "address": target.to_string(),
                    "asset": "USDC",
                    "chain": "ethereum",
                    "status": "APPROVED",
                    "created_at": "2024-01-01T00:00:00Z"
                }]));
        });

        let delete_mock = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists/wl-111"
            ));
            then.status(204);
        });

        let removed = service.remove_whitelist_entries(&target).await.unwrap();

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, "wl-111");
        assert_eq!(removed[0].address, target);
        list_mock.assert();
        delete_mock.assert();
    }

    #[tokio::test]
    async fn remove_whitelist_entries_not_found() {
        let server = MockServer::start();
        let service = test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");
        let other = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": "wl-222",
                    "address": other,
                    "asset": "USDC",
                    "chain": "ethereum",
                    "status": "APPROVED",
                    "created_at": "2024-01-01T00:00:00Z"
                }]));
        });

        assert!(matches!(
            service
                .remove_whitelist_entries(&target)
                .await
                .unwrap_err(),
            AlpacaWalletError::NoWhitelistEntries { address }
                if address == target
        ));
        list_mock.assert();
    }

    #[tokio::test]
    async fn remove_whitelist_entries_multiple() {
        let server = MockServer::start();
        let service = test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": "wl-aaa",
                        "address": target.to_string(),
                        "asset": "USDC",
                        "chain": "ethereum",
                        "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    },
                    {
                        "id": "wl-bbb",
                        "address": target.to_string(),
                        "asset": "USDC",
                        "chain": "ethereum",
                        "status": "PENDING",
                        "created_at": "2024-02-01T00:00:00Z"
                    }
                ]));
        });

        let delete_aaa = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists/wl-aaa"
            ));
            then.status(204);
        });

        let delete_bbb = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/whitelists/wl-bbb"
            ));
            then.status(204);
        });

        let removed = service.remove_whitelist_entries(&target).await.unwrap();

        assert_eq!(removed.len(), 2);
        list_mock.assert();
        delete_aaa.assert();
        delete_bbb.assert();
    }
}
