//! Shared state, the startup account check, and the runner every handler
//! goes through: admission (shutdown, capability switch, human budget), the
//! operation deadline, detached mutations, and audit.

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
use st0x_alpaca::request_id::{self, Traffic, TrafficHandle};
use st0x_alpaca::tokenization::{AlpacaTokenizationError, AlpacaTokenizationService};
use st0x_alpaca::wallet::{AlpacaWalletError, AlpacaWalletService};
use st0x_alpaca_gateway_api::{
    AuditEvent, AuditPhase, ErrorCode, ON_BEHALF_OF_HEADER, Operation, Outcome, REQUEST_ID_HEADER,
    RejectionReason, Tier,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

use crate::answer::Failure;
use crate::audit::AuditSink;
use crate::auth::Principal;
use crate::budget::{Charge, HumanBudget};
use crate::config::GatewayConfig;

/// Longest `X-On-Behalf-Of` value kept in the audit record.
const ON_BEHALF_OF_MAX: usize = 256;

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
    /// Detached mutations; shutdown waits for them.
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

/// Why a mutation stopped waiting for its result.
enum Abandoned {
    Deadline,
    Shutdown,
}

/// What the detached mutation task hands back: its result and the Alpaca
/// traffic it sent.
type Settled<T> = (Result<Done<T>, Failure>, Traffic);

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
            profile = ?config.profile,
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

    /// Admits a call, reserving the most human budget units it can spend.
    /// The charge comes back so the runner can settle it once the work ends.
    ///
    /// A cost above the whole budget can never be admitted, so its refusal
    /// is not retryable and names the limit. Config validation refuses such
    /// a budget; this keeps a state built around it from looping callers.
    fn admit(&self, call: &Call) -> Result<Option<Charge>, Failure> {
        if self.shutdown.is_cancelled() {
            return Err(Failure::new(
                ErrorCode::Unavailable,
                "the gateway is shutting down",
            ));
        }
        if self.config.is_disabled(call.operation) {
            return Err(Failure::new(
                ErrorCode::CapabilityDisabled,
                format!("{} is disabled in this deployment", call.operation),
            ));
        }
        if !call.principal.tier.is_human() {
            return Ok(None);
        }
        let operation = call.operation;
        let cost = operation.human_budget_cost();
        self.budget.take(cost).map(Some).map_err(|wait| {
            let limit = self.config.human_budget_per_minute;
            if cost > limit {
                Failure {
                    retryable: false,
                    ..Failure::new(
                        ErrorCode::Backpressure,
                        format!(
                            "{operation} reserves {cost} units, more than the human budget of \
                             {limit} requests per minute"
                        ),
                    )
                }
            } else {
                Failure {
                    retry_after: Some(wait),
                    ..Failure::new(
                        ErrorCode::Backpressure,
                        "human request budget spent for this minute",
                    )
                }
            }
        })
    }

    /// Keeps the admission units of the Alpaca requests the work sent and
    /// gives back the rest.
    fn settle_charge(&self, charge: Option<Charge>, traffic: &Traffic) {
        if let Some(charge) = charge {
            self.budget.settle(charge, traffic.requests_sent);
        }
    }

    /// Finalizes a refusal decided before any work ran: `not_applied` on a
    /// mutation, the call's request id, and the answered audit record.
    #[must_use]
    pub fn refuse(&self, call: &Call, intent: &Intent, failure: Failure) -> Failure {
        let failure = if call.operation.mutates() {
            failure.for_mutation(call.operation)
        } else {
            failure
        };
        let failure = Failure {
            request_id: call.request_id,
            ..failure
        };
        self.emit(
            call,
            intent,
            AuditPhase::Answered,
            &Traffic::default(),
            Err(&failure),
        );
        failure
    }

    /// Runs a read under its deadline and audits it under `intent`, whose
    /// key names what was looked up. Shutdown ends it at once with
    /// `upstream_transient`. A read its caller abandons still settles its
    /// charge and writes its answered record, marked abandoned.
    pub async fn read<T, Work>(&self, call: Call, intent: Intent, work: Work) -> Response
    where
        T: Serialize,
        Work: Future<Output = Result<T, Failure>>,
    {
        let charge = match self.admit(&call) {
            Ok(charge) => charge,
            Err(failure) => return self.refuse(&call, &intent, failure).into_response(),
        };
        let mut open = OpenRead {
            state: self,
            call: &call,
            intent: &intent,
            traffic: TrafficHandle::default(),
            charge,
            open: true,
        };

        let operation = call.operation;
        let deadline = self.deadline(operation);
        // Boxed: the Alpaca client futures are large, and every handler
        // future would otherwise carry this one inline.
        let result = Box::pin(request_id::collect_into(open.traffic.clone(), async {
            tokio::select! {
                finished = tokio::time::timeout(deadline, work) => {
                    finished.unwrap_or_else(|_| {
                        Err(Failure::new(
                            ErrorCode::UpstreamTransient,
                            format!("{operation} did not finish within {}s", deadline.as_secs()),
                        ))
                    })
                }
                () = self.shutdown.cancelled() => Err(Failure::new(
                    ErrorCode::UpstreamTransient,
                    format!("the gateway is shutting down; {operation} was not finished"),
                )),
            }
        }))
        .await;
        let traffic = open.close();

        match result {
            Ok(body) => {
                self.emit(&call, &intent, AuditPhase::Answered, &traffic, Ok(None));
                success(call.request_id, &body)
            }
            Err(failure) => {
                let failure = Failure {
                    request_id: call.request_id,
                    ..failure
                };
                self.emit(
                    &call,
                    &intent,
                    AuditPhase::Answered,
                    &traffic,
                    Err(&failure),
                );
                failure.into_response()
            }
        }
    }

    /// Runs a mutation on a detached task, so a caller disconnect, an
    /// expired deadline or shutdown never aborts a request already sent to
    /// Alpaca, and answers by the deadline (or at once on shutdown) either
    /// way. The admission charge is settled when the work ends, answered or
    /// not.
    pub async fn mutate<T, Work>(&self, call: Call, intent: Intent, work: Work) -> Response
    where
        T: Serialize + Send + 'static,
        Work: Future<Output = Result<Done<T>, Failure>> + Send + 'static,
    {
        let operation = call.operation;
        let charge = match self.admit(&call) {
            Ok(charge) => charge,
            Err(failure) => return self.refuse(&call, &intent, failure).into_response(),
        };
        if call.principal.tier == Tier::Write
            && intent
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            let failure = Failure::invalid("a human mutation needs a non blank reason");
            self.settle_charge(charge, &Traffic::default());
            return self.refuse(&call, &intent, failure).into_response();
        }

        let (sender, receiver) = oneshot::channel::<Settled<T>>();
        let state = self.clone();
        let settle_call = call.clone();
        let settle_intent = intent.clone();
        self.tasks.spawn(async move {
            let (result, traffic) = request_id::collect(work).await;
            // Before the answer, so the caller's next request sees the units.
            state.settle_charge(charge, &traffic);
            // The handler closes the channel before it answers without a
            // result, so a failed send means nobody will publish this one.
            if let Err((result, traffic)) = sender.send((result, traffic)) {
                let record = |outcome: Result<Option<String>, &Failure>| {
                    state.emit(
                        &settle_call,
                        &settle_intent,
                        AuditPhase::Settled,
                        &traffic,
                        outcome,
                    );
                };
                match result {
                    Ok(done) => record(Ok(done.alpaca_object_id)),
                    Err(failure) => record(Err(&failure.for_mutation(operation))),
                }
            }
        });

        let (result, traffic) = self.wait_for_mutation(&call, receiver).await;

        match result {
            Ok(done) => {
                self.emit(
                    &call,
                    &intent,
                    AuditPhase::Answered,
                    &traffic,
                    Ok(done.alpaca_object_id.clone()),
                );
                success(call.request_id, &done.body)
            }
            Err(failure) => {
                let failure = Failure {
                    request_id: call.request_id,
                    ..failure
                };
                self.emit(
                    &call,
                    &intent,
                    AuditPhase::Answered,
                    &traffic,
                    Err(&failure),
                );
                failure.into_response()
            }
        }
    }

    /// Waits for the detached mutation until its deadline or shutdown,
    /// whichever comes first, and turns a missing result into
    /// `outcome_unknown`.
    async fn wait_for_mutation<T>(
        &self,
        call: &Call,
        mut receiver: oneshot::Receiver<Settled<T>>,
    ) -> Settled<T> {
        let operation = call.operation;
        let waited = tokio::select! {
            settled = &mut receiver => Ok(settled),
            () = tokio::time::sleep(self.deadline(operation)) => Err(Abandoned::Deadline),
            () = self.shutdown.cancelled() => Err(Abandoned::Shutdown),
        };
        // A result that landed while the deadline or shutdown fired is still
        // answered; after `close` a later one goes to the settled record.
        let waited = waited.or_else(|why| {
            receiver.close();
            receiver.try_recv().map(Ok).map_err(|_| why)
        });

        match waited {
            Ok(Ok((result, traffic))) => (
                result.map_err(|failure| failure.for_mutation(operation)),
                traffic,
            ),
            Ok(Err(_)) => (
                Err(Failure::new(
                    ErrorCode::OutcomeUnknown,
                    "the mutation task ended without a result",
                )
                .for_mutation(operation)),
                Traffic::default(),
            ),
            Err(Abandoned::Deadline) => {
                warn!(%operation, request_id = %call.request_id, "Mutation passed its deadline");
                (Err(Failure::deadline_passed(operation)), Traffic::default())
            }
            Err(Abandoned::Shutdown) => {
                warn!(%operation, request_id = %call.request_id, "Mutation still running at shutdown");
                (
                    Err(Failure::new(
                        ErrorCode::OutcomeUnknown,
                        format!(
                            "the gateway is shutting down; {operation} may still complete at Alpaca"
                        ),
                    )
                    .for_mutation(operation)),
                    Traffic::default(),
                )
            }
        }
    }

    /// Writes the audit record of `call`.
    fn emit(
        &self,
        call: &Call,
        intent: &Intent,
        phase: AuditPhase,
        traffic: &Traffic,
        result: Result<Option<String>, &Failure>,
    ) {
        self.audit
            .emit(&self.event(call, intent, phase, traffic, result));
    }

    /// The audit record of `call`. A success carries the status of the last
    /// Alpaca answer; a failure carries its own status, else that of the
    /// last Alpaca answer (a body that did not parse, a network check after
    /// a 200), and the Alpaca objects it had already changed.
    fn event(
        &self,
        call: &Call,
        intent: &Intent,
        phase: AuditPhase,
        traffic: &Traffic,
        result: Result<Option<String>, &Failure>,
    ) -> AuditEvent {
        let (alpaca_object_id, outcome, code, rejection, alpaca_status) = match result {
            Ok(object_id) => (
                object_id,
                call.operation.mutates().then_some(Outcome::Applied),
                None,
                None,
                traffic.last_status,
            ),
            Err(failure) => (
                (!failure.alpaca_object_ids.is_empty())
                    .then(|| failure.alpaca_object_ids.join(",")),
                failure.outcome,
                Some(failure.code),
                failure.reason,
                failure.alpaca_status.or(traffic.last_status),
            ),
        };

        let elapsed = call.started.elapsed().as_millis();
        AuditEvent {
            request_id: call.request_id,
            phase,
            at: Utc::now(),
            deployment: self.config.profile.deployment().to_string(),
            profile: self.config.profile,
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
            alpaca_request_ids: traffic.request_ids.clone(),
            alpaca_object_id,
            outcome,
            code,
            rejection,
            latency_ms: u64::try_from(elapsed).unwrap_or(u64::MAX),
            abandoned: false,
            gateway_version: self.version.clone(),
        }
    }
}

/// A read between admission and its answer. When the server drops the
/// request future first (the caller went away), dropping this settles the
/// charge from the traffic sent until then and writes the answered record
/// the read would otherwise never write, marked abandoned.
struct OpenRead<'a> {
    state: &'a AppState,
    call: &'a Call,
    intent: &'a Intent,
    /// Filled while the work runs, so a drop sees what it sent.
    traffic: TrafficHandle,
    charge: Option<Charge>,
    open: bool,
}

impl OpenRead<'_> {
    /// Ends the read: settles the charge and returns the traffic sent, for
    /// the answer's audit record.
    fn close(&mut self) -> Traffic {
        self.open = false;
        let traffic = self.traffic.snapshot();
        self.state.settle_charge(self.charge.take(), &traffic);
        traffic
    }
}

impl Drop for OpenRead<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let traffic = self.close();
        warn!(
            operation = %self.call.operation,
            request_id = %self.call.request_id,
            "Read abandoned by its caller"
        );
        let event = self.state.event(
            self.call,
            self.intent,
            AuditPhase::Answered,
            &traffic,
            Ok(None),
        );
        self.state.audit.emit(&AuditEvent {
            abandoned: true,
            ..event
        });
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

    #[must_use]
    pub fn key(mut self, key: &impl std::fmt::Display) -> Self {
        self.key = Some(key.to_string());
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: Option<&str>) -> Self {
        self.reason = reason.map(str::to_string);
        self
    }

    #[must_use]
    pub fn note(mut self, field: &str, value: &impl std::fmt::Display) -> Self {
        self.summary.insert(field.to_string(), value.to_string());
        self
    }
}

/// A finished mutation: the answer body and the Alpaca object it created or
/// touched.
pub struct Done<T> {
    pub body: T,
    pub alpaca_object_id: Option<String>,
}

impl<T> Done<T> {
    pub fn new(body: T, alpaca_object_id: &impl std::fmt::Display) -> Self {
        Self {
            body,
            alpaca_object_id: Some(alpaca_object_id.to_string()),
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
            .map(|value| value.chars().take(ON_BEHALF_OF_MAX).collect());

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
