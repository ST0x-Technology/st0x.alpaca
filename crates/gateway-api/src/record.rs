//! The shared operations audit event the gateway emits for every request.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::access::Tier;
use crate::failure::{ErrorCode, Outcome, RejectionReason};
use crate::ops::Operation;

/// Log target the gateway writes audit events under. The audit sink filters
/// on it.
pub const AUDIT_TARGET: &str = "st0x_alpaca_gateway_audit";

/// When in a request's life the event was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditPhase {
    /// Written by the work when the handler took its result, or by the
    /// handler when it answered without the result.
    Answered,
    /// Written by the work when the handler had stopped waiting (its
    /// deadline, shutdown, its caller gone); carries the real result.
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
    /// The failure's own Alpaca HTTP status, or the last Alpaca status a
    /// success received.
    pub alpaca_status: Option<u16>,
    /// `X-Request-ID` of the Alpaca responses this request received, in
    /// order: the first 100, each cut at 128 characters. Join key with
    /// Alpaca support.
    #[serde(default)]
    pub alpaca_request_ids: Vec<String>,
    /// The Alpaca object the record is about: the order, transfer, journal,
    /// tokenization request or whitelist entries a mutation created or
    /// touched.
    pub alpaca_object_id: Option<String>,
    pub outcome: Option<Outcome>,
    pub code: Option<ErrorCode>,
    pub rejection: Option<RejectionReason>,
    pub latency_ms: u64,
    pub gateway_version: String,
}
