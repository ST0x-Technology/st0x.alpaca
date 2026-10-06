//! Turning `st0x-alpaca` errors into the contract's error answer.
//!
//! The classification is the crate's own (`permanence()`, `backpressure()`,
//! its typed rejections). The gateway adds only the `outcome` axis, which
//! depends on whether a mutating request may have been written to Alpaca.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use st0x_alpaca::Permanence;
use st0x_alpaca::broker::{AlpacaBrokerApiError, AlpacaMarketDataError, PlacementError};
use st0x_alpaca::tokenization::AlpacaTokenizationError;
use st0x_alpaca::wallet::AlpacaWalletError;
use st0x_alpaca_gateway_api::{
    ErrorBody, ErrorCode, Operation, Outcome, REQUEST_ID_HEADER, RejectionReason,
};
use uuid::Uuid;

/// A non-2xx answer.
#[derive(Debug, Clone)]
pub struct Failure {
    pub code: ErrorCode,
    pub outcome: Option<Outcome>,
    pub retryable: bool,
    pub retryable_with_same_key: bool,
    pub retry_after: Option<Duration>,
    pub reason: Option<RejectionReason>,
    pub alpaca_status: Option<u16>,
    /// The network Alpaca reported, set with `wrong_network`.
    pub network: Option<String>,
    /// The Alpaca objects the answer names: those a failed mutation already
    /// changed, or the tokenization request a network refusal is about.
    pub alpaca_object_ids: Vec<String>,
    pub message: String,
    pub request_id: Uuid,
}

impl Failure {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            outcome: None,
            retryable: matches!(
                code,
                ErrorCode::Backpressure
                    | ErrorCode::Unavailable
                    | ErrorCode::NotReady
                    | ErrorCode::UpstreamTransient
            ),
            retryable_with_same_key: false,
            retry_after: None,
            reason: None,
            alpaca_status: None,
            network: None,
            alpaca_object_ids: Vec::new(),
            message: message.into(),
            request_id: Uuid::new_v4(),
        }
    }

    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    /// A refusal decided by the gateway before anything was sent, for a
    /// destination outside the deployment's pinned list.
    #[must_use]
    pub fn destination_not_allowed(message: impl Into<String>) -> Self {
        Self {
            reason: Some(RejectionReason::DestinationNotAllowed),
            ..Self::new(ErrorCode::Forbidden, message)
        }
    }

    #[must_use]
    pub fn rejected(reason: RejectionReason, message: impl Into<String>) -> Self {
        Self {
            reason: Some(reason),
            ..Self::new(ErrorCode::Rejected, message)
        }
    }

    /// Sets `outcome` for a mutation: anything that is not `outcome_unknown`
    /// left Alpaca untouched.
    #[must_use]
    pub fn for_mutation(mut self, operation: Operation) -> Self {
        if self.code == ErrorCode::OutcomeUnknown {
            self.outcome = Some(Outcome::Unknown);
            self.retryable = false;
            self.retryable_with_same_key = operation.resendable_with_same_key();
        } else {
            self.outcome = Some(Outcome::NotApplied);
        }
        self
    }

    /// The answer for a mutation still running when its deadline passed.
    #[must_use]
    pub fn deadline_passed(operation: Operation) -> Self {
        Self::new(
            ErrorCode::OutcomeUnknown,
            format!(
                "{operation} did not finish within {}s; it may still complete at Alpaca",
                operation.deadline().as_secs()
            ),
        )
        .for_mutation(operation)
    }

    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code,
            outcome: self.outcome,
            retryable: self.retryable,
            retryable_with_same_key: self.retryable_with_same_key,
            retry_after_secs: self.retry_after.map(whole_seconds),
            reason: self.reason,
            alpaca_status: self.alpaca_status,
            network: self.network.clone(),
            alpaca_object_ids: self.alpaca_object_ids.clone(),
            request_id: self.request_id,
            message: self.message.clone(),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(self.body())).into_response();
        if let Ok(value) = HeaderValue::from_str(&self.request_id.to_string()) {
            response.headers_mut().insert(REQUEST_ID_HEADER, value);
        }
        if let Some(after) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&whole_seconds(after).to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

/// A hold in the whole seconds `retryAfterSecs` and `Retry-After` carry,
/// rounded up and at least one, so a caller that waits the advertised time
/// never retries before the hold ends.
fn whole_seconds(hold: Duration) -> u64 {
    hold.as_secs()
        .saturating_add(u64::from(hold.subsec_nanos() > 0))
        .max(1)
}

/// Whether the failed call was a read or a mutation that may have been
/// written to Alpaca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Read,
    Mutation,
}

/// The answer for an Alpaca failure the crate did not type: a read is
/// transient or permanent per the crate; a mutation may have been applied.
fn untyped(sent: Sent, permanence: Permanence, message: String) -> Failure {
    match sent {
        Sent::Read => Failure {
            retryable: permanence == Permanence::Transient,
            ..Failure::new(ErrorCode::UpstreamTransient, message)
        },
        Sent::Mutation => Failure::new(ErrorCode::OutcomeUnknown, message),
    }
}

/// The answer for an Alpaca error status: a definite 4xx is `rejected`, a
/// 408 or 5xx is [`untyped`]. Either keeps the status Alpaca answered.
fn api_failure(
    status: reqwest::StatusCode,
    sent: Sent,
    permanence: Permanence,
    message: String,
) -> Failure {
    let code = status.as_u16();
    let failure = if status.is_client_error() && code != 408 && code != 429 {
        Failure::rejected(RejectionReason::AlpacaApi, message)
    } else {
        untyped(sent, permanence, message)
    };
    Failure {
        alpaca_status: Some(code),
        ..failure
    }
}

/// The answer for a throttled call. Only a 429 Alpaca answered is
/// `backpressure`; a throttled credential mint (KMS or the token endpoint)
/// stopped before the request was sent, so it is `unavailable`, still
/// carrying the wait.
fn throttled(
    retry_after: Option<Duration>,
    alpaca_status: Option<reqwest::StatusCode>,
    message: String,
) -> Failure {
    let failure = match alpaca_status {
        Some(status) => Failure {
            alpaca_status: Some(status.as_u16()),
            ..Failure::new(ErrorCode::Backpressure, message)
        },
        None => Failure::new(ErrorCode::Unavailable, message),
    };
    Failure {
        retry_after,
        ..failure
    }
}

/// Maps a broker or market data error.
#[must_use]
pub fn broker(error: &AlpacaBrokerApiError, sent: Sent) -> Failure {
    use AlpacaBrokerApiError as E;

    let message = error.to_string();
    if let E::LatestTrade(source) | E::LatestQuote(source) = error {
        return market_data(source, sent, error.permanence(), message);
    }
    if let Some(pressure) = error.backpressure() {
        let status = match error {
            E::ApiError { status, .. } => Some(*status),
            _ => None,
        };
        return throttled(pressure.retry_after, status, message);
    }

    match error {
        E::UsdConversionInsufficientBalance { .. } => Failure {
            alpaca_status: Some(403),
            ..Failure::rejected(RejectionReason::InsufficientBalance, message)
        },
        E::AccountNotActive { .. } => Failure::rejected(RejectionReason::AccountNotActive, message),
        E::AssetNotActive { .. } => Failure::rejected(RejectionReason::AssetNotActive, message),
        E::AssetNotTradable { .. } => Failure::rejected(RejectionReason::AssetNotTradable, message),
        // Raised before anything is sent.
        E::InvalidLimitPricePrecision { .. }
        | E::BelowPrecision { .. }
        | E::UsdcBelowPrecision { .. }
        | E::UsdcPrecisionExceeded { .. }
        | E::NotPositive(_)
        | E::NotPositiveLimitPrice(_)
        | E::UnsafeSymbol { .. }
        | E::InvalidOrderId(_) => Failure::invalid(message),
        E::AccountActivitiesPageLimitExceeded { pages } => Failure::invalid(format!(
            "more than {pages} pages of account activities match; narrow the after and until \
             window"
        )),
        // The request never left: no credential, no endpoint, no connection.
        E::KmsJwt(_) | E::InvalidEndpoint(_) | E::InvalidHeader(_) => {
            Failure::new(ErrorCode::Unavailable, message)
        }
        E::HttpClient(source) if source.is_connect() => {
            Failure::new(ErrorCode::Unavailable, message)
        }
        E::ApiError { status, .. } => api_failure(*status, sent, error.permanence(), message),
        _ => untyped(sent, error.permanence(), message),
    }
}

/// Maps the market data failure inside `LatestTrade` or `LatestQuote`: an
/// Alpaca status goes with the answer as it does for a broker `ApiError`, so
/// a definite 4xx (a delisted symbol, a missing entitlement) is `rejected`.
fn market_data(
    error: &AlpacaMarketDataError,
    sent: Sent,
    permanence: Permanence,
    message: String,
) -> Failure {
    use AlpacaMarketDataError as E;

    let status = match error {
        E::ApiError { status, .. } | E::Entitlement { status, .. } => Some(*status),
        _ => None,
    };
    if let Some(pressure) = error.backpressure() {
        return throttled(pressure.retry_after, status, message);
    }

    match error {
        E::Auth(_) => Failure::new(ErrorCode::Unavailable, message),
        E::Http(source) if source.is_connect() => Failure::new(ErrorCode::Unavailable, message),
        _ => match status {
            Some(status) => api_failure(status, sent, permanence, message),
            None => untyped(sent, permanence, message),
        },
    }
}

/// Maps an order placement failure by whether the order request may have
/// been written. Once it may have, every failure is `outcome_unknown`,
/// whatever a later read answered, since an order may exist. Before that,
/// and on a definite rejection of the request, it maps as a read: nothing
/// was applied.
#[must_use]
pub fn placement(failure: &PlacementError) -> Failure {
    if !failure.written {
        return broker(&failure.error, Sent::Read);
    }
    let alpaca_status = match &failure.error {
        AlpacaBrokerApiError::ApiError { status, .. } => Some(status.as_u16()),
        _ => None,
    };
    Failure {
        alpaca_status,
        ..Failure::new(ErrorCode::OutcomeUnknown, failure.error.to_string())
    }
}

/// Maps a crypto wallet error.
#[must_use]
pub fn wallet(error: &AlpacaWalletError, sent: Sent) -> Failure {
    use AlpacaWalletError as E;

    let message = error.to_string();
    if let Some(pressure) = error.backpressure() {
        let status = match error {
            E::ApiError { status, .. } => Some(*status),
            _ => None,
        };
        return throttled(pressure.retry_after, status, message);
    }

    match error {
        E::AddressNotWhitelisted { .. } | E::NoWhitelistEntries { .. } => {
            Failure::rejected(RejectionReason::AddressNotWhitelisted, message)
        }
        // The request never left: no credential, no endpoint, no connection.
        E::Auth(_) | E::InvalidBaseUrl(_) => Failure::new(ErrorCode::Unavailable, message),
        E::Reqwest(source) if source.is_connect() => Failure::new(ErrorCode::Unavailable, message),
        E::TransferNotFound { .. } => Failure {
            alpaca_status: Some(404),
            ..Failure::rejected(RejectionReason::RequestNotFound, message)
        },
        E::ApiError { status, .. } => api_failure(*status, sent, Permanence::Transient, message),
        _ => untyped(sent, Permanence::Transient, message),
    }
}

/// Maps a tokenization error.
#[must_use]
pub fn tokenization(error: &AlpacaTokenizationError, sent: Sent) -> Failure {
    use AlpacaTokenizationError as E;

    let message = error.to_string();
    if let Some(pressure) = error.backpressure() {
        let status = match error {
            E::ApiError { status, .. } => Some(*status),
            _ => None,
        };
        return throttled(pressure.retry_after, status, message);
    }

    if error.is_definitive_mint_rejection() {
        let reason = match error {
            E::InsufficientPosition { .. } => RejectionReason::InsufficientPosition,
            E::UnsupportedAccount => RejectionReason::UnsupportedAccount,
            _ => RejectionReason::InvalidParameters,
        };
        return Failure {
            alpaca_status: error.status_code().map(|status| status.as_u16()),
            ..Failure::rejected(reason, message)
        };
    }

    match error {
        // The request never left: no credential, no endpoint, no connection.
        E::Auth(_) | E::InvalidBaseUrl(_) | E::PrivateKeyJwtUnsupported => {
            Failure::new(ErrorCode::Unavailable, message)
        }
        E::Reqwest(source) if source.is_connect() => Failure::new(ErrorCode::Unavailable, message),
        // Raised after scanning a list Alpaca answered with 200, so no
        // Alpaca status goes with it.
        E::RequestNotFound { .. } => Failure::rejected(RejectionReason::RequestNotFound, message),
        // Asking again returns the same answer.
        E::WrongNetwork { id, actual, .. } => network_refusal(
            sent,
            RejectionReason::WrongNetwork,
            Some(actual.to_string()),
            id.to_string(),
            message,
        ),
        E::NetworkMissing { id } => network_refusal(
            sent,
            RejectionReason::NetworkMissing,
            None,
            id.to_string(),
            message,
        ),
        E::DuplicateMintIssuerRequestId { .. } => untyped(sent, Permanence::Permanent, message),
        E::ApiError { status, .. } => api_failure(*status, sent, Permanence::Transient, message),
        _ => untyped(sent, Permanence::Transient, message),
    }
}

/// A request Alpaca reported off the bound network, named by its id. A read
/// is a definite rejection carrying the network Alpaca reported; on a mint
/// the request was already written, so it stays `outcome_unknown`.
fn network_refusal(
    sent: Sent,
    reason: RejectionReason,
    network: Option<String>,
    request_id: String,
    message: String,
) -> Failure {
    let failure = match sent {
        Sent::Read => Failure {
            network,
            ..Failure::rejected(reason, message)
        },
        Sent::Mutation => untyped(sent, Permanence::Permanent, message),
    };
    Failure {
        alpaca_object_ids: vec![request_id],
        ..failure
    }
}

#[cfg(test)]
mod tests {
    use st0x_alpaca::KmsJwtError;

    use super::*;

    fn kms_throttled() -> KmsJwtError {
        KmsJwtError::KmsStatus {
            status: 429,
            body: "signing quota exceeded".to_string(),
            retry_after: Some(Duration::from_secs(30)),
        }
    }

    /// A reqwest error from a port nothing listens on.
    async fn connect_error() -> reqwest::Error {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error}");
        error
    }

    #[test]
    fn a_throttled_credential_mint_is_unavailable_without_an_alpaca_status() {
        let failures = [
            broker(
                &AlpacaBrokerApiError::KmsJwt(kms_throttled()),
                Sent::Mutation,
            ),
            broker(
                &AlpacaBrokerApiError::LatestQuote(Box::new(AlpacaMarketDataError::Auth(
                    kms_throttled(),
                ))),
                Sent::Read,
            ),
            wallet(&AlpacaWalletError::Auth(kms_throttled()), Sent::Mutation),
            tokenization(
                &AlpacaTokenizationError::Auth(kms_throttled()),
                Sent::Mutation,
            ),
        ];

        for failure in failures {
            assert_eq!(failure.code, ErrorCode::Unavailable, "{failure:?}");
            assert_eq!(failure.alpaca_status, None, "{failure:?}");
            assert_eq!(
                failure.retry_after,
                Some(Duration::from_secs(30)),
                "{failure:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_keyless_mutation_that_could_not_connect_was_not_applied() {
        let withdrawal = wallet(
            &AlpacaWalletError::Reqwest(connect_error().await),
            Sent::Mutation,
        )
        .for_mutation(Operation::WalletWithdraw);
        let mint = tokenization(
            &AlpacaTokenizationError::Reqwest(connect_error().await),
            Sent::Mutation,
        )
        .for_mutation(Operation::TokenizationMint);

        for failure in [withdrawal, mint] {
            assert_eq!(failure.code, ErrorCode::Unavailable, "{failure:?}");
            assert_eq!(failure.outcome, Some(Outcome::NotApplied), "{failure:?}");
        }
    }

    #[test]
    fn a_fractional_hold_is_advertised_rounded_up() {
        let failure = Failure {
            retry_after: Some(Duration::from_millis(2100)),
            ..Failure::new(ErrorCode::Backpressure, "human request budget spent")
        };

        assert_eq!(failure.body().retry_after_secs, Some(3));
        let response = failure.into_response();
        assert_eq!(
            response.headers().get(axum::http::header::RETRY_AFTER),
            Some(&HeaderValue::from_static("3"))
        );
    }
}
