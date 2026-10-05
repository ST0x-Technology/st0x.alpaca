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
use st0x_alpaca::broker::AlpacaBrokerApiError;
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
            retry_after_secs: self.retry_after.map(|after| after.as_secs().max(1)),
            reason: self.reason,
            alpaca_status: self.alpaca_status,
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
            && let Ok(value) = HeaderValue::from_str(&after.as_secs().max(1).to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
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

fn api_rejection(status: reqwest::StatusCode, message: String) -> Option<Failure> {
    let code = status.as_u16();
    (status.is_client_error() && code != 408 && code != 429).then(|| Failure {
        alpaca_status: Some(code),
        ..Failure::rejected(RejectionReason::AlpacaApi, message)
    })
}

fn backpressure(retry_after: Option<Duration>, message: String) -> Failure {
    Failure {
        retry_after,
        alpaca_status: Some(429),
        ..Failure::new(ErrorCode::Backpressure, message)
    }
}

/// Maps a broker or market data error.
#[must_use]
pub fn broker(error: &AlpacaBrokerApiError, sent: Sent) -> Failure {
    use AlpacaBrokerApiError as E;

    let message = error.to_string();
    if let Some(pressure) = error.backpressure() {
        return backpressure(pressure.retry_after, message);
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
        | E::InvalidOrderId(_) => Failure::invalid(message),
        E::KmsJwt(_) | E::InvalidEndpoint(_) | E::InvalidHeader(_) => {
            Failure::new(ErrorCode::Unavailable, message)
        }
        E::ApiError {
            status, message: _, ..
        } => api_rejection(*status, message.clone())
            .unwrap_or_else(|| untyped(sent, error.permanence(), message)),
        _ => untyped(sent, error.permanence(), message),
    }
}

/// Maps a crypto wallet error.
#[must_use]
pub fn wallet(error: &AlpacaWalletError, sent: Sent) -> Failure {
    use AlpacaWalletError as E;

    let message = error.to_string();
    if let Some(pressure) = error.backpressure() {
        return backpressure(pressure.retry_after, message);
    }

    match error {
        E::AddressNotWhitelisted { .. } | E::NoWhitelistEntries { .. } => {
            Failure::rejected(RejectionReason::AddressNotWhitelisted, message)
        }
        E::Auth(_) | E::InvalidBaseUrl(_) => Failure::new(ErrorCode::Unavailable, message),
        E::TransferNotFound { .. } => Failure {
            alpaca_status: Some(404),
            ..Failure::rejected(RejectionReason::AlpacaApi, message)
        },
        E::ApiError { status, .. } => api_rejection(*status, message.clone())
            .unwrap_or_else(|| untyped(sent, Permanence::Transient, message)),
        _ => untyped(sent, Permanence::Transient, message),
    }
}

/// Maps a tokenization error.
#[must_use]
pub fn tokenization(error: &AlpacaTokenizationError, sent: Sent) -> Failure {
    use AlpacaTokenizationError as E;

    let message = error.to_string();
    if let Some(pressure) = error.backpressure() {
        return backpressure(pressure.retry_after, message);
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
        E::Auth(_) | E::InvalidBaseUrl(_) | E::PrivateKeyJwtUnsupported => {
            Failure::new(ErrorCode::Unavailable, message)
        }
        E::RequestNotFound { .. } => Failure {
            alpaca_status: Some(404),
            ..Failure::rejected(RejectionReason::AlpacaApi, message)
        },
        // Asking again returns the same answer; on a mint the request was
        // already written, so it stays `outcome_unknown`.
        E::WrongNetwork { .. }
        | E::NetworkMissing { .. }
        | E::DuplicateMintIssuerRequestId { .. } => untyped(sent, Permanence::Permanent, message),
        E::ApiError { status, .. } => api_rejection(*status, message.clone())
            .unwrap_or_else(|| untyped(sent, Permanence::Transient, message)),
        _ => untyped(sent, Permanence::Transient, message),
    }
}
