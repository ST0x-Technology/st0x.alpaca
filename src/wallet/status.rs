//! Transfer status polling with exponential backoff for
//! Alpaca Broker API wallet operations.
//!
//! Provides [`poll_transfer_until_complete_with`],
//! [`poll_transfer_tx_hash_with`] and [`poll_deposit_by_tx_hash_with`],
//! which poll a [`WalletTransfers`] until a transfer reaches a terminal state
//! (Complete/Failed) or times out.

use alloy_primitives::TxHash;
use backon::{ExponentialBuilder, Retryable};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};
use tracing::{info, warn};

use super::client::AlpacaWalletError;
use super::transfer::{AlpacaTransferId, Transfer, TransferStatus, WalletTransfers};
use crate::core::{Permanence, response_status_permanence};

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
            timeout: Duration::from_secs(30 * 60),
            max_retries: 10,
            min_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
        }
    }
}

/// Polls a transfer until it reaches a terminal state (Complete or Failed).
///
/// Server errors (5xx) and transient gateway hop failures are retried with
/// exponential backoff; any other read error ends the poll.
///
/// # Errors
///
/// - `TransferTimeout` once `config.timeout` passes.
/// - `InvalidStatusTransition` when the status moves backwards.
/// - The read error once retries run out, or at once when it is not
///   retried.
pub async fn poll_transfer_until_complete_with(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    info!(target: "wallet", %transfer_id, timeout = ?config.timeout, "Polling transfer status");

    let start = Instant::now();
    let mut last_status = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        check_timeout(&start, config.timeout, *transfer_id)?;

        let transfer = (|| transfers.get_transfer(transfer_id))
            .retry(retry_strategy)
            .when(retryable_poll_read_error)
            .await?;

        validate_and_log_status_change(*transfer_id, last_status, &transfer)?;

        match transfer.status {
            TransferStatus::Complete | TransferStatus::Failed => {
                log_transfer_final_status(*transfer_id, transfer.status);
                return Ok(transfer);
            }
            TransferStatus::Pending | TransferStatus::Processing => {
                last_status = Some(transfer.status);
                sleep(config.interval).await;
            }
        }
    }
}

/// Polls a transfer until it is `Complete` and Alpaca reports its onchain tx
/// hash. A hash on a transfer that is not yet `Complete` is ignored.
///
/// # Errors
///
/// - `TransferTimeout` if no completed hash is reported within the polling
///   timeout. Transient read failures (5xx, 408, 429, transport errors,
///   transient gateway hop failures, auth errors that are not deterministic)
///   are retried until then, honoring `Retry-After`.
/// - `TransferFailed` or `FailedTransferHasTx` as soon as the transfer is
///   `Failed`.
/// - Permanent read errors, returned without retry: `TransferNotFound`,
///   other HTTP statuses that do not clear on their own, deterministic auth
///   errors, permanent gateway refusals, and response decoding errors.
pub async fn poll_transfer_tx_hash_with(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
) -> Result<TxHash, AlpacaWalletError> {
    info!(target: "wallet", %transfer_id, timeout = ?config.timeout, "Polling transfer tx hash");

    let start = Instant::now();

    loop {
        check_timeout(&start, config.timeout, *transfer_id)?;

        let remaining = config.timeout.saturating_sub(start.elapsed());
        let read = timeout(remaining, transfers.get_transfer(transfer_id))
            .await
            .map_err(|_| AlpacaWalletError::TransferTimeout {
                transfer_id: *transfer_id,
                elapsed: start.elapsed(),
            })?;

        let delay = match read {
            Ok(transfer) => match transfer_tx_hash_state(&transfer, *transfer_id)? {
                Some(tx_hash) => return Ok(tx_hash),
                None => config.interval,
            },
            Err(error) => {
                if !retryable_transfer_read_error(&error) {
                    return Err(error);
                }

                let retry_after = error.backpressure().and_then(|hint| hint.retry_after);
                warn!(target: "wallet", %transfer_id, %error, "Transfer read failed while waiting for its tx hash");
                retry_after.map_or(config.interval, |hint| hint.max(config.interval))
            }
        };

        sleep(delay.min(config.timeout.saturating_sub(start.elapsed()))).await;
    }
}

/// Retry check of the two backoff polls: a server error, or a gateway hop
/// failure the gateway classified as transient.
fn retryable_poll_read_error(error: &AlpacaWalletError) -> bool {
    match error {
        AlpacaWalletError::ApiError { status, .. } => status.is_server_error(),
        AlpacaWalletError::Gateway(hop) => hop.permanence() == Permanence::Transient,
        _ => false,
    }
}

fn retryable_transfer_read_error(error: &AlpacaWalletError) -> bool {
    match error {
        AlpacaWalletError::ApiError { status, .. } => {
            response_status_permanence(*status) == Permanence::Transient
        }
        AlpacaWalletError::Auth(error) => !error.is_deterministic(),
        AlpacaWalletError::Reqwest(_) => true,
        AlpacaWalletError::Gateway(hop) => hop.permanence() == Permanence::Transient,
        _ => false,
    }
}

fn transfer_tx_hash_state(
    transfer: &Transfer,
    transfer_id: AlpacaTransferId,
) -> Result<Option<TxHash>, AlpacaWalletError> {
    match (transfer.status, transfer.tx) {
        (TransferStatus::Failed, Some(tx_hash)) => Err(AlpacaWalletError::FailedTransferHasTx {
            transfer_id,
            tx_hash,
        }),
        (TransferStatus::Failed, None) => Err(AlpacaWalletError::TransferFailed { transfer_id }),
        (TransferStatus::Complete, Some(tx_hash)) => Ok(Some(tx_hash)),
        (TransferStatus::Pending | TransferStatus::Processing | TransferStatus::Complete, _) => {
            Ok(None)
        }
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

/// Represents a validated status transition.
#[derive(Debug)]
struct ValidatedTransition {
    new_status: TransferStatus,
    changed: bool,
}

/// Parses a status transition, returning Ok if valid or Err for invalid regressions.
fn parse_status_transition(
    prev: TransferStatus,
    next: TransferStatus,
) -> Result<ValidatedTransition, (TransferStatus, TransferStatus)> {
    let is_regression = matches!(
        (prev, next),
        (TransferStatus::Processing, TransferStatus::Pending)
            | (TransferStatus::Complete | TransferStatus::Failed, _)
    );

    if is_regression {
        Err((prev, next))
    } else {
        Ok(ValidatedTransition {
            new_status: next,
            changed: prev != next,
        })
    }
}

fn validate_and_log_status_change(
    transfer_id: AlpacaTransferId,
    last_status: Option<TransferStatus>,
    transfer: &Transfer,
) -> Result<(), AlpacaWalletError> {
    let Some(prev_status) = last_status else {
        let status = transfer.status;
        info!(target: "wallet", %transfer_id, ?status, "Transfer initial status");
        return Ok(());
    };

    let transition =
        parse_status_transition(prev_status, transfer.status).map_err(|(previous, next)| {
            AlpacaWalletError::InvalidStatusTransition {
                transfer_id,
                previous,
                next,
            }
        })?;

    if transition.changed {
        let new_status = transition.new_status;
        info!(target: "wallet", %transfer_id, from = ?prev_status, to = ?new_status, "Transfer status changed");
    }

    Ok(())
}

fn log_transfer_final_status(transfer_id: AlpacaTransferId, status: TransferStatus) {
    match status {
        TransferStatus::Complete => {
            info!(target: "wallet", %transfer_id, "Transfer completed successfully");
        }
        TransferStatus::Failed => warn!(target: "wallet", %transfer_id, "Transfer failed"),
        _ => warn!(
            target: "wallet",
            transfer_id = %transfer_id,
            status = ?status,
            "Unexpected non-final transfer status in log_transfer_final_status"
        ),
    }
}

/// Polls for a deposit transfer matching the given tx hash until it is
/// detected and reaches a terminal state.
///
/// This is used to wait for Alpaca to detect an incoming deposit by its
/// onchain tx hash. Server errors (5xx) and transient gateway hop failures of
/// the lookup are retried with exponential backoff.
///
/// # Errors
///
/// - `DepositTimeout` once `config.timeout` passes.
/// - `InvalidDepositTransition` when the status moves backwards.
/// - The lookup error once retries run out, or at once when it is not
///   retried.
pub async fn poll_deposit_by_tx_hash_with(
    transfers: &impl WalletTransfers,
    tx_hash: &TxHash,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    info!(target: "wallet", %tx_hash, timeout = ?config.timeout, "Polling deposit status");

    let start = Instant::now();
    let mut last_status: Option<TransferStatus> = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        check_deposit_timeout(&start, config.timeout, *tx_hash)?;

        let maybe_transfer = (|| transfers.find_transfer_by_tx_hash(tx_hash))
            .retry(retry_strategy)
            .when(retryable_poll_read_error)
            .await?;

        let Some(transfer) = maybe_transfer else {
            info!(target: "wallet", %tx_hash, "Deposit not yet detected, polling...");
            sleep(config.interval).await;
            continue;
        };

        validate_and_log_deposit_status_change(*tx_hash, last_status, &transfer)?;

        match transfer.status {
            TransferStatus::Complete | TransferStatus::Failed => {
                log_deposit_final_status(tx_hash, transfer.status);
                return Ok(transfer);
            }
            TransferStatus::Pending | TransferStatus::Processing => {
                last_status = Some(transfer.status);
                sleep(config.interval).await;
            }
        }
    }
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

fn validate_and_log_deposit_status_change(
    tx_hash: TxHash,
    last_status: Option<TransferStatus>,
    transfer: &Transfer,
) -> Result<(), AlpacaWalletError> {
    let Some(prev_status) = last_status else {
        let status = transfer.status;
        info!(target: "wallet", %tx_hash, ?status, "Deposit detected");
        return Ok(());
    };

    let transition =
        parse_status_transition(prev_status, transfer.status).map_err(|(previous, next)| {
            AlpacaWalletError::InvalidDepositTransition {
                tx_hash,
                previous,
                next,
            }
        })?;

    if transition.changed {
        let new_status = transition.new_status;
        info!(target: "wallet", %tx_hash, from = ?prev_status, to = ?new_status, "Deposit status changed");
    }

    Ok(())
}

fn log_deposit_final_status(tx_hash: &TxHash, status: TransferStatus) {
    match status {
        TransferStatus::Complete => {
            info!(target: "wallet", %tx_hash, "Deposit completed successfully");
        }
        TransferStatus::Failed => warn!(target: "wallet", %tx_hash, "Deposit failed"),
        _ => warn!(
            target: "wallet",
            %tx_hash,
            ?status,
            "Unexpected non-final deposit status in log_deposit_final_status"
        ),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::fixed_bytes;
    use httpmock::prelude::*;
    use reqwest::StatusCode;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use uuid::{Uuid, uuid};

    use super::*;

    use crate::broker::AlpacaAccountId;
    use crate::core::{AlpacaAuth, GatewayHopError};
    use crate::wallet::client::AlpacaWalletClient;
    use crate::wallet::transfer::TransferDirection;

    const TEST_ACCOUNT_ID: AlpacaAccountId =
        AlpacaAccountId::new(uuid!("904837e3-3b76-47ec-b432-046db621571b"));

    fn transfer_for_hash_poll(
        transfer_id: Uuid,
        status: &str,
        tx_hash: Option<TxHash>,
    ) -> Transfer {
        serde_json::from_value(json!({
            "id": transfer_id, "direction": "OUTGOING", "amount": "100",
            "chain": "ethereum", "asset": "USDC",
            "from_address": "0x0000000000000000000000000000000000000001",
            "to_address": "0x1234567890abcdef1234567890abcdef12345678",
            "status": status, "tx_hash": tx_hash,
            "created_at": "2024-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn hash_poll_rejects_failed_transfer_with_hash() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer = transfer_for_hash_poll(transfer_id, "FAILED", Some(tx_hash));

        assert!(matches!(
            transfer_tx_hash_state(&transfer, transfer_id.into()),
            Err(AlpacaWalletError::FailedTransferHasTx { transfer_id: id, tx_hash: hash })
                if id == transfer_id.into() && hash == tx_hash
        ));
    }

    #[test]
    fn hash_poll_rejects_failed_transfer_without_hash() {
        let transfer_id = Uuid::new_v4();
        let transfer = transfer_for_hash_poll(transfer_id, "FAILED", None);

        assert!(matches!(
            transfer_tx_hash_state(&transfer, transfer_id.into()),
            Err(AlpacaWalletError::TransferFailed { transfer_id: id })
                if id == transfer_id.into()
        ));
    }

    #[test]
    fn hash_poll_waits_for_completion_even_if_hash_is_present() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer = transfer_for_hash_poll(transfer_id, "PROCESSING", Some(tx_hash));

        assert!(
            transfer_tx_hash_state(&transfer, transfer_id.into())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn completed_transfer_without_tx_hash_keeps_polling_until_timeout() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let no_hash = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200).json_body_obj(&json!({
                "id": transfer_id, "direction": "OUTGOING", "amount": "100",
                "chain": "ethereum", "asset": "USDC",
                "from_address": "0x0000000000000000000000000000000000000001",
                "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                "status": "COMPLETE", "tx_hash": null,
                "created_at": "2024-01-01T00:00:00Z"
            }));
        });
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "key".into(),
                api_secret: "secret".into(),
            },
        )
        .unwrap();
        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(100),
            max_retries: 1,
            min_retry_delay: Duration::from_millis(5),
            max_retry_delay: Duration::from_millis(10),
        };

        let error = poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { transfer_id: id, .. } if id == transfer_id.into())
        );
        assert!(no_hash.calls() > 1);
    }

    #[tokio::test]
    async fn completed_transfer_with_tx_hash_returns_it() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let response = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200).json_body_obj(&json!({
                "id": transfer_id, "direction": "OUTGOING", "amount": "100",
                "chain": "ethereum", "asset": "USDC",
                "from_address": "0x0000000000000000000000000000000000000001",
                "to_address": "0x1234567890abcdef1234567890abcdef12345678",
                "status": "COMPLETE", "tx_hash": tx_hash,
                "created_at": "2024-01-01T00:00:00Z"
            }));
        });
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "key".into(),
                api_secret: "secret".into(),
            },
        )
        .unwrap();

        assert_eq!(
            poll_transfer_tx_hash_with(&client, &transfer_id.into(), &PollingConfig::default())
                .await
                .unwrap(),
            tx_hash
        );
        response.assert();
    }

    #[tokio::test]
    async fn transfer_tx_hash_poll_retries_failed_reads_until_timeout() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let failed_read = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(500).body("temporary failure");
        });
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "key".into(),
                api_secret: "secret".into(),
            },
        )
        .unwrap();
        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(100),
            max_retries: 1,
            min_retry_delay: Duration::from_millis(5),
            max_retry_delay: Duration::from_millis(10),
        };

        let error = poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { transfer_id: id, .. } if id == transfer_id.into())
        );
        assert!(failed_read.calls() > 1);
    }

    #[tokio::test]
    async fn transfer_tx_hash_poll_returns_permanent_read_errors() {
        for status in [404, 401] {
            let server = MockServer::start();
            let transfer_id = Uuid::new_v4();
            let rejected_read = server.mock(|when, then| {
                when.method(GET).path(format!(
                    "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
                ));
                then.status(status).body("permanent failure");
            });
            let client = AlpacaWalletClient::new(
                server.base_url(),
                TEST_ACCOUNT_ID,
                AlpacaAuth::Basic {
                    api_key: "key".into(),
                    api_secret: "secret".into(),
                },
            )
            .unwrap();
            let config = PollingConfig {
                interval: Duration::from_millis(10),
                timeout: Duration::from_millis(100),
                max_retries: 1,
                min_retry_delay: Duration::from_millis(5),
                max_retry_delay: Duration::from_millis(10),
            };

            let error = poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config)
                .await
                .unwrap_err();
            match status {
                404 => assert!(matches!(error, AlpacaWalletError::TransferNotFound { .. })),
                401 => assert!(
                    matches!(error, AlpacaWalletError::ApiError { status, .. } if status == reqwest::StatusCode::UNAUTHORIZED)
                ),
                _ => unreachable!(),
            }
            assert_eq!(rejected_read.calls(), 1);
        }
    }

    #[tokio::test]
    async fn transfer_tx_hash_poll_bounds_a_stalled_read_by_its_deadline() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let stalled_read = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .delay(Duration::from_millis(500))
                .body("{}");
        });
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "key".into(),
                api_secret: "secret".into(),
            },
        )
        .unwrap();
        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(50),
            max_retries: 1,
            min_retry_delay: Duration::from_millis(5),
            max_retry_delay: Duration::from_millis(10),
        };

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config),
        )
        .await
        .expect("polling must respect its own deadline");
        assert!(matches!(
            result,
            Err(AlpacaWalletError::TransferTimeout { .. })
        ));
        stalled_read.assert();
    }

    #[tokio::test]
    async fn transfer_tx_hash_poll_respects_retry_after_without_exceeding_deadline() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();
        let rate_limited = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(429)
                .header("Retry-After", "1")
                .body("rate limited");
        });
        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "key".into(),
                api_secret: "secret".into(),
            },
        )
        .unwrap();
        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(100),
            max_retries: 1,
            min_retry_delay: Duration::from_millis(5),
            max_retry_delay: Duration::from_millis(10),
        };

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config),
        )
        .await
        .expect("the Retry-After delay must be clamped to the polling deadline");
        assert!(matches!(
            result,
            Err(AlpacaWalletError::TransferTimeout { .. })
        ));
        assert_eq!(
            rate_limited.calls(),
            1,
            "the Retry-After hint must delay the next read"
        );
    }

    #[tokio::test]
    async fn test_poll_transfer_processing_to_complete() {
        let server = MockServer::start();

        let transfer_id = Uuid::new_v4();

        let complete_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&json!({
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let result = poll_transfer_until_complete_with(
            &client,
            &AlpacaTransferId::from(transfer_id),
            &config,
        )
        .await
        .unwrap();

        assert_eq!(result.status, TransferStatus::Complete);

        complete_mock.assert();
    }

    #[tokio::test]
    async fn test_poll_transfer_failed() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&json!({
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let result = poll_transfer_until_complete_with(
            &client,
            &AlpacaTransferId::from(transfer_id),
            &config,
        )
        .await
        .unwrap();

        assert_eq!(result.status, TransferStatus::Failed);

        status_mock.assert();
    }

    #[tokio::test]
    async fn test_poll_transfer_timeout() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let status_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&json!({
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(500),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_transfer_until_complete_with(
            &client,
            &AlpacaTransferId::from(transfer_id),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AlpacaWalletError::TransferTimeout { .. }));

        assert!(status_mock.calls() >= 2);
    }

    #[tokio::test]
    async fn test_poll_transfer_retry_on_5xx() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let error_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(503).body("Service Unavailable");
        });

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(100),
            timeout: Duration::from_secs(10),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_transfer_until_complete_with(
            &client,
            &AlpacaTransferId::from(transfer_id),
            &config,
        )
        .await
        .unwrap_err();

        assert!(
            error_mock.calls() >= 1,
            "Expected at least one retry attempt"
        );
        assert!(
            matches!(error, AlpacaWalletError::ApiError { status, .. } if status.as_u16() == 503)
        );
    }

    #[tokio::test]
    async fn test_poll_transfer_status_regression() {
        let server = MockServer::start();
        let transfer_id = Uuid::new_v4();

        let client = Arc::new(
            AlpacaWalletClient::new(
                server.base_url(),
                TEST_ACCOUNT_ID,
                AlpacaAuth::Basic {
                    api_key: "test_key_id".to_string(),
                    api_secret: "test_secret_key".to_string(),
                },
            )
            .unwrap(),
        );

        let mut processing_mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers/{transfer_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&json!({
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

        let config = PollingConfig {
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let client_clone = Arc::clone(&client);
        let transfer_id_clone = AlpacaTransferId::from(transfer_id);
        let poll_handle = tokio::spawn(async move {
            poll_transfer_until_complete_with(&*client_clone, &transfer_id_clone, &config).await
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
                .json_body_obj(&json!({
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
    fn test_parse_status_transition_processing_to_pending_is_regression() {
        let error = parse_status_transition(TransferStatus::Processing, TransferStatus::Pending)
            .unwrap_err();
        assert_eq!(error, (TransferStatus::Processing, TransferStatus::Pending));
    }

    #[test]
    fn test_parse_status_transition_complete_to_any_is_regression() {
        let error =
            parse_status_transition(TransferStatus::Complete, TransferStatus::Pending).unwrap_err();
        assert_eq!(error, (TransferStatus::Complete, TransferStatus::Pending));

        let error = parse_status_transition(TransferStatus::Complete, TransferStatus::Processing)
            .unwrap_err();
        assert_eq!(
            error,
            (TransferStatus::Complete, TransferStatus::Processing)
        );

        let error =
            parse_status_transition(TransferStatus::Complete, TransferStatus::Failed).unwrap_err();
        assert_eq!(error, (TransferStatus::Complete, TransferStatus::Failed));
    }

    #[test]
    fn test_parse_status_transition_failed_to_any_is_regression() {
        let error =
            parse_status_transition(TransferStatus::Failed, TransferStatus::Pending).unwrap_err();
        assert_eq!(error, (TransferStatus::Failed, TransferStatus::Pending));

        let error = parse_status_transition(TransferStatus::Failed, TransferStatus::Processing)
            .unwrap_err();
        assert_eq!(error, (TransferStatus::Failed, TransferStatus::Processing));

        let error =
            parse_status_transition(TransferStatus::Failed, TransferStatus::Complete).unwrap_err();
        assert_eq!(error, (TransferStatus::Failed, TransferStatus::Complete));
    }

    #[test]
    fn test_parse_status_transition_valid_transitions() {
        let result =
            parse_status_transition(TransferStatus::Pending, TransferStatus::Processing).unwrap();
        assert!(result.changed);
        assert_eq!(result.new_status, TransferStatus::Processing);

        let result =
            parse_status_transition(TransferStatus::Pending, TransferStatus::Complete).unwrap();
        assert!(result.changed);
        assert_eq!(result.new_status, TransferStatus::Complete);

        let result =
            parse_status_transition(TransferStatus::Pending, TransferStatus::Failed).unwrap();
        assert!(result.changed);
        assert_eq!(result.new_status, TransferStatus::Failed);

        let result =
            parse_status_transition(TransferStatus::Processing, TransferStatus::Complete).unwrap();
        assert!(result.changed);
        assert_eq!(result.new_status, TransferStatus::Complete);

        let result =
            parse_status_transition(TransferStatus::Processing, TransferStatus::Failed).unwrap();
        assert!(result.changed);
        assert_eq!(result.new_status, TransferStatus::Failed);
    }

    #[test]
    fn test_parse_status_transition_same_status_not_changed() {
        let result =
            parse_status_transition(TransferStatus::Pending, TransferStatus::Pending).unwrap();
        assert!(!result.changed);
        assert_eq!(result.new_status, TransferStatus::Pending);
    }

    #[tokio::test]
    async fn test_poll_deposit_by_tx_hash_found_immediately() {
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let transfer = poll_deposit_by_tx_hash_with(&client, &tx_hash, &config)
            .await
            .unwrap();

        assert_eq!(transfer.status, TransferStatus::Complete);
        assert_eq!(transfer.tx, Some(tx_hash));

        transfers_mock.assert();
    }

    #[tokio::test]
    async fn test_poll_deposit_by_tx_hash_not_found_then_found() {
        let server = MockServer::start();
        let tx_hash: TxHash =
            fixed_bytes!("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfer_id = Uuid::new_v4();

        let client = Arc::new(
            AlpacaWalletClient::new(
                server.base_url(),
                TEST_ACCOUNT_ID,
                AlpacaAuth::Basic {
                    api_key: "test_key_id".to_string(),
                    api_secret: "test_secret_key".to_string(),
                },
            )
            .unwrap(),
        );

        let mut empty_mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/accounts/{TEST_ACCOUNT_ID}/wallets/transfers"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });

        let config = PollingConfig {
            interval: Duration::from_millis(50),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let client_clone = Arc::clone(&client);
        let poll_handle = tokio::spawn(async move {
            poll_deposit_by_tx_hash_with(&*client_clone, &tx_hash, &config).await
        });

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
    async fn test_poll_deposit_by_tx_hash_timeout() {
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_millis(50),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let error = poll_deposit_by_tx_hash_with(&client, &tx_hash, &config)
            .await
            .unwrap_err();

        assert!(
            matches!(error, AlpacaWalletError::DepositTimeout { tx_hash: _, .. }),
            "Expected DepositTimeout error, got: {error:?}"
        );

        assert!(
            empty_mock.calls() >= 1,
            "Expected at least one poll attempt"
        );
    }

    #[tokio::test]
    async fn test_poll_deposit_by_tx_hash_found_failed() {
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

        let client = AlpacaWalletClient::new(
            server.base_url(),
            TEST_ACCOUNT_ID,
            AlpacaAuth::Basic {
                api_key: "test_key_id".to_string(),
                api_secret: "test_secret_key".to_string(),
            },
        )
        .unwrap();

        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(100),
        };

        let transfer = poll_deposit_by_tx_hash_with(&client, &tx_hash, &config)
            .await
            .unwrap();

        assert_eq!(transfer.status, TransferStatus::Failed);

        transfers_mock.assert();
    }

    /// Answers each transfer read and each tx hash lookup with the next
    /// scripted result, the way a gateway client would.
    struct ScriptedTransfers {
        reads: RefCell<VecDeque<Result<Transfer, AlpacaWalletError>>>,
        lookups: RefCell<VecDeque<Result<Option<Transfer>, AlpacaWalletError>>>,
    }

    impl ScriptedTransfers {
        fn reading(reads: impl IntoIterator<Item = Result<Transfer, AlpacaWalletError>>) -> Self {
            Self {
                reads: RefCell::new(reads.into_iter().collect()),
                lookups: RefCell::default(),
            }
        }

        fn looking_up(
            lookups: impl IntoIterator<Item = Result<Option<Transfer>, AlpacaWalletError>>,
        ) -> Self {
            Self {
                reads: RefCell::default(),
                lookups: RefCell::new(lookups.into_iter().collect()),
            }
        }

        fn reads_left(&self) -> usize {
            self.reads.borrow().len()
        }

        fn lookups_left(&self) -> usize {
            self.lookups.borrow().len()
        }
    }

    impl WalletTransfers for ScriptedTransfers {
        fn get_transfer(
            &self,
            _transfer_id: &AlpacaTransferId,
        ) -> impl Future<Output = Result<Transfer, AlpacaWalletError>> + Send {
            let next = self.reads.borrow_mut().pop_front();
            async move { next.expect("the poll read more transfers than scripted") }
        }

        fn find_transfer_by_tx_hash(
            &self,
            _tx_hash: &TxHash,
        ) -> impl Future<Output = Result<Option<Transfer>, AlpacaWalletError>> + Send {
            let next = self.lookups.borrow_mut().pop_front();
            async move { next.expect("the poll looked up more transfers than scripted") }
        }
    }

    fn gateway_failure() -> AlpacaWalletError {
        AlpacaWalletError::Gateway(GatewayHopError::transport("gateway unreachable"))
    }

    /// A refusal the gateway decided itself, `forbidden` say: retrying it
    /// cannot succeed.
    fn gateway_refusal() -> AlpacaWalletError {
        AlpacaWalletError::Gateway(GatewayHopError {
            retryable: false,
            ..GatewayHopError::transport("forbidden")
        })
    }

    fn api_error(status: StatusCode) -> AlpacaWalletError {
        AlpacaWalletError::ApiError {
            status,
            message: String::new(),
            retry_after: None,
        }
    }

    fn deposit(transfer_id: Uuid, status: &str, tx_hash: TxHash) -> Transfer {
        let mut transfer = transfer_for_hash_poll(transfer_id, status, Some(tx_hash));
        transfer.direction = TransferDirection::Incoming;
        transfer
    }

    fn fast_polling() -> PollingConfig {
        PollingConfig {
            interval: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            min_retry_delay: Duration::from_millis(1),
            max_retry_delay: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn generic_transfer_poll_retries_gateway_failures_like_server_errors() {
        let transfer_id = Uuid::new_v4();
        let transfers = ScriptedTransfers::reading([
            Err(gateway_failure()),
            Err(api_error(StatusCode::SERVICE_UNAVAILABLE)),
            Ok(transfer_for_hash_poll(transfer_id, "PENDING", None)),
            Err(gateway_failure()),
            Ok(transfer_for_hash_poll(transfer_id, "COMPLETE", None)),
        ]);

        let transfer =
            poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &fast_polling())
                .await
                .unwrap();

        assert_eq!(transfer.status, TransferStatus::Complete);
        assert_eq!(transfers.reads_left(), 0);
    }

    #[tokio::test]
    async fn generic_transfer_poll_returns_a_client_error_without_retry() {
        let transfer_id = Uuid::new_v4();
        let transfers = ScriptedTransfers::reading([
            Err(api_error(StatusCode::BAD_REQUEST)),
            Ok(transfer_for_hash_poll(transfer_id, "COMPLETE", None)),
        ]);

        let error =
            poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &fast_polling())
                .await
                .unwrap_err();

        assert!(
            matches!(error, AlpacaWalletError::ApiError { status, .. } if status == StatusCode::BAD_REQUEST),
            "{error:?}"
        );
        assert_eq!(transfers.reads_left(), 1);
    }

    #[tokio::test]
    async fn generic_transfer_poll_returns_a_gateway_refusal_without_retry() {
        let transfer_id = Uuid::new_v4();
        let transfers = ScriptedTransfers::reading([
            Err(gateway_refusal()),
            Ok(transfer_for_hash_poll(transfer_id, "COMPLETE", None)),
        ]);

        let error =
            poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &fast_polling())
                .await
                .unwrap_err();

        assert!(
            matches!(&error, AlpacaWalletError::Gateway(hop) if !hop.retryable),
            "{error:?}"
        );
        assert_eq!(transfers.reads_left(), 1);
    }

    #[tokio::test]
    async fn generic_transfer_poll_rejects_a_status_regression() {
        let transfer_id = Uuid::new_v4();
        let transfers = ScriptedTransfers::reading([
            Ok(transfer_for_hash_poll(transfer_id, "PROCESSING", None)),
            Ok(transfer_for_hash_poll(transfer_id, "PENDING", None)),
        ]);

        let error =
            poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &fast_polling())
                .await
                .unwrap_err();

        assert!(
            matches!(
                error,
                AlpacaWalletError::InvalidStatusTransition {
                    transfer_id: id,
                    previous: TransferStatus::Processing,
                    next: TransferStatus::Pending,
                } if id == transfer_id.into()
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn generic_tx_hash_poll_retries_a_gateway_failure() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfers = ScriptedTransfers::reading([
            Err(gateway_failure()),
            Ok(transfer_for_hash_poll(
                transfer_id,
                "COMPLETE",
                Some(tx_hash),
            )),
        ]);

        let found = poll_transfer_tx_hash_with(&transfers, &transfer_id.into(), &fast_polling())
            .await
            .unwrap();

        assert_eq!(found, tx_hash);
        assert_eq!(transfers.reads_left(), 0);
    }

    /// The tx hash poll retries until its timeout with no attempt cap, so a
    /// refusal it retried would be re-sent for the whole timeout and then
    /// reported as a timeout instead of the refusal.
    #[tokio::test]
    async fn generic_tx_hash_poll_returns_a_gateway_refusal_without_retry() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfers = ScriptedTransfers::reading([
            Err(gateway_refusal()),
            Ok(transfer_for_hash_poll(
                transfer_id,
                "COMPLETE",
                Some(tx_hash),
            )),
        ]);

        let error = poll_transfer_tx_hash_with(&transfers, &transfer_id.into(), &fast_polling())
            .await
            .unwrap_err();

        assert!(
            matches!(&error, AlpacaWalletError::Gateway(hop) if !hop.retryable),
            "{error:?}"
        );
        assert_eq!(transfers.reads_left(), 1);
    }

    /// The tx hash poll holds its next read off for the wait a gateway hop
    /// relays, as it does for Alpaca's own `Retry-After`, instead of reading
    /// again at the poll interval.
    #[tokio::test]
    async fn generic_tx_hash_poll_holds_off_for_the_wait_a_gateway_hop_relays() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let wait = Duration::from_millis(200);
        let transfers = ScriptedTransfers::reading([
            Err(AlpacaWalletError::Gateway(GatewayHopError {
                retry_after: Some(wait),
                ..GatewayHopError::transport("credential mint throttled")
            })),
            Ok(transfer_for_hash_poll(
                transfer_id,
                "COMPLETE",
                Some(tx_hash),
            )),
        ]);

        let started = Instant::now();
        let found = poll_transfer_tx_hash_with(&transfers, &transfer_id.into(), &fast_polling())
            .await
            .unwrap();

        assert_eq!(found, tx_hash);
        assert!(started.elapsed() >= wait, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn generic_deposit_poll_retries_a_gateway_failure_until_the_deposit_completes() {
        let deposit_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfers = ScriptedTransfers::looking_up([
            Err(gateway_failure()),
            Ok(None),
            Ok(Some(deposit(deposit_id, "PENDING", tx_hash))),
            Ok(Some(deposit(deposit_id, "COMPLETE", tx_hash))),
        ]);

        let found = poll_deposit_by_tx_hash_with(&transfers, &tx_hash, &fast_polling())
            .await
            .unwrap();

        assert_eq!(found.id, deposit_id.into());
        assert_eq!(found.status, TransferStatus::Complete);
        assert_eq!(transfers.lookups_left(), 0);
    }

    #[tokio::test]
    async fn generic_deposit_poll_rejects_a_status_regression() {
        let deposit_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfers = ScriptedTransfers::looking_up([
            Ok(Some(deposit(deposit_id, "PROCESSING", tx_hash))),
            Ok(Some(deposit(deposit_id, "PENDING", tx_hash))),
        ]);

        let error = poll_deposit_by_tx_hash_with(&transfers, &tx_hash, &fast_polling())
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                AlpacaWalletError::InvalidDepositTransition {
                    tx_hash: hash,
                    previous: TransferStatus::Processing,
                    next: TransferStatus::Pending,
                } if hash == tx_hash
            ),
            "{error:?}"
        );
    }
}
