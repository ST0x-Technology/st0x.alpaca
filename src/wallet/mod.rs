//! Alpaca Broker API crypto wallet client for USDC deposits and withdrawals.
//!
//! This module integrates with the wallet endpoints of the Alpaca Broker API,
//! supporting USDC deposit address lookup, deposits, and withdrawals.
//!
//! # Authentication
//!
//! Authentication uses Alpaca Broker API credentials (API key and secret).
//! The client automatically fetches and caches the account ID.
//!
//! # Whitelisting
//!
//! Alpaca requires addresses to be whitelisted before
//! withdrawals. After whitelisting, there is a 24-hour
//! approval period before the address can be used.
//!
//! # Transfer Lifecycle
//!
//! Transfers progress through states:
//! Pending -> Processing -> Complete/Failed.
//! Use `poll_transfer_until_complete()` to wait for a
//! transfer to reach a terminal state, or
//! [`poll_transfer_until_complete_with`] to poll any [`WalletTransfers`].

mod asset;
mod client;
mod status;
mod transfer;
mod whitelist;

use alloy_primitives::{Address, TxHash};
use std::sync::Arc;
use tracing::error;

use st0x_finance::Usdc;

use crate::broker::{AlpacaAccountId, Positive};

#[cfg(any(test, feature = "test-support"))]
pub use client::AlpacaWalletClient;
#[cfg(not(any(test, feature = "test-support")))]
use client::AlpacaWalletClient;
pub use client::AlpacaWalletError;
pub use status::{
    PollingConfig, poll_deposit_by_tx_hash_with, poll_transfer_tx_hash_with,
    poll_transfer_until_complete_with,
};
pub use transfer::{
    AlpacaTransferId, Network, ReportedFeesError, TokenSymbol, Transfer, TransferDirection,
    TransferStatus, TransferWithFees, WalletTransfers,
};
pub use whitelist::{TravelRuleInfo, WhitelistEntry, WhitelistStatus};

/// Service facade for Alpaca crypto wallet operations.
///
/// Provides a high-level API for deposit address lookup, deposits, withdrawals,
/// and transfer polling.
pub struct AlpacaWalletService {
    client: Arc<AlpacaWalletClient>,
    polling_config: PollingConfig,
}

impl AlpacaWalletService {
    /// Builds the wallet service for `account_id` at `base_url`.
    ///
    /// # Errors
    ///
    /// Returns the client construction or authentication error.
    pub fn new(
        base_url: String,
        account_id: AlpacaAccountId,
        auth: crate::core::AlpacaAuth,
    ) -> Result<Self, AlpacaWalletError> {
        let client = AlpacaWalletClient::new(base_url, account_id, auth)?;

        Ok(Self {
            client: Arc::new(client),
            polling_config: PollingConfig::default(),
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_client(
        client: AlpacaWalletClient,
        polling_config: Option<PollingConfig>,
    ) -> Self {
        Self {
            client: Arc::new(client),
            polling_config: polling_config.unwrap_or_default(),
        }
    }

    /// Initiates a withdrawal to a whitelisted address: the whitelist check
    /// of [`check_withdrawal_whitelist`](Self::check_withdrawal_whitelist),
    /// then the POST of [`submit_withdrawal`](Self::submit_withdrawal).
    ///
    /// The address must be whitelisted and approved before this call.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The address is not whitelisted and approved
    /// - The API call fails
    pub async fn initiate_withdrawal(
        &self,
        amount: Positive<Usdc>,
        asset: &TokenSymbol,
        to_address: &Address,
    ) -> Result<Transfer, AlpacaWalletError> {
        self.check_withdrawal_whitelist(asset, to_address).await?;
        self.submit_withdrawal(amount, asset, to_address).await
    }

    /// Reads the whitelist once and checks that `to_address` holds an
    /// approved entry for `asset`. Sends nothing that moves funds, so a
    /// failure here means no withdrawal was requested.
    ///
    /// # Errors
    ///
    /// `AddressNotWhitelisted` when no approved entry matches, or the error
    /// of the whitelist read.
    pub async fn check_withdrawal_whitelist(
        &self,
        asset: &TokenSymbol,
        to_address: &Address,
    ) -> Result<(), AlpacaWalletError> {
        let network = Network::new("ethereum");

        if self
            .client
            .is_address_whitelisted_and_approved(to_address, asset, &network)
            .await?
        {
            Ok(())
        } else {
            Err(AlpacaWalletError::AddressNotWhitelisted {
                address: *to_address,
                asset: asset.clone(),
                network,
            })
        }
    }

    /// Sends the withdrawal POST alone, without reading the whitelist first.
    /// Alpaca does not deduplicate withdrawals, so a failure without a
    /// definite 4xx answer may have left a withdrawal behind.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error.
    pub async fn submit_withdrawal(
        &self,
        amount: Positive<Usdc>,
        asset: &TokenSymbol,
        to_address: &Address,
    ) -> Result<Transfer, AlpacaWalletError> {
        transfer::initiate_withdrawal(&self.client, amount, asset, to_address).await
    }

    /// Polls a transfer until it reaches a terminal state (Complete or Failed).
    /// See [`poll_transfer_until_complete_with`].
    ///
    /// This method will retry transient errors and timeout after the configured duration.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The transfer times out
    /// - An invalid status regression is detected
    /// - The API call fails persistently
    pub async fn poll_transfer_until_complete(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<Transfer, AlpacaWalletError> {
        poll_transfer_until_complete_with(&*self.client, transfer_id, &self.polling_config).await
    }

    /// Polls a transfer until it is `Complete` and Alpaca reports its onchain
    /// tx hash. See [`poll_transfer_tx_hash_with`].
    ///
    /// # Errors
    ///
    /// As [`poll_transfer_tx_hash_with`].
    pub async fn poll_transfer_tx_hash(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<TxHash, AlpacaWalletError> {
        poll_transfer_tx_hash_with(&*self.client, transfer_id, &self.polling_config).await
    }

    /// Reads a transfer's current state once, with no polling.
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails.
    pub async fn get_transfer(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<TransferWithFees, AlpacaWalletError> {
        transfer::get_transfer_with_fees(&self.client, transfer_id).await
    }

    /// Polls for an incoming deposit by its onchain transaction hash. See
    /// [`poll_deposit_by_tx_hash_with`].
    ///
    /// Alpaca auto-detects incoming transfers to their funding wallet addresses.
    /// This method polls until the deposit is detected and reaches a terminal state.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The deposit times out (not detected within timeout)
    /// - The API call fails persistently
    pub async fn poll_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Transfer, AlpacaWalletError> {
        poll_deposit_by_tx_hash_with(&*self.client, tx_hash, &self.polling_config).await
    }

    /// Finds an incoming deposit by its on-chain transaction hash with a
    /// single query -- no polling deadline. `transfer recheck` uses this to
    /// verify whether a deposit the poller once gave up on has since settled
    /// at Alpaca: by recheck time the deposit either settled or it did not,
    /// so waiting adds nothing. Returns `None` when Alpaca has not detected
    /// the transfer, or when the hash matches an outgoing transfer (never a
    /// deposit).
    ///
    /// # Errors
    ///
    /// Returns an error if the API call fails.
    pub async fn find_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<Transfer>, AlpacaWalletError> {
        transfer::find_deposit_by_tx_hash(&self.client, tx_hash).await
    }

    /// Looks up the account's deposit address for `asset` on `network`.
    ///
    /// # Errors
    ///
    /// Returns the HTTP, parse, or address error.
    pub async fn get_wallet_address(
        &self,
        asset: &TokenSymbol,
        network: &Network,
    ) -> Result<Address, AlpacaWalletError> {
        self.client.get_wallet_address(asset, network).await
    }

    /// Requests a withdrawal whitelist entry for `address`.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error.
    pub async fn create_whitelist_entry(
        &self,
        address: &Address,
        asset: &TokenSymbol,
        network: &Network,
        travel_rule_info: &TravelRuleInfo,
    ) -> Result<whitelist::WhitelistEntry, AlpacaWalletError> {
        self.client
            .create_whitelist_entry(address, asset, network, travel_rule_info)
            .await
    }

    /// Deletes one whitelist entry by its Alpaca id.
    ///
    /// # Errors
    ///
    /// Returns the HTTP error.
    pub async fn delete_whitelist_entry(
        &self,
        whitelist_id: &str,
    ) -> Result<(), AlpacaWalletError> {
        self.client.delete_whitelist_entry(whitelist_id).await
    }

    /// Sets the Travel Rule info of one whitelist entry by its Alpaca id.
    ///
    /// # Errors
    ///
    /// Returns the HTTP error.
    pub async fn patch_whitelist_travel_rule(
        &self,
        whitelist_id: &str,
        travel_rule_info: &TravelRuleInfo,
    ) -> Result<(), AlpacaWalletError> {
        self.client
            .patch_whitelist_travel_rule(whitelist_id, travel_rule_info)
            .await
    }

    /// Removes all whitelist entries matching the given address, one
    /// [`delete_whitelist_entry`](Self::delete_whitelist_entry) per entry.
    ///
    /// Returns the entries that were deleted. Errors if no entries
    /// match the address. A failure stops at the first entry that fails, so
    /// the entries before it stay deleted.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error, or an error when no entry matches.
    pub async fn remove_whitelist_entries(
        &self,
        address: &Address,
    ) -> Result<Vec<whitelist::WhitelistEntry>, AlpacaWalletError> {
        let entries = self.client.get_whitelisted_addresses().await?;

        let matching: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.address == *address)
            .collect();

        if matching.is_empty() {
            return Err(AlpacaWalletError::NoWhitelistEntries { address: *address });
        }

        for entry in &matching {
            self.delete_whitelist_entry(&entry.id).await?;
        }

        Ok(matching)
    }

    /// Patches travel rule info on all existing whitelisted addresses, one
    /// [`patch_whitelist_travel_rule`](Self::patch_whitelist_travel_rule) per
    /// entry.
    ///
    /// Returns all whitelist entries that were patched. A failure stops at
    /// the first entry that fails, so the entries before it stay patched.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error.
    pub async fn patch_all_whitelist_travel_rules(
        &self,
        travel_rule_info: &TravelRuleInfo,
    ) -> Result<Vec<whitelist::WhitelistEntry>, AlpacaWalletError> {
        let entries = self.client.get_whitelisted_addresses().await?;

        for entry in &entries {
            self.patch_whitelist_travel_rule(&entry.id, travel_rule_info)
                .await
                .inspect_err(|err| {
                    error!(
                        target: "wallet",
                        whitelist_id = %entry.id,
                        address = %entry.address,
                        ?err,
                        "failed to patch travel rule on whitelist entry"
                    );
                })?;
        }

        Ok(entries)
    }

    /// Gets all whitelisted addresses for this account.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error.
    pub async fn get_whitelisted_addresses(
        &self,
    ) -> Result<Vec<whitelist::WhitelistEntry>, AlpacaWalletError> {
        self.client.get_whitelisted_addresses().await
    }

    /// Lists all transfers for this account.
    ///
    /// # Errors
    ///
    /// Returns the HTTP or parse error.
    pub async fn list_all_transfers(&self) -> Result<Vec<Transfer>, AlpacaWalletError> {
        transfer::list_all_transfers(&self.client).await
    }
}

/// Reads Alpaca directly through the service's own client.
impl WalletTransfers for AlpacaWalletService {
    fn get_transfer(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> impl Future<Output = Result<Transfer, AlpacaWalletError>> + Send {
        WalletTransfers::get_transfer(&*self.client, transfer_id)
    }

    fn find_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> impl Future<Output = Result<Option<Transfer>, AlpacaWalletError>> + Send {
        transfer::find_deposit_by_tx_hash(&self.client, tx_hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, fixed_bytes};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::time::Duration;
    use uuid::{Uuid, uuid};

    use crate::broker::AlpacaAccountId;

    use super::*;

    use crate::core::AlpacaAuth;
    use st0x_float_macro::float;

    const TEST_ACCOUNT_ID: AlpacaAccountId =
        AlpacaAccountId::new(uuid!("904837e3-3b76-47ec-b432-046db621571b"));

    fn create_test_service(server: &MockServer) -> AlpacaWalletService {
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key".to_string(),
                api_secret: "test_secret".to_string(),
            },
        )
        .unwrap();

        AlpacaWalletService::new_with_client(client, None)
    }

    #[tokio::test]
    async fn get_transfer_reads_the_requested_id_and_preserves_reported_fees() {
        let server = MockServer::start();
        let service = create_test_service(&server);
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let response = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200).json_body(json!({
                "id": transfer_id, "direction": "OUTGOING", "amount": "100",
                "chain": "ethereum", "asset": "USDC",
                "from_address": "0x0000000000000000000000000000000000000001",
                "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                "status": "COMPLETE", "created_at": "2024-01-01T00:00:00Z",
                "network_fee": "0.5", "fees": "0.25"
            }));
        });

        let transfer = service.get_transfer(&transfer_id).await.unwrap();
        assert_eq!(transfer.transfer.id, transfer_id);
        assert_eq!(
            transfer.reported_fees().unwrap(),
            Some(Usdc::new(float!(0.75)))
        );
        response.assert();
    }

    #[tokio::test]
    async fn test_initiate_withdrawal_not_whitelisted() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let asset = TokenSymbol::new("USDC");
        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");
        let amount = Positive::new(Usdc::new(float!(100))).unwrap();

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
    async fn test_initiate_withdrawal_pending_whitelist() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
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

        let asset = TokenSymbol::new("USDC");
        let amount = Positive::new(Usdc::new(float!(100))).unwrap();

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
    async fn test_initiate_withdrawal_approved() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
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
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/transfers");
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

        let asset = TokenSymbol::new("USDC");
        let amount = Positive::new(Usdc::new(float!(100))).unwrap();

        let result = service
            .initiate_withdrawal(amount, &asset, &to_address)
            .await
            .unwrap();

        assert_eq!(result.to, to_address);
        whitelist_mock.assert();
        transfer_mock.assert();
    }

    #[tokio::test]
    async fn submit_withdrawal_sends_one_post_and_reads_no_whitelist() {
        let server = MockServer::start();
        let service = create_test_service(&server);
        let to_address = address!("0x1234567890abcdef1234567890abcdef12345678");

        let whitelist_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
            then.status(200).json_body(json!([]));
        });
        let transfer_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/transfers");
            then.status(200).json_body(json!({
                "id": "550e8400-e29b-41d4-a716-446655440000",
                "direction": "OUTGOING", "amount": "100",
                "chain": "ethereum", "asset": "USDC",
                "from_address": "0x0000000000000000000000000000000000000001",
                "to_address": to_address.to_string(),
                "status": "PENDING", "created_at": "2024-01-01T00:00:00Z"
            }));
        });

        let transfer = service
            .submit_withdrawal(
                Positive::new(Usdc::new(float!(100))).unwrap(),
                &TokenSymbol::new("USDC"),
                &to_address,
            )
            .await
            .unwrap();

        assert_eq!(transfer.to, to_address);
        transfer_mock.assert_calls(1);
        whitelist_mock.assert_calls(0);
    }

    #[tokio::test]
    async fn test_poll_transfer_until_complete() {
        let server = MockServer::start();

        let polling_config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key".to_string(),
                api_secret: "test_secret".to_string(),
            },
        )
        .unwrap();

        let service = AlpacaWalletService::new_with_client(client, Some(polling_config));

        let transfer_id = Uuid::new_v4();

        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/transfers/{transfer_id}"
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

        let tid = transfer::AlpacaTransferId::from(transfer_id);
        let result = service.poll_transfer_until_complete(&tid).await.unwrap();

        assert_eq!(result.status, transfer::TransferStatus::Complete);
        status_mock.assert();
    }

    #[tokio::test]
    async fn test_remove_whitelist_entries_found() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
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
            when.method(DELETE).path(
                "/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists/wl-111",
            );
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
    async fn test_remove_whitelist_entries_not_found() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");
        let other = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
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
    async fn test_remove_whitelist_entries_multiple() {
        let server = MockServer::start();
        let service = create_test_service(&server);

        let target = address!("0x1234567890abcdef1234567890abcdef12345678");

        let list_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists");
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
            when.method(DELETE).path(
                "/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists/wl-aaa",
            );
            then.status(204);
        });

        let delete_bbb = server.mock(|when, then| {
            when.method(DELETE).path(
                "/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists/wl-bbb",
            );
            then.status(204);
        });

        let removed = service.remove_whitelist_entries(&target).await.unwrap();

        assert_eq!(removed.len(), 2);
        list_mock.assert();
        delete_aaa.assert();
        delete_bbb.assert();
    }

    #[tokio::test]
    async fn traffic_of_answered_and_failed_wallet_calls_is_collected() {
        let server = MockServer::start();
        let service = create_test_service(&server);
        let target = address!("0x1234567890abcdef1234567890abcdef12345678");
        let whitelists = "/v1/accounts/904837e3-3b76-47ec-b432-046db621571b/wallets/whitelists";

        server.mock(|when, then| {
            when.method(GET).path(whitelists);
            then.status(200)
                .header(crate::request_id::ALPACA_REQUEST_ID_HEADER, "req-list")
                .json_body(json!([
                    {
                        "id": "wl-aaa", "address": target.to_string(), "asset": "USDC",
                        "chain": "ethereum", "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    },
                    {
                        "id": "wl-bbb", "address": target.to_string(), "asset": "USDC",
                        "chain": "ethereum", "status": "APPROVED",
                        "created_at": "2024-01-01T00:00:00Z"
                    }
                ]));
        });
        server.mock(|when, then| {
            when.method(DELETE).path(format!("{whitelists}/wl-aaa"));
            then.status(204).header(
                crate::request_id::ALPACA_REQUEST_ID_HEADER,
                "req-delete-aaa",
            );
        });
        server.mock(|when, then| {
            when.method(DELETE).path(format!("{whitelists}/wl-bbb"));
            then.status(404)
                .header(
                    crate::request_id::ALPACA_REQUEST_ID_HEADER,
                    "req-delete-bbb",
                )
                .body("not found");
        });

        let (result, traffic) =
            crate::request_id::collect(service.remove_whitelist_entries(&target)).await;

        assert!(
            matches!(
                result,
                Err(AlpacaWalletError::ApiError { status, .. })
                    if status == reqwest::StatusCode::NOT_FOUND
            ),
            "{result:?}"
        );
        assert_eq!(
            traffic.request_ids,
            ["req-list", "req-delete-aaa", "req-delete-bbb"]
        );
        assert_eq!(traffic.last_status, Some(404));
    }

    /// The service lookup is the tolerant scan: a row on another chain, a row
    /// without a direction and an unparsable row carrying another EVM hash
    /// are all invisible, so they cannot fail a lookup they do not match.
    #[tokio::test]
    async fn find_deposit_by_tx_hash_ignores_rows_it_does_not_match() {
        let server = MockServer::start();
        let service = create_test_service(&server);
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();
        let evm_row = |id: Uuid, hash: TxHash, amount: &str| {
            json!({
                "id": id, "direction": "INCOMING", "amount": amount,
                "chain": "ethereum", "asset": "USDC",
                "from_address": "0x0000000000000000000000000000000000000001",
                "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                "status": "COMPLETE", "tx_hash": hash,
                "created_at": "2024-01-01T00:00:00Z"
            })
        };
        let transfers = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200).json_body(json!([
                {
                    "id": Uuid::new_v4(), "direction": "INCOMING", "amount": "2.5",
                    "chain": "solana", "asset": "SOL",
                    "from_address": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                    "to_address": "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d",
                    "status": "COMPLETE",
                    "tx_hash": "5wHu1qwD4kKKyN1EEPBLRZ8hUvmCwF9zPSNdPCVBLcNq",
                    "created_at": "2024-01-01T00:00:00Z"
                },
                {
                    "id": Uuid::new_v4(), "amount": "500", "chain": "ethereum",
                    "asset": "USDC", "status": "COMPLETE", "tx_hash": tx_hash,
                    "created_at": "2024-01-01T00:00:00Z"
                },
                evm_row(Uuid::new_v4(), TxHash::repeat_byte(0x11), "not a number"),
                evm_row(transfer_id, tx_hash, "100"),
            ]));
        });

        let found = service
            .find_deposit_by_tx_hash(&tx_hash)
            .await
            .unwrap()
            .expect("the matching transfer behind the rows it does not match");
        let missing = service
            .find_deposit_by_tx_hash(&TxHash::repeat_byte(0x22))
            .await
            .unwrap();

        assert_eq!(found.id, transfer_id.into());
        assert_eq!(found.tx, Some(tx_hash));
        assert_eq!(found.direction, TransferDirection::Incoming);
        assert_eq!(missing.map(|transfer| transfer.id), None);
        transfers.assert_calls(2);
    }
}
