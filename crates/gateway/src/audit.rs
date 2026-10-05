//! Where audit events go.
//!
//! Production writes one JSON line per event to stdout, which Cloud Run
//! turns into a structured log entry carrying the `log` label the audit sink
//! filters on. Tests collect events in memory.

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use serde_json::json;
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
        let line = json!({
            "severity": "NOTICE",
            "message": format!("{} {}", event.operation, event.phase.as_str()),
            "logging.googleapis.com/labels": { "log": AUDIT_TARGET },
            "audit": event,
        });
        let mut stdout = std::io::stdout().lock();
        if let Err(write_error) = writeln!(stdout, "{line}") {
            error!(%write_error, request_id = %event.request_id, "Could not write audit event");
        }
    }
}

/// Keeps events in memory, for tests.
#[derive(Clone, Default)]
pub struct MemorySink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

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

impl AuditSink for MemorySink {
    fn emit(&self, event: &AuditEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
    }
}
