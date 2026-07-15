//! Transfer status polling with exponential backoff for Alpaca Broker API
//! wallet operations.
//!
//! Provides `poll_transfer_status` and `poll_deposit_by_tx_hash` which poll
//! until a transfer reaches a terminal state (Complete/Failed) or times out.

use alloy_primitives::TxHash;
use backon::{ExponentialBuilder, Retryable};
use std::time::Duration;
use tokio::time::{Instant, sleep};

use super::AlpacaWalletError;
use super::transfer::{
    AlpacaTransferId, Transfer, TransferStatus, find_deposit_by_tx_hash, get_transfer_status,
};
use crate::core::{AlpacaClient, AlpacaError};

/// Timing parameters for transfer/deposit polling.
///
/// `interval` spaces successive status polls; `timeout` bounds the whole
/// wait. The `*_retry_*` fields configure the exponential backoff applied
/// to transient (5xx) failures of an individual poll request.
#[derive(Debug, Clone)]
pub struct PollingConfig {
    pub interval: Duration,
    pub timeout: Duration,
    pub max_retries: usize,
    pub min_retry_delay: Duration,
    pub max_retry_delay: Duration,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_mins(30),
            max_retries: 10,
            min_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_mins(1),
        }
    }
}

pub(super) async fn poll_transfer_status(
    client: &AlpacaClient,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    let start = Instant::now();
    let mut last_status = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        check_timeout(&start, config.timeout, *transfer_id)?;

        let transfer = (|| async { get_transfer_status(client, transfer_id).await })
            .retry(retry_strategy)
            .when(is_transient_server_error)
            .await?;

        if let Some(previous) = last_status
            && is_status_regression(previous, transfer.status)
        {
            return Err(AlpacaWalletError::InvalidStatusTransition {
                transfer_id: *transfer_id,
                previous,
                next: transfer.status,
            });
        }

        match transfer.status {
            TransferStatus::Complete | TransferStatus::Failed => return Ok(transfer),
            TransferStatus::Pending | TransferStatus::Processing => {
                last_status = Some(transfer.status);
                sleep(config.interval).await;
            }
        }
    }
}

/// Polls for a deposit transfer matching the given tx hash until it's
/// detected and reaches a terminal state.
///
/// Used to wait for Alpaca to detect an incoming deposit by its on-chain
/// tx hash.
pub(super) async fn poll_deposit_by_tx_hash(
    client: &AlpacaClient,
    tx_hash: &TxHash,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    let start = Instant::now();
    let mut last_status: Option<TransferStatus> = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        check_deposit_timeout(&start, config.timeout, *tx_hash)?;

        let maybe_transfer = (|| async { find_deposit_by_tx_hash(client, tx_hash).await })
            .retry(retry_strategy)
            .when(is_transient_server_error)
            .await?;

        let Some(transfer) = maybe_transfer else {
            sleep(config.interval).await;
            continue;
        };

        if let Some(previous) = last_status
            && is_status_regression(previous, transfer.status)
        {
            return Err(AlpacaWalletError::InvalidDepositTransition {
                tx_hash: *tx_hash,
                previous,
                next: transfer.status,
            });
        }

        match transfer.status {
            TransferStatus::Complete | TransferStatus::Failed => return Ok(transfer),
            TransferStatus::Pending | TransferStatus::Processing => {
                last_status = Some(transfer.status);
                sleep(config.interval).await;
            }
        }
    }
}

/// Only 5xx API responses are retried within a single poll; everything else
/// (4xx, parse failures, wallet invariants) is surfaced immediately.
fn is_transient_server_error(error: &AlpacaWalletError) -> bool {
    matches!(
        error,
        AlpacaWalletError::Alpaca(AlpacaError::Api {
            status_code: 500..=599,
            ..
        })
    )
}

/// A regression is any move backwards (Processing -> Pending) or any move
/// out of a terminal state.
fn is_status_regression(previous: TransferStatus, next: TransferStatus) -> bool {
    match (previous, next) {
        (TransferStatus::Processing, TransferStatus::Pending)
        | (TransferStatus::Complete | TransferStatus::Failed, _) => true,
        (TransferStatus::Pending | TransferStatus::Processing, _) => false,
    }
}

fn check_timeout(
    start: &Instant,
    timeout: Duration,
    transfer_id: AlpacaTransferId,
) -> Result<(), AlpacaWalletError> {
    let elapsed = start.elapsed();

    if elapsed >= timeout {
        return Err(AlpacaWalletError::TransferTimeout {
            transfer_id,
            elapsed,
        });
    }

    Ok(())
}

fn check_deposit_timeout(
    start: &Instant,
    timeout: Duration,
    tx_hash: TxHash,
) -> Result<(), AlpacaWalletError> {
    let elapsed = start.elapsed();

    if elapsed >= timeout {
        return Err(AlpacaWalletError::DepositTimeout { tx_hash, elapsed });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::fixed_bytes;
    use httpmock::prelude::*;
    use serde_json::json;
    use std::sync::Arc;
    use uuid::Uuid;

    use super::*;
    use crate::wallet::{TEST_ACCOUNT_ID, test_client};

    fn fast_polling_config() -> PollingConfig {
        PollingConfig {
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn poll_transfer_returns_complete() {
        let server = MockServer::start();

        let transfer_id = Uuid::new_v4();

        let complete_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": transfer_id,
                    "direction": "OUTGOING",
                    "amount": "100.0",
                    "usd_value": "100.0",
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

        let client = test_client(server.base_url());
        let config = fast_polling_config();

        let result = poll_transfer_status(&client, &AlpacaTransferId::from(transfer_id), &config)
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Complete);

        complete_mock.assert();
    }

    #[tokio::test]
    async fn poll_transfer_returns_failed() {
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
                    "amount": "100.0",
                    "usd_value": "100.0",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "FAILED",
                    "tx_hash": null,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0",
                    "fees": "0"
                }));
        });

        let client = test_client(server.base_url());
        let config = fast_polling_config();

        let result = poll_transfer_status(&client, &AlpacaTransferId::from(transfer_id), &config)
            .await
            .unwrap();

        assert_eq!(result.status, TransferStatus::Failed);

        status_mock.assert();
    }

    #[tokio::test]
    async fn poll_transfer_times_out_while_pending() {
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
                    "amount": "100.0",
                    "usd_value": "100.0",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "PENDING",
                    "tx_hash": null,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let client = test_client(server.base_url());

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(500),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_transfer_status(&client, &AlpacaTransferId::from(transfer_id), &config)
            .await
            .unwrap_err();

        assert!(matches!(error, AlpacaWalletError::TransferTimeout { .. }));

        assert!(status_mock.calls() >= 2);
    }

    #[tokio::test]
    async fn poll_transfer_retries_on_5xx() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let error_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(503).body("Service Unavailable");
        });

        let client = test_client(server.base_url());

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(10),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_transfer_status(&client, &AlpacaTransferId::from(transfer_id), &config)
            .await
            .unwrap_err();

        assert!(
            error_mock.calls() >= 2,
            "Expected at least one retry attempt"
        );
        assert!(matches!(
            error,
            AlpacaWalletError::Alpaca(AlpacaError::Api {
                status_code: 503,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn poll_transfer_rejects_status_regression() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let client = Arc::new(test_client(server.base_url()));

        let mut processing_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": transfer_id,
                    "direction": "OUTGOING",
                    "amount": "100.0",
                    "usd_value": "100.0",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "PROCESSING",
                    "tx_hash": "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let config = fast_polling_config();

        let client_clone = Arc::clone(&client);
        let transfer_id_clone = AlpacaTransferId::from(transfer_id);
        let poll_handle = tokio::spawn(async move {
            poll_transfer_status(&client_clone, &transfer_id_clone, &config).await
        });

        while processing_mock.calls() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        processing_mock.delete();

        let pending_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": transfer_id,
                    "direction": "OUTGOING",
                    "amount": "100.0",
                    "usd_value": "100.0",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x0000000000000000000000000000000000000001",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "PENDING",
                    "tx_hash": null,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0.5",
                    "fees": "0"
                }));
        });

        let error = poll_handle.await.unwrap().unwrap_err();

        assert!(matches!(
            error,
            AlpacaWalletError::InvalidStatusTransition {
                previous: TransferStatus::Processing,
                next: TransferStatus::Pending,
                ..
            }
        ));

        pending_mock.assert();
    }

    #[test]
    fn processing_to_pending_is_regression() {
        assert!(is_status_regression(
            TransferStatus::Processing,
            TransferStatus::Pending
        ));
    }

    #[test]
    fn complete_to_any_is_regression() {
        assert!(is_status_regression(
            TransferStatus::Complete,
            TransferStatus::Pending
        ));
        assert!(is_status_regression(
            TransferStatus::Complete,
            TransferStatus::Processing
        ));
        assert!(is_status_regression(
            TransferStatus::Complete,
            TransferStatus::Failed
        ));
    }

    #[test]
    fn failed_to_any_is_regression() {
        assert!(is_status_regression(
            TransferStatus::Failed,
            TransferStatus::Pending
        ));
        assert!(is_status_regression(
            TransferStatus::Failed,
            TransferStatus::Processing
        ));
        assert!(is_status_regression(
            TransferStatus::Failed,
            TransferStatus::Complete
        ));
    }

    #[test]
    fn forward_transitions_are_not_regressions() {
        assert!(!is_status_regression(
            TransferStatus::Pending,
            TransferStatus::Processing
        ));
        assert!(!is_status_regression(
            TransferStatus::Pending,
            TransferStatus::Complete
        ));
        assert!(!is_status_regression(
            TransferStatus::Pending,
            TransferStatus::Failed
        ));
        assert!(!is_status_regression(
            TransferStatus::Processing,
            TransferStatus::Complete
        ));
        assert!(!is_status_regression(
            TransferStatus::Processing,
            TransferStatus::Failed
        ));
    }

    #[test]
    fn same_status_is_not_regression() {
        assert!(!is_status_regression(
            TransferStatus::Pending,
            TransferStatus::Pending
        ));
        assert!(!is_status_regression(
            TransferStatus::Processing,
            TransferStatus::Processing
        ));
    }

    #[tokio::test]
    async fn poll_deposit_by_tx_hash_found_immediately() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": transfer_id,
                    "direction": "INCOMING",
                    "amount": "500",
                    "usd_value": "500",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x9999999999999999999999999999999999999999",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "COMPLETE",
                    "tx_hash": tx_hash,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0",
                    "fees": "0"
                }]));
        });

        let client = test_client(server.base_url());
        let config = fast_polling_config();

        let transfer = poll_deposit_by_tx_hash(&client, &tx_hash, &config)
            .await
            .unwrap();

        assert_eq!(transfer.status, TransferStatus::Complete);
        assert_eq!(transfer.tx, Some(tx_hash));

        transfers_mock.assert();
    }

    #[tokio::test]
    async fn poll_deposit_by_tx_hash_not_found_then_found() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();

        let client = Arc::new(test_client(server.base_url()));

        let mut empty_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let config = fast_polling_config();

        let client_clone = Arc::clone(&client);
        let poll_handle =
            tokio::spawn(
                async move { poll_deposit_by_tx_hash(&client_clone, &tx_hash, &config).await },
            );

        while empty_mock.calls() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        empty_mock.delete();

        let found_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": transfer_id,
                    "direction": "INCOMING",
                    "amount": "500",
                    "usd_value": "500",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x9999999999999999999999999999999999999999",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "COMPLETE",
                    "tx_hash": tx_hash,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0",
                    "fees": "0"
                }]));
        });

        let transfer = poll_handle.await.unwrap().unwrap();
        assert_eq!(transfer.status, TransferStatus::Complete);
        found_mock.assert();
    }

    #[tokio::test]
    async fn poll_deposit_by_tx_hash_times_out_when_never_detected() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");

        let empty_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let client = test_client(server.base_url());

        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(50),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_deposit_by_tx_hash(&client, &tx_hash, &config)
            .await
            .unwrap_err();

        assert!(
            matches!(error, AlpacaWalletError::DepositTimeout { .. }),
            "Expected DepositTimeout error, got: {error:?}"
        );

        assert!(
            empty_mock.calls() >= 1,
            "Expected at least one poll attempt"
        );
    }

    #[tokio::test]
    async fn poll_deposit_by_tx_hash_found_failed() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();

        let transfers_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([{
                    "id": transfer_id,
                    "direction": "INCOMING",
                    "amount": "500",
                    "usd_value": "500",
                    "chain": "ethereum",
                    "asset": "USDC",
                    "from_address": "0x9999999999999999999999999999999999999999",
                    "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "status": "FAILED",
                    "tx_hash": tx_hash,
                    "created_at": "2024-01-01T00:00:00Z",
                    "network_fee": "0",
                    "fees": "0"
                }]));
        });

        let client = test_client(server.base_url());
        let config = fast_polling_config();

        let transfer = poll_deposit_by_tx_hash(&client, &tx_hash, &config)
            .await
            .unwrap();

        assert_eq!(transfer.status, TransferStatus::Failed);

        transfers_mock.assert();
    }
}
