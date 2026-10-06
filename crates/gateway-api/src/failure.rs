//! The error answer every operation shares.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What went wrong, as one stable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request failed validation; nothing was sent.
    InvalidRequest,
    /// No usable caller identity.
    Unauthenticated,
    /// The caller may not use this operation.
    Forbidden,
    /// The operation is switched off in this deployment's config.
    CapabilityDisabled,
    /// No such operation for this deployment and tier.
    UnknownOperation,
    /// Alpaca refused the request; nothing was applied.
    Rejected,
    /// Rate limited, by Alpaca or by the human budget; nothing was applied.
    Backpressure,
    /// Failure before anything reached Alpaca (token mint, connect).
    Unavailable,
    /// The gateway has not finished its startup checks.
    NotReady,
    /// A read failed at Alpaca or in transit.
    UpstreamTransient,
    /// A mutation may have reached Alpaca and its result is not known.
    OutcomeUnknown,
}

impl ErrorCode {
    /// HTTP status the code is answered with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Unauthenticated => 401,
            Self::Forbidden | Self::CapabilityDisabled => 403,
            Self::UnknownOperation => 404,
            Self::Rejected => 422,
            Self::Backpressure => 429,
            Self::UpstreamTransient => 502,
            Self::Unavailable | Self::NotReady => 503,
            Self::OutcomeUnknown => 504,
        }
    }
}

/// Whether a mutation took effect. Absent on reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    NotApplied,
    Unknown,
}

/// The typed reason behind a `rejected` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// `UsdConversionInsufficientBalance`: safe to resize and submit again.
    InsufficientBalance,
    AccountNotActive,
    AssetNotActive,
    AssetNotTradable,
    /// Definitive mint rejections.
    InsufficientPosition,
    UnsupportedAccount,
    InvalidParameters,
    AddressNotWhitelisted,
    /// The request is outside a pinned destination, recipient or
    /// counterparty list.
    DestinationNotAllowed,
    /// Alpaca answered for a request on another network than the one asked
    /// for; `network` names the network Alpaca reported.
    WrongNetwork,
    /// Alpaca reported the request without a network, so it cannot be
    /// proven to be on the network asked for.
    NetworkMissing,
    /// The deployment holds no client for the requested network. Decided by
    /// the gateway; nothing was sent.
    UnsupportedNetwork,
    /// A keyed read of an object Alpaca does not hold: a tokenization
    /// request (`RequestNotFound`) or a wallet transfer (`TransferNotFound`).
    RequestNotFound,
    /// Any other Alpaca 4xx; see `alpaca_status` and `message`.
    AlpacaApi,
}

/// Body of every non-2xx answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: ErrorCode,
    /// Set on mutations only.
    pub outcome: Option<Outcome>,
    /// Whether the same request may be sent again unchanged.
    pub retryable: bool,
    /// Whether a mutation that answered `outcome_unknown` may be sent again
    /// with the same key. Alpaca dedupes these, or they are idempotent.
    pub retryable_with_same_key: bool,
    pub retry_after_secs: Option<u64>,
    pub reason: Option<RejectionReason>,
    pub alpaca_status: Option<u16>,
    /// The network Alpaca reported, set with `wrong_network`.
    pub network: Option<String>,
    /// The Alpaca objects the answer names: those a failed mutation already
    /// changed (the whitelist entries written before a loop failed), or the
    /// tokenization request a `wrong_network` or `network_missing` refusal
    /// is about.
    #[serde(default)]
    pub alpaca_object_ids: Vec<String>,
    pub request_id: Uuid,
    pub message: String,
}
