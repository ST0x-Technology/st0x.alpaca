//! Where audit events go.
//!
//! Production writes one JSON line per event to stdout, which Cloud Run
//! turns into a structured log entry carrying the `log` label the audit sink
//! filters on. Tests collect events in memory.

use std::io::Write as _;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AUDIT_TARGET, AuditEvent};
use tracing::error;

/// Receives every audit event.
pub trait AuditSink: Send + Sync {
    fn emit(&self, event: &AuditEvent);
}

/// One structured JSON line per event on stdout.
pub struct StdoutSink;

impl AuditSink for StdoutSink {
    fn emit(&self, event: &AuditEvent) {
        let line = line(event);
        let mut stdout = std::io::stdout().lock();
        if let Err(write_error) = writeln!(stdout, "{line}") {
            error!(%write_error, request_id = %event.request_id, "Could not write audit event");
        }
    }
}

/// The structured log entry of `event`: Cloud Run reads `severity`,
/// `message` and the `logging.googleapis.com/labels` the audit sink filters
/// on; the event goes under `audit`.
fn line(event: &AuditEvent) -> Value {
    json!({
        "severity": "NOTICE",
        "message": format!("{} {}", event.operation, event.phase.as_str()),
        "logging.googleapis.com/labels": { "log": AUDIT_TARGET },
        "audit": event,
    })
}

/// Keeps events in memory, for tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Default)]
pub struct MemorySink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemorySink {
    /// Events emitted so far.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl AuditSink for MemorySink {
    fn emit(&self, event: &AuditEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use st0x_alpaca_gateway_api::{
        AuditPhase, ErrorCode, Operation, Outcome, Profile, RejectionReason, Tier,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn the_stdout_line_carries_the_audit_label_and_the_whole_event() {
        let event = AuditEvent {
            request_id: Uuid::new_v4(),
            phase: AuditPhase::Settled,
            at: Utc::now(),
            deployment: "t0-alpaca".to_string(),
            profile: Profile::T0,
            environment: "production".to_string(),
            account_id: "904837e3-3b76-47ec-b432-046db621571b".to_string(),
            principal: "111".to_string(),
            principal_email: Some("bot@example.com".to_string()),
            tier: Tier::Bot,
            on_behalf_of: Some("operator".to_string()),
            operation: Operation::WalletWithdraw,
            key: Some("key".to_string()),
            reason: None,
            request_digest: Some("digest".to_string()),
            summary: BTreeMap::from([("amount".to_string(), "10".to_string())]),
            alpaca_status: Some(422),
            alpaca_request_ids: vec!["alpaca-request".to_string()],
            alpaca_object_id: Some("transfer".to_string()),
            outcome: Some(Outcome::NotApplied),
            code: Some(ErrorCode::Rejected),
            rejection: Some(RejectionReason::AlpacaApi),
            latency_ms: 12,
            abandoned: false,
            gateway_version: "0.1.0+rev".to_string(),
        };

        let line = line(&event);

        assert_eq!(
            line["logging.googleapis.com/labels"]["log"],
            "st0x_alpaca_gateway_audit"
        );
        assert_eq!(line["severity"], "NOTICE");
        assert_eq!(line["message"], "wallet.withdraw settled");
        let audit: AuditEvent = serde_json::from_value(line["audit"].clone()).unwrap();
        assert_eq!(audit, event);
    }
}
