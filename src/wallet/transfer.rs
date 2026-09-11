//! Alpaca Broker API crypto transfer types and operations.
//!
//! Provides withdrawal initiation and transfer-status lookup. Transfers
//! progress through Pending -> Processing -> Complete/Failed.

use alloy_primitives::{Address, TxHash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_finance::{EmptySymbolError, Positive, Symbol, Usdc};
use uuid::Uuid;

use super::{AlpacaWalletError, get_json, post_json};
use crate::core::{AlpacaClient, AlpacaError};

/// Crypto asset symbol exactly as Alpaca encodes it on the wire (e.g.
/// `"USDC"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenSymbol(pub Symbol);

impl TokenSymbol {
    /// # Errors
    ///
    /// Returns [`EmptySymbolError`] when the symbol is empty or whitespace-only.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptySymbolError> {
        Symbol::new(value).map(Self)
    }
}

impl TryFrom<String> for TokenSymbol {
    type Error = EmptySymbolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl AsRef<str> for TokenSymbol {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for TokenSymbol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Alpaca-assigned identifier for a crypto wallet transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlpacaTransferId(pub Uuid);

impl From<Uuid> for AlpacaTransferId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl std::fmt::Display for AlpacaTransferId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TransferDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TransferStatus {
    Pending,
    Processing,
    Complete,
    Failed,
}

impl TransferStatus {
    /// Whether the transfer is still in flight (not yet complete or failed).
    #[must_use]
    pub fn is_pending(self) -> bool {
        match self {
            Self::Pending | Self::Processing => true,
            Self::Complete | Self::Failed => false,
        }
    }
}

/// Transfer response from the Alpaca Crypto Wallets API.
#[derive(Debug, Clone, Deserialize)]
pub struct Transfer {
    pub id: AlpacaTransferId,
    #[serde(rename = "tx_hash", default)]
    pub tx: Option<TxHash>,
    pub direction: TransferDirection,
    pub amount: Usdc,
    pub chain: String,
    pub asset: TokenSymbol,
    #[serde(rename = "from_address")]
    pub from: Address,
    #[serde(rename = "to_address")]
    pub to: Address,
    pub status: TransferStatus,
    pub created_at: DateTime<Utc>,
}

/// Blockchain network a wallet operation targets, normalized to lowercase
/// (Alpaca accepts `"ethereum"` on requests but echoes `"ETH"` in some
/// responses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Network(String);

impl<'de> serde::Deserialize<'de> for Network {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::new(raw))
    }
}

impl Network {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().to_lowercase())
    }
}

impl From<String> for Network {
    fn from(value: String) -> Self {
        Self(value.to_lowercase())
    }
}

impl AsRef<str> for Network {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Serialize)]
struct WithdrawalRequest<'a> {
    amount: Usdc,
    asset: &'a TokenSymbol,
    address: String,
}

pub(super) async fn initiate_withdrawal(
    client: &AlpacaClient,
    amount: Positive<Usdc>,
    asset: &TokenSymbol,
    address: &Address,
) -> Result<Transfer, AlpacaWalletError> {
    let request = WithdrawalRequest {
        amount: amount.inner(),
        asset,
        // None = standard EIP-55 checksum (no chain-specific EIP-1191
        // encoding). Alpaca requires checksummed addresses for whitelist
        // matching. Fine for now since this system only handles Ethereum
        // mainnet.
        address: address.to_checksum(None),
    };

    let url = format!(
        "{}/v1/accounts/{}/wallets/transfers",
        client.base_url(),
        client.account_id()
    );

    post_json(client, &url, &request).await
}

pub(super) async fn get_transfer_status(
    client: &AlpacaClient,
    transfer_id: &AlpacaTransferId,
) -> Result<Transfer, AlpacaWalletError> {
    // Use the documented by-id endpoint for a single transfer:
    // https://docs.alpaca.markets/us/reference/getcryptofundingtransfer-1.
    // The list endpoint returns an account-wide array and has no documented
    // transfer_id filter, so status polling must not depend on client-side
    // filtering of a potentially capped transfer list.
    let url = format!(
        "{}/v1/accounts/{}/wallets/transfers/{}",
        client.base_url(),
        client.account_id(),
        transfer_id
    );

    get_json(client, &url).await.map_err(|error| match error {
        AlpacaWalletError::Alpaca(AlpacaError::Api {
            status_code: 404, ..
        }) => AlpacaWalletError::TransferNotFound {
            transfer_id: *transfer_id,
        },
        error => error,
    })
}

/// Lists all transfers for the account.
pub(super) async fn list_all_transfers(
    client: &AlpacaClient,
) -> Result<Vec<Transfer>, AlpacaWalletError> {
    let url = format!(
        "{}/v1/accounts/{}/wallets/transfers",
        client.base_url(),
        client.account_id()
    );

    get_json(client, &url).await
}

/// Finds an incoming deposit by its transaction hash.
///
/// Fetches all transfers and filters by `tx_hash`. Returns the first match
/// or `None` if no transfer with that tx hash exists.
pub(super) async fn find_deposit_by_tx_hash(
    client: &AlpacaClient,
    tx_hash: &TxHash,
) -> Result<Option<Transfer>, AlpacaWalletError> {
    let transfers = list_all_transfers(client).await?;

    Ok(transfers.into_iter().find(|transfer| {
        transfer.direction == TransferDirection::Incoming && transfer.tx.as_ref() == Some(tx_hash)
    }))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, fixed_bytes};
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::wallet::{TEST_ACCOUNT_ID, test_client};

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid test token symbol: {error}"))
    }

    fn usdc(value: &str) -> Usdc {
        value
            .parse()
            .unwrap_or_else(|error| panic!("invalid test USDC amount: {error}"))
    }

    fn positive_usdc(value: &str) -> Positive<Usdc> {
        Positive::new(usdc(value))
            .unwrap_or_else(|error| panic!("non-positive test USDC amount: {error}"))
    }

    #[tokio::test]
    async fn initiate_withdrawal_sends_checksummed_address_and_string_amount() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let to_address = address!("0xbd41F40D91eE4E816Ada1Aa842e94aEb6B6385a6");

        let withdrawal_mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"))
                .json_body(json!({
                    "amount": "100.5",
                    "asset": "USDC",
                    // Alpaca requires EIP-55 checksummed addresses for
                    // whitelist matching.
                    "address": "0xbd41F40D91eE4E816Ada1Aa842e94aEb6B6385a6"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": transfer_id,
                    "direction": "OUTGOING",
                    "amount": "100.5",
                    "usd_value": "100.48",
                    "chain": "ETH",
                    "asset": "USDC",
                    "from_address": "0xabcdef1234567890abcdef1234567890abcdef12",
                    "to_address": to_address.to_string(),
                    "status": "PENDING",
                    "tx_hash": null,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let client = test_client(server.base_url());
        let amount = positive_usdc("100.5");
        let asset = token_symbol("USDC");

        let transfer = initiate_withdrawal(&client, amount, &asset, &to_address)
            .await
            .unwrap();

        assert_eq!(transfer.id, AlpacaTransferId::from(transfer_id));
        assert_eq!(transfer.direction, TransferDirection::Outgoing);
        assert_eq!(transfer.amount, usdc("100.5"));
        assert_eq!(transfer.asset.as_ref(), "USDC");
        assert_eq!(transfer.to, to_address);
        assert_eq!(transfer.status, TransferStatus::Pending);

        withdrawal_mock.assert();
    }

    #[test]
    fn initiate_withdrawal_rejects_zero_amount() {
        let error = Positive::new(usdc("0")).unwrap_err();

        assert!(matches!(
            error,
            st0x_finance::NotPositive::Constraint { value } if value == usdc("0")
        ));
    }

    #[test]
    fn initiate_withdrawal_rejects_negative_amount() {
        let amount = usdc("-100");
        let error = Positive::new(amount).unwrap_err();

        assert!(matches!(
            error,
            st0x_finance::NotPositive::Constraint { value } if value == amount
        ));
    }

    #[tokio::test]
    async fn initiate_withdrawal_invalid_asset() {
        let server = MockServer::start();
        let withdrawal_mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(400)
                .header("content-type", "application/json")
                .json_body(json!({
                    "message": "Invalid asset"
                }));
        });

        let client = test_client(server.base_url());
        let amount = positive_usdc("100");
        let asset = token_symbol("INVALID");
        let addr = address!("0x1234567890abcdef1234567890abcdef12345678");

        let error = initiate_withdrawal(&client, amount, &asset, &addr)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 400,
                ..
            })
        ));

        withdrawal_mock.assert();
    }

    #[tokio::test]
    async fn initiate_withdrawal_api_error() {
        let server = MockServer::start();
        let withdrawal_mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(500).body("Internal Server Error");
        });

        let client = test_client(server.base_url());
        let amount = positive_usdc("100");
        let asset = token_symbol("USDC");
        let addr = address!("0x1234567890abcdef1234567890abcdef12345678");

        let error = initiate_withdrawal(&client, amount, &asset, &addr)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));

        withdrawal_mock.assert();
    }

    fn transfer_status_mock_body(
        transfer_id: Uuid,
        status: &str,
        tx_hash: Option<&str>,
    ) -> serde_json::Value {
        json!({
            "id": transfer_id,
            "direction": "OUTGOING",
            "amount": "100.0",
            "usd_value": "99.98",
            "chain": "ETH",
            "asset": "USDC",
            "from_address": "0xabcdef1234567890abcdef1234567890abcdef12",
            "to_address": "0x1234567890abcdef1234567890abcdef12345678",
            "status": status,
            "tx_hash": tx_hash,
            "created_at": "2024-01-01T00:00:00Z",
            "network_fee": "0.5",
            "fees": "0"
        })
    }

    #[tokio::test]
    async fn get_transfer_status_pending() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(transfer_status_mock_body(transfer_id, "PENDING", None));
        });

        let client = test_client(server.base_url());

        let result = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Pending);
        assert_eq!(result.id, AlpacaTransferId::from(transfer_id));

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_processing() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(transfer_status_mock_body(
                    transfer_id,
                    "PROCESSING",
                    Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"),
                ));
        });

        let client = test_client(server.base_url());

        let result = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Processing);
        assert_eq!(
            result.tx,
            Some(fixed_bytes!(
                "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
            ))
        );

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_complete() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(transfer_status_mock_body(
                    transfer_id,
                    "COMPLETE",
                    Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"),
                ));
        });

        let client = test_client(server.base_url());

        let result = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Complete);

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_failed() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(transfer_status_mock_body(transfer_id, "FAILED", None));
        });

        let client = test_client(server.base_url());

        let result = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Failed);

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_not_found() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({ "message": "transfer not found" }));
        });

        let client = test_client(server.base_url());

        let error = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap_err();

        assert!(matches!(error, AlpacaWalletError::TransferNotFound { .. }));

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_api_error() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(500).body("Internal Server Error");
        });

        let client = test_client(server.base_url());

        let error = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));

        status_mock.assert();
    }

    #[tokio::test]
    async fn get_transfer_status_malformed_json() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .body("not valid json");
        });

        let client = test_client(server.base_url());

        let error = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Parse { .. })
        ));

        status_mock.assert();
    }

    /// Regression test: status polling must use Alpaca's by-id endpoint
    /// rather than the list endpoint, so an account-wide response cap cannot
    /// hide an older in-flight withdrawal.
    #[tokio::test]
    async fn get_transfer_status_uses_by_id_endpoint() {
        let server = MockServer::start();
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
                    "usd_value": "99.98",
                    "chain": "ETH",
                    "asset": "USDC",
                    "from_address": "0xA0D2C7210D7e2112A4F7888B8658CB579226dB3B",
                    "to_address": "0x5A379C330c84Af97864507FfeA4c23aEAF3476d9",
                    "status": "PROCESSING",
                    "created_at": "2024-12-26T20:43:29Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let client = test_client(server.base_url());

        let transfer = get_transfer_status(&client, &AlpacaTransferId::from(transfer_id))
            .await
            .unwrap();

        assert_eq!(
            transfer.id,
            AlpacaTransferId::from(transfer_id),
            "status polling must request the transfer by ID"
        );
        assert_eq!(
            transfer.amount,
            usdc("100"),
            "status polling must parse the by-id transfer payload"
        );
        assert_eq!(transfer.direction, TransferDirection::Outgoing);
        assert_eq!(transfer.status, TransferStatus::Processing);

        status_mock.assert();
    }

    #[test]
    fn network_normalizes_to_lowercase() {
        let network = Network::new("Ethereum");
        assert_eq!(network.as_ref(), "ethereum");
    }

    #[test]
    fn network_from_string_normalizes() {
        let network = Network::from("EtHeReuM".to_string());
        assert_eq!(network.as_ref(), "ethereum");
    }

    #[tokio::test]
    async fn find_deposit_by_tx_hash_ignores_outgoing_transfer_with_same_hash() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": Uuid::new_v4(),
                        "direction": "OUTGOING",
                        "amount": "100",
                        "usd_value": "99.98",
                        "chain": "ETH",
                        "asset": "USDC",
                        "from_address": "0xabcdef1234567890abcdef1234567890abcdef12",
                        "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                        "status": "COMPLETE",
                        "tx_hash": tx_hash,
                        "created_at": "2024-01-01T00:00:00Z",
                        "network_fee": "0",
                        "fees": "0"
                    },
                    {
                        "id": transfer_id,
                        "direction": "INCOMING",
                        "amount": "500",
                        "usd_value": "499.90",
                        "chain": "ETH",
                        "asset": "USDC",
                        "from_address": "0x9999999999999999999999999999999999999999",
                        "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                        "status": "COMPLETE",
                        "tx_hash": tx_hash,
                        "created_at": "2024-01-02T00:00:00Z",
                        "network_fee": "0.5",
                        "fees": "0"
                    }
                ]));
        });

        let client = test_client(server.base_url());

        let transfer = find_deposit_by_tx_hash(&client, &tx_hash)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(transfer.id, AlpacaTransferId::from(transfer_id));
        assert_eq!(transfer.tx, Some(tx_hash));
        assert_eq!(transfer.status, TransferStatus::Complete);

        transfers_mock.assert();
    }

    #[tokio::test]
    async fn find_deposit_by_tx_hash_not_found() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "id": Uuid::new_v4(),
                        "direction": "OUTGOING",
                        "amount": "100",
                        "usd_value": "99.98",
                        "chain": "ETH",
                        "asset": "USDC",
                        "from_address": "0xabcdef1234567890abcdef1234567890abcdef12",
                        "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                        "status": "COMPLETE",
                        "tx_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
                        "created_at": "2024-01-01T00:00:00Z",
                        "network_fee": "0",
                        "fees": "0"
                    }
                ]));
        });

        let client = test_client(server.base_url());

        let result = find_deposit_by_tx_hash(&client, &tx_hash).await.unwrap();

        assert_eq!(result.map(|transfer| transfer.id), None);

        transfers_mock.assert();
    }

    #[tokio::test]
    async fn find_deposit_by_tx_hash_empty_list() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let client = test_client(server.base_url());

        let result = find_deposit_by_tx_hash(&client, &tx_hash).await.unwrap();

        assert_eq!(result.map(|transfer| transfer.id), None);

        transfers_mock.assert();
    }

    #[tokio::test]
    async fn find_deposit_by_tx_hash_api_error() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(500).body("Internal Server Error");
        });

        let client = test_client(server.base_url());

        let error = find_deposit_by_tx_hash(&client, &tx_hash)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));

        transfers_mock.assert();
    }

    #[test]
    fn pending_and_processing_are_pending_statuses() {
        assert!(TransferStatus::Pending.is_pending());
        assert!(TransferStatus::Processing.is_pending());
    }

    #[test]
    fn complete_and_failed_are_not_pending() {
        assert!(!TransferStatus::Complete.is_pending());
        assert!(!TransferStatus::Failed.is_pending());
    }

    #[test]
    fn malformed_decimal_string_fails_deserialization() {
        let malformed = json!({
            "id": Uuid::new_v4(),
            "direction": "OUTGOING",
            "amount": "not_a_number",
            "usd_value": "100.0",
            "chain": "BASE",
            "asset": "USDC",
            "from_address": Address::ZERO,
            "to_address": Address::ZERO,
            "status": "COMPLETE",
            "created_at": "2025-01-01T00:00:00Z",
            "network_fee": "0.001",
            "fees": "0.0"
        });

        assert!(serde_json::from_value::<Transfer>(malformed).is_err());
    }
}
