//! Transfer status polling with exponential backoff for
//! Alpaca Broker API wallet operations.
//!
//! Provides [`poll_transfer_until_complete_with`],
//! [`poll_transfer_tx_hash_with`] and [`poll_deposit_by_tx_hash_with`],
//! which poll a [`WalletTransfers`] until a transfer reaches a terminal state
//! (Complete/Failed) or times out.

use alloy_primitives::TxHash;
use backon::{BackoffBuilder, ExponentialBuilder};
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{info, warn};

use super::client::AlpacaWalletError;
use super::transfer::{AlpacaTransferId, Transfer, TransferStatus, WalletTransfers};
use crate::core::{BackpressurePolicy, Permanence};
use crate::rate_limit::{backpressure_wait, next_poll_delay, poll_deadline, read_before};

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

/// Runs one read of a backoff poll, reading again after a retried error
/// (`is_retried`) until the retry budget runs out. A backpressure error is
/// waited out as [`retry_wait`] decides; when the deadline then comes before
/// the next read answers, the poll ends with that backpressure error rather
/// than its timeout.
async fn retry_read_before<T, Read, ReadFuture>(
    deadline: Instant,
    mut read: Read,
    strategy: ExponentialBuilder,
    policy: BackpressurePolicy,
    timed_out: &impl Fn() -> AlpacaWalletError,
) -> Result<T, AlpacaWalletError>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: Future<Output = Result<T, AlpacaWalletError>>,
{
    let mut backoff = strategy.build();
    let mut throttled = None;
    loop {
        let outcome = read_before(deadline, &mut read, || {
            throttled.take().unwrap_or_else(timed_out)
        })
        .await;

        match outcome {
            Ok(value) => return Ok(value),
            Err(error) if is_retried(&error) => {
                let Some(backoff_delay) = backoff.next() else {
                    return Err(error);
                };
                let Some(wait) = retry_wait(&error, backoff_delay, policy, deadline) else {
                    return Err(error);
                };
                throttled = error.backpressure().is_some().then_some(error);
                sleep(next_poll_delay(wait, deadline)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// The wait before a retried read error (`is_retried`) is read again, or
/// `None` when the poll ends with it now. Any error but backpressure waits
/// `scheduled`; a backpressure error follows `policy`, as
/// [`backpressure_wait`] describes.
fn retry_wait(
    error: &AlpacaWalletError,
    scheduled: Duration,
    policy: BackpressurePolicy,
    deadline: Instant,
) -> Option<Duration> {
    match error.backpressure() {
        Some(backpressure) => backpressure_wait(policy, backpressure, scheduled, deadline),
        None => Some(scheduled),
    }
}

/// Polls a transfer until it reaches a terminal state (Complete or Failed).
///
/// A failed read the poll retries (`is_retried`) is read again after the
/// longer of its exponential backoff and its `Retry-After` hint, the hint
/// capped at five minutes. No read or backoff wait runs past the timeout.
/// Backpressure follows [`BackpressurePolicy::RetryInPlace`]; see
/// [`AlpacaWalletService::with_backpressure_policy`](super::AlpacaWalletService::with_backpressure_policy)
/// to return it instead.
///
/// # Errors
///
/// `TransferTimeout` once `config.timeout` passes, `InvalidStatusTransition`
/// when the status moves backwards, the backpressure error when its wait
/// would reach the timeout or the timeout comes during the read after it,
/// or the read error once retries run out or at once when it is not
/// retried.
pub async fn poll_transfer_until_complete_with(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    poll_transfer_until_complete_under(
        transfers,
        transfer_id,
        config,
        BackpressurePolicy::RetryInPlace,
    )
    .await
}

/// [`poll_transfer_until_complete_with`] under the backpressure `policy`.
pub(super) async fn poll_transfer_until_complete_under(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
    policy: BackpressurePolicy,
) -> Result<Transfer, AlpacaWalletError> {
    info!(target: "wallet", %transfer_id, timeout = ?config.timeout, "Polling transfer status");

    let start = Instant::now();
    let deadline = poll_deadline(start, config.timeout);
    let timed_out = || AlpacaWalletError::TransferTimeout {
        transfer_id: *transfer_id,
        elapsed: start.elapsed(),
    };
    let mut last_status = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        let transfer = retry_read_before(
            deadline,
            || transfers.get_transfer(transfer_id),
            retry_strategy,
            policy,
            &timed_out,
        )
        .await?;

        validate_and_log_status_change(*transfer_id, last_status, &transfer)?;

        match transfer.status {
            TransferStatus::Complete | TransferStatus::Failed => {
                log_transfer_final_status(*transfer_id, transfer.status);
                return Ok(transfer);
            }
            TransferStatus::Pending | TransferStatus::Processing => {
                last_status = Some(transfer.status);
                sleep(next_poll_delay(config.interval, deadline)).await;
            }
        }
    }
}

/// Polls a transfer until it is `Complete` and Alpaca reports its onchain tx
/// hash. A hash on a transfer that is not yet `Complete` is ignored. A failed
/// read the poll retries (`is_retried`) is read again at the next interval,
/// or after the `Retry-After` it relayed when that is longer, capped at five
/// minutes. No read starts at or after the timeout, and a read still running
/// there is dropped. Backpressure follows
/// [`BackpressurePolicy::RetryInPlace`], as in
/// [`poll_transfer_until_complete_with`].
///
/// # Errors
///
/// `TransferTimeout` once `config.timeout` passes, `TransferFailed` or
/// `FailedTransferHasTx` as soon as the transfer is `Failed`, the
/// backpressure error when its wait would reach the timeout or the timeout
/// comes during the read after it, or a read error that is not retried.
pub async fn poll_transfer_tx_hash_with(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
) -> Result<TxHash, AlpacaWalletError> {
    poll_transfer_tx_hash_under(
        transfers,
        transfer_id,
        config,
        BackpressurePolicy::RetryInPlace,
    )
    .await
}

/// [`poll_transfer_tx_hash_with`] under the backpressure `policy`.
pub(super) async fn poll_transfer_tx_hash_under(
    transfers: &impl WalletTransfers,
    transfer_id: &AlpacaTransferId,
    config: &PollingConfig,
    policy: BackpressurePolicy,
) -> Result<TxHash, AlpacaWalletError> {
    info!(target: "wallet", %transfer_id, timeout = ?config.timeout, "Polling transfer tx hash");

    let start = Instant::now();
    let deadline = poll_deadline(start, config.timeout);
    let timed_out = || AlpacaWalletError::TransferTimeout {
        transfer_id: *transfer_id,
        elapsed: start.elapsed(),
    };

    let mut throttled = None;

    loop {
        let read = read_before(
            deadline,
            || transfers.get_transfer(transfer_id),
            || throttled.take().unwrap_or_else(timed_out),
        )
        .await;

        let delay = match read {
            Ok(transfer) => {
                if let Some(tx_hash) = transfer_tx_hash_state(&transfer, *transfer_id)? {
                    return Ok(tx_hash);
                }
                // A read answered, so the throttle is over: a later deadline
                // is a plain pending timeout, not that 429.
                throttled = None;
                config.interval
            }
            Err(error) if is_retried(&error) => {
                warn!(target: "wallet", %transfer_id, %error, "Transfer read failed while waiting for its tx hash");
                let Some(wait) = retry_wait(&error, config.interval, policy, deadline) else {
                    return Err(error);
                };
                throttled = error.backpressure().is_some().then_some(error);
                wait
            }
            Err(error) => return Err(error),
        };

        sleep(next_poll_delay(delay, deadline)).await;
    }
}

/// The one retry rule of the three wallet polls: a read error a later read
/// can clear by [`AlpacaWalletError::permanence`] (a 5xx, 408 or 429, a
/// transport failure, a mint failure that is not deterministic, a gateway
/// hop the gateway classified as transient) is read again; any other
/// error, the poll's own timeout included, ends the poll.
fn is_retried(error: &AlpacaWalletError) -> bool {
    error.permanence() == Permanence::Transient
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

/// Polls for an incoming deposit transfer matching the given tx hash, through
/// [`WalletTransfers::find_deposit_by_tx_hash`], until it is detected and
/// reaches a terminal state. A failed lookup the poll retries
/// (`is_retried`) is looked up again after the longer of its exponential
/// backoff and its `Retry-After` hint, the hint capped at five minutes. No
/// lookup or backoff wait runs past the timeout. Backpressure follows
/// [`BackpressurePolicy::RetryInPlace`], as in
/// [`poll_transfer_until_complete_with`].
///
/// # Errors
///
/// `DepositTimeout` once `config.timeout` passes, `InvalidDepositTransition`
/// when the status moves backwards, the backpressure error when its wait
/// would reach the timeout or the timeout comes during the lookup after it,
/// or the lookup error once retries run out or at once when it is not
/// retried.
pub async fn poll_deposit_by_tx_hash_with(
    transfers: &impl WalletTransfers,
    tx_hash: &TxHash,
    config: &PollingConfig,
) -> Result<Transfer, AlpacaWalletError> {
    poll_deposit_by_tx_hash_under(transfers, tx_hash, config, BackpressurePolicy::RetryInPlace)
        .await
}

/// [`poll_deposit_by_tx_hash_with`] under the backpressure `policy`.
pub(super) async fn poll_deposit_by_tx_hash_under(
    transfers: &impl WalletTransfers,
    tx_hash: &TxHash,
    config: &PollingConfig,
    policy: BackpressurePolicy,
) -> Result<Transfer, AlpacaWalletError> {
    info!(target: "wallet", %tx_hash, timeout = ?config.timeout, "Polling deposit status");

    let start = Instant::now();
    let deadline = poll_deadline(start, config.timeout);
    let timed_out = || AlpacaWalletError::DepositTimeout {
        tx_hash: *tx_hash,
        elapsed: start.elapsed(),
    };
    let mut last_status: Option<TransferStatus> = None;

    let retry_strategy = ExponentialBuilder::default()
        .with_max_times(config.max_retries)
        .with_min_delay(config.min_retry_delay)
        .with_max_delay(config.max_retry_delay);

    loop {
        let maybe_transfer = retry_read_before(
            deadline,
            || transfers.find_deposit_by_tx_hash(tx_hash),
            retry_strategy,
            policy,
            &timed_out,
        )
        .await?;

        let Some(transfer) = maybe_transfer else {
            info!(target: "wallet", %tx_hash, "Deposit not yet detected, polling...");
            sleep(next_poll_delay(config.interval, deadline)).await;
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
                sleep(next_poll_delay(config.interval, deadline)).await;
            }
        }
    }
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
    use crate::rate_limit::MAX_RETRY_AFTER_HOLD;
    use crate::wallet::client::AlpacaWalletClient;

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

    /// A `Retry-After` that outlasts the timeout ends the poll with the 429
    /// and its hint at once, not with the timeout once the deadline comes.
    #[tokio::test]
    async fn transfer_tx_hash_poll_returns_a_429_whose_retry_after_outlasts_the_deadline() {
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

        let error = tokio::time::timeout(
            Duration::from_millis(500),
            poll_transfer_tx_hash_with(&client, &transfer_id.into(), &config),
        )
        .await
        .expect("the poll must end before its deadline")
        .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::ApiError { status, .. } if status == StatusCode::TOO_MANY_REQUESTS),
            "{error:?}"
        );
        assert_eq!(
            error.backpressure().and_then(|hint| hint.retry_after),
            Some(Duration::from_secs(1))
        );
        assert_eq!(rate_limited.calls(), 1);
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

    /// Answers each transfer read and each deposit lookup with the next
    /// scripted result.
    struct ScriptedTransfers {
        reads: RefCell<VecDeque<Result<Transfer, AlpacaWalletError>>>,
        deposits: RefCell<VecDeque<Result<Option<Transfer>, AlpacaWalletError>>>,
    }

    impl ScriptedTransfers {
        fn new(
            reads: Vec<Result<Transfer, AlpacaWalletError>>,
            deposits: Vec<Result<Option<Transfer>, AlpacaWalletError>>,
        ) -> Self {
            Self {
                reads: RefCell::new(reads.into()),
                deposits: RefCell::new(deposits.into()),
            }
        }

        fn left(&self) -> usize {
            self.reads.borrow().len() + self.deposits.borrow().len()
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

        fn find_deposit_by_tx_hash(
            &self,
            _tx_hash: &TxHash,
        ) -> impl Future<Output = Result<Option<Transfer>, AlpacaWalletError>> + Send {
            let next = self.deposits.borrow_mut().pop_front();
            async move { next.expect("the poll looked up more deposits than scripted") }
        }
    }

    fn fast_polling(timeout: Duration) -> PollingConfig {
        PollingConfig {
            interval: Duration::from_millis(1),
            timeout,
            max_retries: 3,
            min_retry_delay: Duration::from_millis(1),
            max_retry_delay: Duration::from_millis(5),
        }
    }

    /// A 429 that a later read answers is over: a tx hash poll that then only
    /// reads `PENDING` ends with `TransferTimeout`, not that old 429.
    #[tokio::test(start_paused = true)]
    async fn transfer_tx_hash_poll_times_out_after_a_429_that_later_reads_answered() {
        let transfer_id = Uuid::new_v4();
        let pending = transfer_for_hash_poll(transfer_id, "PENDING", None);
        let config = PollingConfig {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(5),
            max_retries: 1,
            min_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(1),
        };
        let limited = AlpacaWalletError::ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
            retry_after: Some(Duration::from_secs(1)),
        };
        let mut reads = vec![Err(limited)];
        reads.extend((0..20).map(|_| Ok(pending.clone())));
        let transfers = ScriptedTransfers::new(reads, vec![]);

        let error = poll_transfer_tx_hash_with(&transfers, &transfer_id.into(), &config)
            .await
            .unwrap_err();

        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { .. }),
            "{error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_polls_wait_out_retry_after_before_their_next_read() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let complete = transfer_for_hash_poll(transfer_id, "COMPLETE", Some(tx_hash));
        let wait = Duration::from_secs(60);
        let config = PollingConfig {
            interval: Duration::from_secs(1),
            timeout: wait + Duration::from_secs(1),
            max_retries: 1,
            min_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(1),
        };
        let limited = || AlpacaWalletError::ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
            retry_after: Some(wait),
        };

        let transfers = ScriptedTransfers::new(vec![Err(limited()), Ok(complete.clone())], vec![]);
        let started = Instant::now();
        let transfer = poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &config)
            .await
            .unwrap();
        assert_eq!(transfer.status, TransferStatus::Complete);
        assert_eq!(started.elapsed(), wait);
        assert_eq!(transfers.left(), 0);

        let deposits = ScriptedTransfers::new(vec![], vec![Err(limited()), Ok(Some(complete))]);
        let started = Instant::now();
        let deposit = poll_deposit_by_tx_hash_with(&deposits, &tx_hash, &config)
            .await
            .unwrap();
        assert_eq!(deposit.status, TransferStatus::Complete);
        assert_eq!(started.elapsed(), wait);
        assert_eq!(deposits.left(), 0);
    }

    /// The retry rule the three polls share: a read error `permanence` calls
    /// transient is read again, and any other ends the poll on that read.
    #[tokio::test]
    async fn wallet_polls_retry_only_a_transient_read_error() {
        fn api_error(status: StatusCode) -> AlpacaWalletError {
            AlpacaWalletError::ApiError {
                status,
                message: String::new(),
                retry_after: None,
            }
        }

        fn hop(retryable: bool) -> AlpacaWalletError {
            AlpacaWalletError::Gateway(GatewayHopError {
                retryable,
                ..GatewayHopError::transport("gateway unreachable")
            })
        }

        let rows: [(fn() -> AlpacaWalletError, bool); 4] = [
            (|| hop(true), true),
            (|| hop(false), false),
            (|| api_error(StatusCode::TOO_MANY_REQUESTS), true),
            (|| api_error(StatusCode::BAD_REQUEST), false),
        ];
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let complete = transfer_for_hash_poll(transfer_id, "COMPLETE", Some(tx_hash));
        let config = fast_polling(Duration::from_secs(5));

        for (error, retried) in rows {
            let reading =
                || ScriptedTransfers::new(vec![Err(error()), Ok(complete.clone())], vec![]);
            let (transfers, hashes) = (reading(), reading());
            let deposits =
                ScriptedTransfers::new(vec![], vec![Err(error()), Ok(Some(complete.clone()))]);

            let failures = [
                poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &config)
                    .await
                    .err(),
                poll_transfer_tx_hash_with(&hashes, &transfer_id.into(), &config)
                    .await
                    .err(),
                poll_deposit_by_tx_hash_with(&deposits, &tx_hash, &config)
                    .await
                    .err(),
            ];

            let expected = error().to_string();
            for (failure, scripted) in failures.into_iter().zip([&transfers, &hashes, &deposits]) {
                assert_eq!(
                    failure.map(|failure| failure.to_string()),
                    (!retried).then(|| expected.clone()),
                );
                assert_eq!(scripted.left(), usize::from(!retried), "{expected}");
            }
        }
    }

    #[tokio::test]
    async fn deposit_poll_rejects_a_status_regression() {
        let deposit_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let transfers = ScriptedTransfers::new(
            vec![],
            vec![
                Ok(Some(transfer_for_hash_poll(
                    deposit_id,
                    "PROCESSING",
                    Some(tx_hash),
                ))),
                Ok(Some(transfer_for_hash_poll(
                    deposit_id,
                    "PENDING",
                    Some(tx_hash),
                ))),
            ],
        );

        let error = poll_deposit_by_tx_hash_with(
            &transfers,
            &tx_hash,
            &fast_polling(Duration::from_secs(5)),
        )
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

    /// Answers every read and lookup with one that never completes,
    /// counting how many were started.
    #[derive(Default)]
    struct StalledTransfers {
        started: std::cell::Cell<usize>,
    }

    impl WalletTransfers for StalledTransfers {
        fn get_transfer(
            &self,
            _transfer_id: &AlpacaTransferId,
        ) -> impl Future<Output = Result<Transfer, AlpacaWalletError>> + Send {
            self.started.set(self.started.get() + 1);
            std::future::pending()
        }

        fn find_deposit_by_tx_hash(
            &self,
            _tx_hash: &TxHash,
        ) -> impl Future<Output = Result<Option<Transfer>, AlpacaWalletError>> + Send {
            self.started.set(self.started.get() + 1);
            std::future::pending()
        }
    }

    /// A read still running at the deadline is dropped there: each poll
    /// ends as its timeout at the deadline, not whenever the read would
    /// have answered.
    #[tokio::test(start_paused = true)]
    async fn every_wallet_poll_drops_a_read_still_running_at_its_deadline() {
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let deadline = Duration::from_secs(10);
        let config = fast_polling(deadline);

        let transfers = StalledTransfers::default();
        let started = Instant::now();
        let error = poll_transfer_until_complete_with(&transfers, &transfer_id, &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { .. }),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), deadline);
        assert_eq!(transfers.started.get(), 1);

        let transfers = StalledTransfers::default();
        let started = Instant::now();
        let error = poll_transfer_tx_hash_with(&transfers, &transfer_id, &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { .. }),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), deadline);
        assert_eq!(transfers.started.get(), 1);

        let transfers = StalledTransfers::default();
        let started = Instant::now();
        let error = poll_deposit_by_tx_hash_with(&transfers, &tx_hash, &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::DepositTimeout { .. }),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), deadline);
        assert_eq!(transfers.started.get(), 1);
    }

    /// A backoff wait longer than the time left ends at the deadline: both
    /// backoff polls time out there instead of sleeping the whole wait.
    #[tokio::test(start_paused = true)]
    async fn a_backoff_wait_past_the_deadline_ends_the_poll_at_the_deadline() {
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let deadline = Duration::from_secs(1);
        let config = PollingConfig {
            min_retry_delay: Duration::from_secs(60),
            max_retry_delay: Duration::from_secs(60),
            ..fast_polling(deadline)
        };
        let unavailable = || AlpacaWalletError::ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: String::new(),
            retry_after: None,
        };

        let transfers = ScriptedTransfers::new(vec![Err(unavailable())], vec![]);
        let started = Instant::now();
        let error = poll_transfer_until_complete_with(&transfers, &transfer_id, &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::TransferTimeout { .. }),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), deadline);

        let deposits = ScriptedTransfers::new(vec![], vec![Err(unavailable())]);
        let started = Instant::now();
        let error = poll_deposit_by_tx_hash_with(&deposits, &tx_hash, &config)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AlpacaWalletError::DepositTimeout { .. }),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), deadline);
    }

    fn rate_limited(retry_after: Duration) -> AlpacaWalletError {
        AlpacaWalletError::ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: String::new(),
            retry_after: Some(retry_after),
        }
    }

    fn assert_rate_limited(error: &AlpacaWalletError, retry_after: Duration) {
        assert!(
            matches!(error, AlpacaWalletError::ApiError { status, .. } if *status == StatusCode::TOO_MANY_REQUESTS),
            "{error:?}"
        );
        assert_eq!(
            error.backpressure().and_then(|hint| hint.retry_after),
            Some(retry_after)
        );
    }

    /// A `Retry-After` that outlasts the timeout ends each wallet poll at
    /// once with the 429 and its hint, not with the timeout: a caller that
    /// keeps a pending deposit only on backpressure still sees it.
    #[tokio::test(start_paused = true)]
    async fn wallet_polls_return_a_429_whose_wait_would_reach_the_timeout() {
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let retry_after = Duration::from_mins(2);
        let config = fast_polling(Duration::from_mins(1));

        let transfers = ScriptedTransfers::new(vec![Err(rate_limited(retry_after))], vec![]);
        let started = Instant::now();
        let error = poll_transfer_until_complete_with(&transfers, &transfer_id, &config)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(transfers.left(), 0);

        let hashes = ScriptedTransfers::new(vec![Err(rate_limited(retry_after))], vec![]);
        let error = poll_transfer_tx_hash_with(&hashes, &transfer_id, &config)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(hashes.left(), 0);

        let deposits = ScriptedTransfers::new(vec![], vec![Err(rate_limited(retry_after))]);
        let error = poll_deposit_by_tx_hash_with(&deposits, &tx_hash, &config)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(deposits.left(), 0);
    }

    /// An unbounded `Retry-After` holds each wallet poll for at most
    /// `MAX_RETRY_AFTER_HOLD` before its next read.
    #[tokio::test(start_paused = true)]
    async fn wallet_polls_cap_a_retry_after_wait_at_the_max_hold() {
        let transfer_id = Uuid::new_v4();
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let complete = transfer_for_hash_poll(transfer_id, "COMPLETE", Some(tx_hash));
        let unbounded = || rate_limited(Duration::from_secs(86_400));
        let config = fast_polling(Duration::from_hours(1));

        let transfers =
            ScriptedTransfers::new(vec![Err(unbounded()), Ok(complete.clone())], vec![]);
        let started = Instant::now();
        poll_transfer_until_complete_with(&transfers, &transfer_id.into(), &config)
            .await
            .unwrap();
        assert_eq!(started.elapsed(), MAX_RETRY_AFTER_HOLD);

        let hashes = ScriptedTransfers::new(vec![Err(unbounded()), Ok(complete.clone())], vec![]);
        let started = Instant::now();
        poll_transfer_tx_hash_with(&hashes, &transfer_id.into(), &config)
            .await
            .unwrap();
        assert_eq!(started.elapsed(), MAX_RETRY_AFTER_HOLD);

        let deposits = ScriptedTransfers::new(vec![], vec![Err(unbounded()), Ok(Some(complete))]);
        let started = Instant::now();
        poll_deposit_by_tx_hash_with(&deposits, &tx_hash, &config)
            .await
            .unwrap();
        assert_eq!(started.elapsed(), MAX_RETRY_AFTER_HOLD);
    }

    /// Answers the first read and lookup with a 429, then stalls the next
    /// one past any deadline.
    struct ThrottledThenStalled {
        retry_after: Duration,
        throttled: std::cell::Cell<bool>,
    }

    impl ThrottledThenStalled {
        fn new(retry_after: Duration) -> Self {
            Self {
                retry_after,
                throttled: std::cell::Cell::new(false),
            }
        }

        fn answer<T: Send>(&self) -> impl Future<Output = Result<T, AlpacaWalletError>> + Send {
            let first = !self.throttled.replace(true);
            let error = first.then(|| rate_limited(self.retry_after));
            async move {
                match error {
                    Some(error) => Err(error),
                    None => std::future::pending().await,
                }
            }
        }
    }

    impl WalletTransfers for ThrottledThenStalled {
        fn get_transfer(
            &self,
            _transfer_id: &AlpacaTransferId,
        ) -> impl Future<Output = Result<Transfer, AlpacaWalletError>> + Send {
            self.answer()
        }

        fn find_deposit_by_tx_hash(
            &self,
            _tx_hash: &TxHash,
        ) -> impl Future<Output = Result<Option<Transfer>, AlpacaWalletError>> + Send {
            self.answer()
        }
    }

    /// A deadline that comes during the read after a backpressure wait ends
    /// each wallet poll with that 429, not with the timeout.
    #[tokio::test(start_paused = true)]
    async fn wallet_polls_return_the_last_429_when_the_deadline_comes_after_its_wait() {
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let retry_after = Duration::from_secs(30);
        let timeout = Duration::from_mins(1);
        let config = fast_polling(timeout);

        let started = Instant::now();
        let error = poll_transfer_until_complete_with(
            &ThrottledThenStalled::new(retry_after),
            &transfer_id,
            &config,
        )
        .await
        .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), timeout);

        let started = Instant::now();
        let error = poll_transfer_tx_hash_with(
            &ThrottledThenStalled::new(retry_after),
            &transfer_id,
            &config,
        )
        .await
        .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), timeout);

        let started = Instant::now();
        let error = poll_deposit_by_tx_hash_with(
            &ThrottledThenStalled::new(retry_after),
            &tx_hash,
            &config,
        )
        .await
        .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(started.elapsed(), timeout);
    }

    /// Under `BackpressurePolicy::Return` each wallet poll ends on its first
    /// throttled read with the 429 and its hint, even with time to spare.
    #[tokio::test(start_paused = true)]
    async fn wallet_polls_return_a_429_at_once_under_the_return_policy() {
        let transfer_id = AlpacaTransferId::from(Uuid::new_v4());
        let tx_hash =
            fixed_bytes!("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890");
        let retry_after = Duration::from_secs(1);
        let config = fast_polling(Duration::from_hours(1));
        let policy = BackpressurePolicy::Return;

        let transfers = ScriptedTransfers::new(vec![Err(rate_limited(retry_after))], vec![]);
        let started = Instant::now();
        let error = poll_transfer_until_complete_under(&transfers, &transfer_id, &config, policy)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(transfers.left(), 0);

        let hashes = ScriptedTransfers::new(vec![Err(rate_limited(retry_after))], vec![]);
        let error = poll_transfer_tx_hash_under(&hashes, &transfer_id, &config, policy)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(hashes.left(), 0);

        let deposits = ScriptedTransfers::new(vec![], vec![Err(rate_limited(retry_after))]);
        let error = poll_deposit_by_tx_hash_under(&deposits, &tx_hash, &config, policy)
            .await
            .unwrap_err();
        assert_rate_limited(&error, retry_after);
        assert_eq!(deposits.left(), 0);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
