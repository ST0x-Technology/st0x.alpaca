//! The shared operations audit event the gateway emits for every request.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::access::{Profile, Tier};
use crate::failure::{ErrorCode, Outcome, RejectionReason};
use crate::ops::Operation;

/// Log target the gateway writes audit events under. The audit sink filters
/// on it.
pub const AUDIT_TARGET: &str = "st0x_alpaca_gateway_audit";

/// When in a request's life the event was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditPhase {
    /// The gateway answered the caller.
    Answered,
    /// A mutation that outlived its answer finished at Alpaca.
    Settled,
}

impl AuditPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Settled => "settled",
        }
    }
}

/// One audit record. Never carries credentials, tokens, Travel Rule names or
/// response bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub request_id: Uuid,
    pub phase: AuditPhase,
    pub at: DateTime<Utc>,
    pub deployment: String,
    pub profile: Profile,
    pub environment: String,
    pub account_id: String,
    /// Verified, stable subject id of the caller.
    pub principal: String,
    /// Caller email when the token carried one. Logged, never trusted.
    pub principal_email: Option<String>,
    pub tier: Tier,
    /// The human a bot acted for, from `X-On-Behalf-Of`. Informational.
    pub on_behalf_of: Option<String>,
    pub operation: Operation,
    /// The idempotency key: client order id, issuer request id, order id or
    /// operation id.
    pub key: Option<String>,
    /// Free text reason a human writer gave for a mutation.
    pub reason: Option<String>,
    /// SHA-256 of the request body, hex.
    pub request_digest: Option<String>,
    /// Money moving fields: symbol, side, quantity, amount, asset, network,
    /// destination, counterparty, recipient.
    pub summary: BTreeMap<String, String>,
    pub alpaca_status: Option<u16>,
    /// Order, transfer, journal or tokenization request id from Alpaca.
    pub alpaca_object_id: Option<String>,
    pub outcome: Option<Outcome>,
    pub code: Option<ErrorCode>,
    pub rejection: Option<RejectionReason>,
    pub latency_ms: u64,
    pub gateway_version: String,
}
