//! Shared state, the startup account check, and the runner every handler
//! goes through: admission (shutdown, capability switch, human budget), the
//! operation deadline, detached work, and audit.

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::FromRequestParts;
use axum::http::HeaderValue;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use st0x_alpaca::broker::{AlpacaBrokerApi, AlpacaBrokerApiError};
use st0x_alpaca::core::Network;
use st0x_alpaca::request_id::{self, SendGate, Traffic};
use st0x_alpaca::tokenization::{AlpacaTokenizationError, AlpacaTokenizationService};
use st0x_alpaca::wallet::{AlpacaWalletError, AlpacaWalletService};
use st0x_alpaca_gateway_api::{
    AuditEvent, AuditPhase, DEPLOYMENT, ErrorCode, ON_BEHALF_OF_HEADER, Operation, Outcome,
    REQUEST_ID_HEADER, RejectionReason, Tier,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

use crate::answer::Failure;
use crate::audit::AuditSink;
use crate::auth::Principal;
use crate::budget::{self, HumanBudget};
use crate::config::GatewayConfig;

/// Longest caller supplied value the audit record keeps: the key, the
/// reason, each summary value and `X-On-Behalf-Of` keep at most their first
/// this many characters, so one record stays one line the log pipeline
/// accepts. The request digest still covers the whole request.
const AUDIT_FIELD_MAX: usize = 256;

/// Longest Alpaca supplied id the audit record keeps, in characters.
const AUDIT_ID_MAX: usize = 128;

/// Most Alpaca supplied ids one audit list keeps: the first this many.
const AUDIT_IDS_MAX: usize = 100;

/// `text` cut to its first `max` characters.
fn cut(mut text: String, max: usize) -> String {
    if let Some((at, _)) = text.char_indices().nth(max) {
        text.truncate(at);
    }
    text
}

/// `value` displayed, cut to its first [`AUDIT_FIELD_MAX`] characters.
fn capped(value: &impl std::fmt::Display) -> String {
    cut(value.to_string(), AUDIT_FIELD_MAX)
}

/// `ids` as the audit record keeps them: the first [`AUDIT_IDS_MAX`], each
/// cut to its first [`AUDIT_ID_MAX`] characters.
fn audit_ids(ids: &[String]) -> Vec<String> {
    ids.iter()
        .take(AUDIT_IDS_MAX)
        .map(|id| cut(id.clone(), AUDIT_ID_MAX))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("Alpaca account check failed: {0}")]
    Broker(#[from] AlpacaBrokerApiError),
    #[error(
        "configured account reports account number {reported:?}, expected {expected}; \
         refusing to serve"
    )]
    AccountMismatch {
        expected: String,
        reported: Option<String>,
    },
    #[error("wallet client: {0}")]
    Wallet(#[from] AlpacaWalletError),
    #[error("tokenization client: {0}")]
    Tokenization(#[from] AlpacaTokenizationError),
}

pub struct Inner {
    pub config: GatewayConfig,
    pub broker: AlpacaBrokerApi,
    pub wallet: AlpacaWalletService,
    pub tokenizers: HashMap<Network, AlpacaTokenizationService>,
    pub audit: Arc<dyn AuditSink>,
    pub budget: HumanBudget,
    /// Detached work; shutdown waits for it.
    pub tasks: TaskTracker,
    /// Cancelled when shutdown starts. Requests in flight answer at once and
    /// new ones are refused, so the connection drain fits the grace period.
    pub shutdown: CancellationToken,
    pub version: String,
    /// Replaces every operation deadline.
    #[cfg(any(test, feature = "test-support"))]
    deadline_override: Option<Duration>,
}

/// Cheap to clone; every handler gets one.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Builds the Alpaca clients and proves the configured account is the
    /// expected one. Fails closed: no state, no server.
    ///
    /// # Errors
    ///
    /// Returns [`StartupError`] when Alpaca cannot be reached, the account is
    /// not active, or its account number differs from config.
    pub async fn connect(
        config: GatewayConfig,
        audit: Arc<dyn AuditSink>,
        version: String,
    ) -> Result<Self, StartupError> {
        let broker = AlpacaBrokerApi::try_from_ctx(config.broker.clone()).await?;

        let reported = broker.account_number().map(str::to_string);
        if reported.as_deref() != Some(config.expected_account_number.as_str()) {
            return Err(StartupError::AccountMismatch {
                expected: config.expected_account_number.clone(),
                reported,
            });
        }

        let base_url = config.broker.base_url().to_string();
        let wallet = AlpacaWalletService::new(
            base_url.clone(),
            config.broker.account_id,
            config.broker.auth.clone(),
        )?;

        let mut tokenizers = HashMap::new();
        for network in &config.tokenization.networks {
            let service = AlpacaTokenizationService::new(
                base_url.clone(),
                config.broker.account_id,
                config.broker.auth.clone(),
                *network,
            )?;
            tokenizers.insert(*network, service);
        }

        info!(
            environment = %config.environment,
            account_id = %config.broker.account_id,
            "Gateway bound to its Alpaca account"
        );

        let budget = HumanBudget::new(config.human_budget_per_minute);
        Ok(Self(Arc::new(Inner {
            config,
            broker,
            wallet,
            tokenizers,
            audit,
            budget,
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            version,
            #[cfg(any(test, feature = "test-support"))]
            deadline_override: None,
        })))
    }

    /// [`Self::connect`] with every operation deadline replaced, so tests
    /// can exercise the deadline path without waiting for it.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    #[cfg(any(test, feature = "test-support"))]
    pub async fn connect_with_deadline(
        config: GatewayConfig,
        audit: Arc<dyn AuditSink>,
        version: String,
        deadline_override: Option<Duration>,
    ) -> Result<Self, StartupError> {
        let mut state = Self::connect(config, audit, version).await?;
        // Nothing else holds the state yet.
        if let Some(inner) = Arc::get_mut(&mut state.0) {
            inner.deadline_override = deadline_override;
        }
        Ok(state)
    }

    /// The tokenization client for `network`, or a refusal naming it.
    ///
    /// # Errors
    ///
    /// Returns `422 rejected` with `unsupported_network` when the deployment
    /// holds no client for `network`.
    pub fn tokenizer(&self, network: Network) -> Result<&AlpacaTokenizationService, Failure> {
        self.tokenizers.get(&network).ok_or_else(|| {
            Failure::rejected(
                RejectionReason::UnsupportedNetwork,
                format!(
                    "network {} is not configured on this deployment",
                    network.as_str()
                ),
            )
        })
    }

    /// The deadline this state runs `operation` under.
    #[must_use]
    pub fn deadline(&self, operation: Operation) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(deadline) = self.deadline_override {
            return deadline;
        }
        operation.deadline()
    }

    /// Admits a call: refused at shutdown, for a disabled operation, for a
    /// human mutation without a reason, and for a human call past this
    /// minute's budget.
    fn admit(&self, call: &Call, intent: &Intent) -> Result<(), Failure> {
        let operation = call.operation;
        if self.shutdown.is_cancelled() {
            return Err(Failure::new(
                ErrorCode::Unavailable,
                "the gateway is shutting down",
            ));
        }
        if self.config.is_disabled(operation) {
            return Err(Failure::new(
                ErrorCode::CapabilityDisabled,
                format!("{operation} is disabled in this deployment"),
            ));
        }
        let tier = call.principal.tier;
        if tier == Tier::Write
            && operation.mutates()
            && intent
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(Failure::invalid(
                "a human mutation needs a non blank reason",
            ));
        }
        if !tier.is_human() {
            return Ok(());
        }
        self.budget
            .take(budget::cost(operation))
            .map_err(|wait| Failure {
                retry_after: Some(wait),
                ..Failure::new(
                    ErrorCode::Backpressure,
                    "human request budget spent for this minute",
                )
            })
    }

    /// Answers `call` with `failure` and writes its answered record without
    /// Alpaca traffic: a refusal decided before any work ran, or an answer
    /// without the work's result. A mutation's failure carries its outcome.
    #[must_use]
    pub fn refuse(&self, call: &Call, intent: &Intent, failure: Failure) -> Failure {
        let failure = Failure {
            request_id: call.request_id,
            ..failure.for_mutation(call.operation)
        };
        self.audit.emit(&self.event(
            call,
            intent,
            AuditPhase::Answered,
            &Traffic::default(),
            Err(&failure),
        ));
        failure
    }

    /// [`Self::run`] for a read, whose answer names no Alpaca object.
    pub async fn read<T, Build, Work>(&self, call: Call, intent: Intent, read: Build) -> Response
    where
        T: Serialize + Send + 'static,
        Build: FnOnce(Self) -> Work,
        Work: Future<Output = Result<T, Failure>> + Send + 'static,
    {
        let read = read(self.clone());
        self.run(call, intent, |_, _| async move {
            read.await.map(|body| Done {
                body,
                alpaca_object_ids: Vec::new(),
            })
        })
        .await
    }

    /// Runs an admitted call on a detached task, so a caller disconnect,
    /// the deadline or shutdown never aborts a request already sent to
    /// Alpaca. `work` gets the state and the deadline. It runs under a send
    /// gate that closes at the deadline, at shutdown, when the caller goes
    /// away, and before the caller is answered without the result: no Alpaca
    /// request and no credential mint starts after that, and one the gate
    /// holds back fails as never sent. The task writes the one record
    /// carrying the result and its Alpaca traffic: `answered` when the
    /// handler took the result, `settled` when the handler had stopped
    /// waiting. The handler answers by the deadline, or at once on shutdown;
    /// without the result it answers `outcome_unknown` for a mutation and
    /// `upstream_transient` for a read, and writes that answer's record.
    pub async fn run<T, Build, Work>(&self, call: Call, intent: Intent, work: Build) -> Response
    where
        T: Serialize + Send + 'static,
        Build: FnOnce(Self, tokio::time::Instant) -> Work,
        Work: Future<Output = Result<Done<T>, Failure>> + Send + 'static,
    {
        if let Err(failure) = self.admit(&call, &intent) {
            return self.refuse(&call, &intent, failure).into_response();
        }
        let operation = call.operation;
        let deadline = tokio::time::Instant::now() + self.deadline(operation);
        let shutdown = self.shutdown.clone();
        let gate = SendGate::new(move || {
            !shutdown.is_cancelled() && tokio::time::Instant::now() < deadline
        });
        let work = request_id::collect(request_id::gated(
            gate.clone(),
            work(self.clone(), deadline),
        ));
        let (sender, mut receiver) = oneshot::channel::<Result<Done<T>, Failure>>();
        let (state, task_call, task_intent) = (self.clone(), call.clone(), intent.clone());
        self.tasks.spawn(async move {
            let (result, traffic) = work.await;
            let result = result.map_err(|failure| failure.for_mutation(operation));
            let mut event = state.event(
                &task_call,
                &task_intent,
                AuditPhase::Answered,
                &traffic,
                result
                    .as_ref()
                    .map(|done| done.alpaca_object_ids.as_slice()),
            );
            if sender.send(result).is_err() {
                event.phase = AuditPhase::Settled;
            }
            state.audit.emit(&event);
        });

        // A caller that goes away drops this future; the guard then closes
        // the gate, so its recovery read cannot race a write still to come.
        let _close_on_drop = CloseOnDrop(gate.clone());
        let result = tokio::select! {
            result = &mut receiver => result.ok(),
            () = tokio::time::sleep_until(deadline) => None,
            () = self.shutdown.cancelled() => None,
        };
        // The gate first, so no request starts after the answer; a result
        // sent before the channel closed is still answered.
        let result = result.or_else(|| {
            gate.close();
            receiver.close();
            receiver.try_recv().ok()
        });
        match result {
            Some(Ok(done)) => success(call.request_id, &done.body),
            Some(Err(failure)) => Failure {
                request_id: call.request_id,
                ..failure
            }
            .into_response(),
            None => {
                warn!(%operation, request_id = %call.request_id, "Answered without the result");
                let failure = self.without_result(operation);
                self.refuse(&call, &intent, failure).into_response()
            }
        }
    }

    /// The answer to a call whose result came neither by its deadline nor
    /// before shutdown.
    fn without_result(&self, operation: Operation) -> Failure {
        let within = self.deadline(operation).as_secs();
        let message = match (self.shutdown.is_cancelled(), operation.mutates()) {
            (true, true) => {
                format!("the gateway is shutting down; {operation} may still complete at Alpaca")
            }
            (true, false) => format!("the gateway is shutting down; {operation} was not finished"),
            (false, true) => format!(
                "{operation} did not finish within {within}s; it may still complete at Alpaca"
            ),
            (false, false) => format!("{operation} did not finish within {within}s"),
        };
        let code = if operation.mutates() {
            ErrorCode::OutcomeUnknown
        } else {
            ErrorCode::UpstreamTransient
        };
        Failure::new(code, message)
    }

    /// The audit record of `call`. A success carries the status of the last
    /// Alpaca API answer; a failure carries its own Alpaca status and the
    /// Alpaca objects it had already changed. The Alpaca ids are kept as
    /// [`audit_ids`] bounds them.
    fn event(
        &self,
        call: &Call,
        intent: &Intent,
        phase: AuditPhase,
        traffic: &Traffic,
        result: Result<&[String], &Failure>,
    ) -> AuditEvent {
        let (object_ids, outcome, code, rejection, alpaca_status) = match result {
            Ok(object_ids) => (
                object_ids,
                call.operation.mutates().then_some(Outcome::Applied),
                None,
                None,
                traffic.last_status,
            ),
            Err(failure) => (
                failure.alpaca_object_ids.as_slice(),
                failure.outcome,
                Some(failure.code),
                failure.reason,
                failure.alpaca_status,
            ),
        };

        let elapsed = call.started.elapsed().as_millis();
        AuditEvent {
            request_id: call.request_id,
            phase,
            at: Utc::now(),
            deployment: DEPLOYMENT.to_string(),
            environment: self.config.environment.as_str().to_string(),
            account_id: self.config.broker.account_id.to_string(),
            principal: call.principal.subject.clone(),
            principal_email: call.principal.email.clone(),
            tier: call.principal.tier,
            on_behalf_of: call.on_behalf_of.clone(),
            operation: call.operation,
            key: intent.key.clone(),
            reason: intent.reason.clone(),
            request_digest: intent.digest.clone(),
            summary: intent.summary.clone(),
            alpaca_status,
            alpaca_request_ids: audit_ids(&traffic.request_ids),
            alpaca_object_id: (!object_ids.is_empty()).then(|| audit_ids(object_ids).join(",")),
            outcome,
            code,
            rejection,
            latency_ms: u64::try_from(elapsed).unwrap_or(u64::MAX),
            gateway_version: self.version.clone(),
        }
    }
}

/// Closes a call's send gate when the handler is done with it, including
/// when the caller goes away and the handler future is dropped. Closing after
/// the work finished changes nothing.
struct CloseOnDrop(SendGate);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn success<T: Serialize>(request_id: Uuid, body: &T) -> Response {
    let mut response = Json(body).into_response();
    if let Ok(value) = HeaderValue::from_str(&request_id.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// What a request is about, for the audit record: the key a read looked up
/// or a mutation is keyed by, and a mutation's reason, digest and summary.
#[derive(Debug, Clone, Default)]
pub struct Intent {
    pub key: Option<String>,
    pub reason: Option<String>,
    pub digest: Option<String>,
    pub summary: BTreeMap<String, String>,
}

impl Intent {
    /// Starts an intent for `request`, digesting its canonical JSON.
    #[must_use]
    pub fn of<Request: Serialize>(request: &Request) -> Self {
        let digest = serde_json::to_vec(request)
            .ok()
            .map(|bytes| hex::encode(Sha256::digest(bytes)));
        Self {
            digest,
            ..Self::default()
        }
    }

    /// Keeps the key for the audit record, at most its first
    /// [`AUDIT_FIELD_MAX`] characters.
    #[must_use]
    pub fn key(mut self, key: &impl std::fmt::Display) -> Self {
        self.key = Some(capped(key));
        self
    }

    /// Keeps the caller's reason for the audit record, at most its first
    /// [`AUDIT_FIELD_MAX`] characters.
    #[must_use]
    pub fn reason(mut self, reason: Option<&str>) -> Self {
        self.reason = reason.map(|reason| capped(&reason));
        self
    }

    /// Keeps `value` in the audit summary under `field`, at most its first
    /// [`AUDIT_FIELD_MAX`] characters.
    #[must_use]
    pub fn note(mut self, field: &str, value: &impl std::fmt::Display) -> Self {
        self.summary.insert(field.to_string(), capped(value));
        self
    }
}

/// A finished call: the answer body and the Alpaca objects a mutation
/// created or touched, as [`Failure::alpaca_object_ids`] names a failure's.
pub struct Done<T> {
    pub body: T,
    pub alpaca_object_ids: Vec<String>,
}

impl<T> Done<T> {
    pub fn new(body: T, alpaca_object_id: &impl std::fmt::Display) -> Self {
        Self {
            body,
            alpaca_object_ids: vec![alpaca_object_id.to_string()],
        }
    }
}

/// One request: its id, operation and verified caller.
#[derive(Debug, Clone)]
pub struct Call {
    pub request_id: Uuid,
    pub operation: Operation,
    pub principal: Principal,
    pub on_behalf_of: Option<String>,
    pub started: Instant,
}

impl Call {
    /// The call of the request behind `parts`, built once per request and
    /// kept in its extensions, so a refusal raised by a later extractor
    /// answers and audits under the same request id.
    ///
    /// # Errors
    ///
    /// Returns a failure when the route carries no operation or no
    /// verified caller.
    pub fn of(parts: &mut Parts) -> Result<Self, Failure> {
        if let Some(call) = parts.extensions.get::<Self>() {
            return Ok(call.clone());
        }
        let operation = *parts
            .extensions
            .get::<Operation>()
            .ok_or_else(|| Failure::new(ErrorCode::Unavailable, "route carries no operation"))?;
        let principal = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::Unauthenticated, "no verified caller"))?;
        let on_behalf_of = parts
            .headers
            .get(ON_BEHALF_OF_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| capped(&value));

        let call = Self {
            request_id: Uuid::new_v4(),
            operation,
            principal,
            on_behalf_of,
            started: Instant::now(),
        };
        parts.extensions.insert(call.clone());
        Ok(call)
    }
}

impl FromRequestParts<AppState> for Call {
    type Rejection = Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Self::of(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_caller_value_the_audit_keeps_stops_at_the_field_cap() {
        let long = "é".repeat(AUDIT_FIELD_MAX + 1);
        let capped = "é".repeat(AUDIT_FIELD_MAX);
        let short = "é".repeat(AUDIT_FIELD_MAX - 1);

        let intent = Intent::default()
            .key(&long)
            .reason(Some(&long))
            .note("counterparty", &long)
            .note("symbol", &short);

        assert_eq!(intent.key.as_deref(), Some(capped.as_str()));
        assert_eq!(intent.reason.as_deref(), Some(capped.as_str()));
        assert_eq!(intent.summary["counterparty"], capped);
        assert_eq!(intent.summary["symbol"], short);
    }

    #[test]
    fn the_audit_keeps_the_first_alpaca_ids_each_cut_at_the_id_cap() {
        let many: Vec<String> = (0..=AUDIT_IDS_MAX).map(|n| n.to_string()).collect();
        assert_eq!(audit_ids(&many), many[..AUDIT_IDS_MAX]);

        let long = vec![format!("{}x", "é".repeat(AUDIT_ID_MAX))];
        assert_eq!(audit_ids(&long), ["é".repeat(AUDIT_ID_MAX)]);
    }
}
