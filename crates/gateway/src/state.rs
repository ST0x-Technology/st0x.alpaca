//! Shared state, the startup account check, and the runner every handler
//! goes through: admission (capability switch, human budget), the operation
//! deadline, detached mutations, and audit.

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
use st0x_alpaca::tokenization::{AlpacaTokenizationError, AlpacaTokenizationService};
use st0x_alpaca::wallet::{AlpacaWalletError, AlpacaWalletService};
use st0x_alpaca_gateway_api::{
    AuditEvent, AuditPhase, ErrorCode, ON_BEHALF_OF_HEADER, Operation, Outcome, REQUEST_ID_HEADER,
    Tier,
};
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};
use uuid::Uuid;

use crate::answer::Failure;
use crate::audit::AuditSink;
use crate::auth::Principal;
use crate::budget::HumanBudget;
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
    pub version: String,
    /// Replaces every operation deadline. Tests only; production passes
    /// `None` and runs the catalog deadlines.
    pub deadline_override: Option<Duration>,
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
        Self::connect_with_deadline(config, audit, version, None).await
    }

    /// [`Self::connect`] with every operation deadline replaced, so tests
    /// can exercise the deadline path without waiting for it.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn connect_with_deadline(
        config: GatewayConfig,
        audit: Arc<dyn AuditSink>,
        version: String,
        deadline_override: Option<Duration>,
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
            version,
            deadline_override,
        })))
    }

    /// The tokenization client for `network`, or a refusal naming the
    /// configured networks.
    ///
    /// # Errors
    ///
    /// Returns a `400` when the deployment holds no client for `network`.
    pub fn tokenizer(&self, network: Network) -> Result<&AlpacaTokenizationService, Failure> {
        self.tokenizers.get(&network).ok_or_else(|| {
            Failure::invalid(format!(
                "network {} is not configured on this deployment",
                network.as_str()
            ))
        })
    }

    fn deadline(&self, operation: Operation) -> Duration {
        self.deadline_override
            .unwrap_or_else(|| operation.deadline())
    }

    fn admit(&self, call: &Call) -> Result<(), Failure> {
        if self.config.is_disabled(call.operation) {
            return Err(Failure::new(
                ErrorCode::CapabilityDisabled,
                format!("{} is disabled in this deployment", call.operation),
            ));
        }
        if call.principal.tier.is_human()
            && let Err(wait) = self.budget.take(call.operation.human_budget_cost())
        {
            return Err(Failure {
                retry_after: Some(wait),
                ..Failure::new(
                    ErrorCode::Backpressure,
                    "human request budget spent for this minute",
                )
            });
        }
        Ok(())
    }

    /// Runs a read under its deadline and audits it.
    pub async fn read<T, Work>(&self, call: Call, work: Work) -> Response
    where
        T: Serialize,
        Work: Future<Output = Result<T, Failure>>,
    {
        let result = match self.admit(&call) {
            Err(failure) => Err(failure),
            Ok(()) => tokio::time::timeout(self.deadline(call.operation), work)
                .await
                .unwrap_or_else(|_| {
                    Err(Failure::new(
                        ErrorCode::UpstreamTransient,
                        format!(
                            "{} did not finish within {}s",
                            call.operation,
                            self.deadline(call.operation).as_secs()
                        ),
                    ))
                }),
        };

        match result {
            Ok(body) => {
                self.emit(&call, &Intent::default(), AuditPhase::Answered, Ok(None));
                success(call.request_id, &body)
            }
            Err(failure) => {
                let failure = Failure {
                    request_id: call.request_id,
                    ..failure
                };
                self.emit(
                    &call,
                    &Intent::default(),
                    AuditPhase::Answered,
                    Err(&failure),
                );
                failure.into_response()
            }
        }
    }

    /// Runs a mutation on a detached task, so a caller disconnect or an
    /// expired deadline never aborts a request already sent to Alpaca, and
    /// answers by the deadline either way.
    pub async fn mutate<T, Work>(&self, call: Call, intent: Intent, work: Work) -> Response
    where
        T: Serialize + Send + 'static,
        Work: Future<Output = Result<Done<T>, Failure>> + Send + 'static,
    {
        let operation = call.operation;
        let refusal = self.admit(&call).err().or_else(|| {
            (call.principal.tier == Tier::Write
                && intent
                    .reason
                    .as_deref()
                    .is_none_or(|reason| reason.trim().is_empty()))
            .then(|| Failure::invalid("a human mutation needs a non blank reason"))
        });
        if let Some(failure) = refusal {
            let failure = Failure {
                request_id: call.request_id,
                ..failure.for_mutation(operation)
            };
            self.emit(&call, &intent, AuditPhase::Answered, Err(&failure));
            return failure.into_response();
        }

        let (sender, receiver) = oneshot::channel();
        let settle = self.clone();
        let settle_call = call.clone();
        let settle_intent = intent.clone();
        self.tasks.spawn(async move {
            let result = work.await;
            if let Err(unsent) = sender.send(result) {
                // Nobody is waiting any more: the answer already went out as
                // outcome_unknown, or the caller left. Record what happened.
                match unsent {
                    Ok(done) => settle.emit(
                        &settle_call,
                        &settle_intent,
                        AuditPhase::Settled,
                        Ok(done.alpaca_object_id),
                    ),
                    Err(failure) => settle.emit(
                        &settle_call,
                        &settle_intent,
                        AuditPhase::Settled,
                        Err(&failure.for_mutation(operation)),
                    ),
                }
            }
        });

        let result = match tokio::time::timeout(self.deadline(operation), receiver).await {
            Ok(Ok(result)) => result.map_err(|failure| failure.for_mutation(operation)),
            Ok(Err(_)) => Err(Failure::new(
                ErrorCode::OutcomeUnknown,
                "the mutation task ended without a result",
            )
            .for_mutation(operation)),
            Err(_) => {
                warn!(%operation, request_id = %call.request_id, "Mutation passed its deadline");
                Err(Failure::deadline_passed(operation))
            }
        };

        match result {
            Ok(done) => {
                self.emit(
                    &call,
                    &intent,
                    AuditPhase::Answered,
                    Ok(done.alpaca_object_id.clone()),
                );
                success(call.request_id, &done.body)
            }
            Err(failure) => {
                let failure = Failure {
                    request_id: call.request_id,
                    ..failure
                };
                self.emit(&call, &intent, AuditPhase::Answered, Err(&failure));
                failure.into_response()
            }
        }
    }

    fn emit(
        &self,
        call: &Call,
        intent: &Intent,
        phase: AuditPhase,
        result: Result<Option<String>, &Failure>,
    ) {
        let (alpaca_object_id, outcome, code, rejection, alpaca_status) = match result {
            Ok(object_id) => (
                object_id,
                call.operation.mutates().then_some(Outcome::Applied),
                None,
                None,
                None,
            ),
            Err(failure) => (
                None,
                failure.outcome,
                Some(failure.code),
                failure.reason,
                failure.alpaca_status,
            ),
        };

        let elapsed = call.started.elapsed().as_millis();
        self.audit.emit(&AuditEvent {
            request_id: call.request_id,
            phase,
            at: Utc::now(),
            deployment: self.config.profile.deployment().to_string(),
            profile: self.config.profile,
            environment: self.config.environment.clone(),
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
            alpaca_object_id,
            outcome,
            code,
            rejection,
            latency_ms: u64::try_from(elapsed).unwrap_or(u64::MAX),
            gateway_version: self.version.clone(),
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

/// What a mutation is about, for the audit record.
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

impl FromRequestParts<AppState> for Call {
    type Rejection = Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
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

        Ok(Self {
            request_id: Uuid::new_v4(),
            operation,
            principal,
            on_behalf_of,
            started: Instant::now(),
        })
    }
}
