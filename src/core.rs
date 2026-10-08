//! Shared Alpaca transport: HTTP client construction, the dual
//! authentication scheme (HTTP Basic plus the legacy `APCA-API-KEY-ID` /
//! `APCA-API-SECRET-KEY` headers), the retry policy, the error taxonomy,
//! and wire types shared across surface modules.

#[cfg(feature = "issuer")]
use backon::{BackoffBuilder, ExponentialBuilder};
use serde::{Deserialize, Serialize};
#[cfg(feature = "issuer")]
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
#[cfg(feature = "issuer")]
use tokio::time::Instant;
#[cfg(feature = "issuer")]
use url::Url;

#[cfg(feature = "issuer")]
use crate::auth::AuthRuntime;
#[cfg(feature = "issuer")]
use crate::auth::KmsJwtError;
#[cfg(feature = "issuer")]
use crate::endpoint::{EndpointError, EndpointRole, resolve_segments, validate_origin};
#[cfg(feature = "issuer")]
use crate::rate_limit::MAX_RETRY_AFTER_HOLD;
#[cfg(feature = "issuer")]
use crate::request_id::{self, GateClosed, SendError};

/// Alpaca API credentials applied to every request.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum AlpacaAuth {
    Basic {
        api_key: String,
        api_secret: String,
    },
    KmsJwt {
        client_id: String,
        kms_key_version: String,
    },
    PrivateKeyJwt {
        client_id: String,
        private_key_pem: String,
    },
}

impl std::fmt::Debug for AlpacaAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { .. } => formatter
                .debug_struct("Basic")
                .field("api_key", &"[REDACTED]")
                .field("api_secret", &"[REDACTED]")
                .finish(),
            Self::KmsJwt {
                client_id,
                kms_key_version,
            } => formatter
                .debug_struct("KmsJwt")
                .field("client_id", client_id)
                .field("kms_key_version", kms_key_version)
                .finish(),
            Self::PrivateKeyJwt { client_id, .. } => formatter
                .debug_struct("PrivateKeyJwt")
                .field("client_id", client_id)
                .field("private_key_pem", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Longest total time one [`AlpacaClient::with_retry`] call waits on
/// `Retry-After` deadlines. A longer hint returns the rate-limit error at
/// once, so the caller's own retry cadence (a job re-fetch, a poll tick)
/// takes over instead of one call parking for minutes.
#[cfg(feature = "issuer")]
const RETRY_AFTER_BUDGET: Duration = Duration::from_secs(30);

/// Upper bound, in milliseconds, of the random delay each caller adds after
/// a shared `Retry-After` deadline, so callers parked on one deadline do not
/// all send at the same instant when it passes.
#[cfg(feature = "issuer")]
const RETRY_AFTER_JITTER_MS: u64 = 1_000;

/// HTTP client for Alpaca's issuer APIs.
///
/// Carries the configured base URL, account id, credentials, and retry
/// policy. Surface modules send requests through the crate-private
/// authenticated builders, which build the URL from percent-encoded path
/// segments on the configured origin, and wrap idempotent calls
/// in [`Self::with_retry`].
#[cfg(feature = "issuer")]
#[derive(Clone)]
pub struct AlpacaClient {
    http: reqwest::Client,
    base_url: Url,
    account_id: String,
    auth: AuthRuntime,
    max_retries: usize,
    /// Shared by every clone: no request leaves this client before the
    /// latest `Retry-After` deadline any caller observed.
    not_before: Arc<StdMutex<Option<(Instant, Duration)>>>,
}

#[cfg(feature = "issuer")]
impl std::fmt::Debug for AlpacaClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlpacaClient")
            .field("base_url", &self.base_url.as_str())
            .field("account_id", &self.account_id)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "issuer")]
impl AlpacaClient {
    /// Builds a Basic-auth client with the given connection and request
    /// timeouts.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError::InvalidUrl`] for a base URL that is not HTTPS
    /// (or HTTP on a loopback host), or [`AlpacaError::Reqwest`] if the
    /// underlying HTTP client cannot be constructed.
    pub fn new(
        base_url: &str,
        account_id: String,
        api_key: String,
        api_secret: String,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AlpacaError> {
        Self::with_auth(
            base_url,
            account_id,
            AlpacaAuth::Basic {
                api_key,
                api_secret,
            },
            "",
            connect_timeout,
            request_timeout,
        )
    }

    /// Builds a client supporting Basic, KMS JWT, or local private-key JWT
    /// authentication.
    ///
    /// `token_url` is ignored for Basic auth and must name the environment's
    /// Alpaca authx token endpoint for either JWT variant.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError::InvalidUrl`] for an invalid base URL,
    /// [`AlpacaError::Jwt`] for an invalid token URL or JWT credentials, or
    /// [`AlpacaError::Reqwest`] when the HTTP client cannot be built.
    pub fn with_auth(
        base_url: &str,
        account_id: String,
        auth: AlpacaAuth,
        token_url: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AlpacaError> {
        let base_url = validate_origin(base_url, EndpointRole::BaseUrl)?;
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base_url,
            account_id,
            auth: AuthRuntime::build(auth, token_url)?,
            max_retries: 5,
            not_before: Arc::new(StdMutex::new(None)),
        })
    }

    /// Overrides the retry budget (defaults to 5 retries after the first
    /// attempt).
    #[must_use]
    pub const fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Configured base URL with any trailing slash removed.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base_url.as_str().trim_end_matches('/')
    }

    /// Alpaca account id used in endpoint paths.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Authenticated GET for literal path `segments` on the configured
    /// origin; each segment is percent-encoded.
    pub(crate) async fn get(
        &self,
        segments: &[&str],
    ) -> Result<reqwest::RequestBuilder, AlpacaError> {
        let url = resolve_segments(&self.base_url, segments)?;
        self.authenticate(self.http.get(url)).await
    }

    /// Authenticated POST for literal path `segments` on the configured
    /// origin; each segment is percent-encoded.
    pub(crate) async fn post(
        &self,
        segments: &[&str],
    ) -> Result<reqwest::RequestBuilder, AlpacaError> {
        let url = resolve_segments(&self.base_url, segments)?;
        self.authenticate(self.http.post(url)).await
    }

    /// Applies Alpaca's dual authentication to a request: HTTP Basic auth
    /// plus the legacy `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` headers, or
    /// the bearer token for either JWT mode. Alpaca's tokenization endpoints
    /// require both legacy header forms.
    async fn authenticate(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.auth.apply_wallet(builder).await.map_err(Into::into)
    }

    /// Runs `operation` under the shared retry policy: exponential backoff
    /// with jitter, up to `max_retries` retries, retrying only errors that
    /// [`AlpacaError::is_retryable`] classifies as transient.
    ///
    /// A `Retry-After` hint on any error (an API 429 or a rate-limited token
    /// mint) sets a deadline shared by every clone of this client, capped at
    /// five minutes. No attempt is sent before it. The call waits for the
    /// deadline, plus up to a second of jitter, while its
    /// total `Retry-After` wait stays within a 30-second budget; past that it
    /// returns the error at once. A call that starts inside a deadline longer
    /// than its budget returns [`AlpacaError::RateLimited`] without sending
    /// anything.
    ///
    /// # Errors
    ///
    /// Returns the final [`AlpacaError`] produced by `operation` once the
    /// error is non-retryable, the retry budget is exhausted, or a
    /// `Retry-After` deadline exceeds the wait budget.
    pub async fn with_retry<Value, Fut, Operation>(
        &self,
        operation: Operation,
    ) -> Result<Value, AlpacaError>
    where
        Operation: FnMut() -> Fut,
        Fut: Future<Output = Result<Value, AlpacaError>>,
    {
        self.with_retry_reporting(operation)
            .await
            .map_err(|failure| failure.error)
    }

    /// [`Self::with_retry`], also reporting whether any attempt may have
    /// reached Alpaca and the status of the last answer Alpaca gave.
    pub(crate) fn with_retry_reporting<Value, Fut, Operation>(
        &self,
        operation: Operation,
    ) -> impl Future<Output = Result<Value, IssuerCallError>>
    where
        Operation: FnMut() -> Fut,
        Fut: Future<Output = Result<Value, AlpacaError>>,
    {
        request_id::tracking_last_answer(self.retry_attempts(operation))
    }

    /// The retry loop of [`Self::with_retry_reporting`], run inside its
    /// [`request_id::tracking_last_answer`] scope.
    async fn retry_attempts<Value, Fut, Operation>(
        &self,
        mut operation: Operation,
    ) -> Result<Value, IssuerCallError>
    where
        Operation: FnMut() -> Fut,
        Fut: Future<Output = Result<Value, AlpacaError>>,
    {
        let mut delays = ExponentialBuilder::default()
            .with_max_times(self.max_retries)
            .with_jitter()
            .build();
        let mut budget = RETRY_AFTER_BUDGET;
        let mut written = false;

        let error = 'attempts: loop {
            // A different caller may extend the deadline while we sleep.
            while let Some(wait) = self.backpressure_wait() {
                if wait > budget {
                    break 'attempts AlpacaError::RateLimited {
                        body: "client is inside an earlier Retry-After deadline".to_string(),
                        retry_after: Some(wait),
                    };
                }
                let wait = (wait + retry_after_jitter()).min(budget);
                budget -= wait;
                tokio::time::sleep(wait).await;
            }
            let error = match operation().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            written |= may_have_reached_alpaca(&error);

            if let Some(retry_after) = error.backpressure().and_then(|hint| hint.retry_after) {
                self.hold_for(retry_after);
            }

            if !error.is_retryable() {
                break error;
            }
            let Some(delay) = delays.next() else {
                break error;
            };

            let wait = self.backpressure_wait().unwrap_or_default();
            if wait > budget {
                break error;
            }
            tokio::time::sleep(delay).await;
        };
        Err(IssuerCallError {
            written,
            alpaca_status: request_id::last_answer(),
            error,
        })
    }

    /// Time left until the shared `Retry-After` deadline, if one is ahead.
    fn backpressure_wait(&self) -> Option<Duration> {
        let not_before = *self
            .not_before
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        not_before
            .map(|(started, delay)| delay.saturating_sub(started.elapsed()))
            .filter(|wait| !wait.is_zero())
    }

    /// Extends the shared hold when `delay` exceeds the remaining wait.
    /// `delay` is capped at [`MAX_RETRY_AFTER_HOLD`]: while the hold runs no
    /// call reaches the server, so no success can shorten it.
    fn hold_for(&self, delay: Duration) {
        let delay = delay.min(MAX_RETRY_AFTER_HOLD);
        let mut not_before = self
            .not_before
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if not_before
            .is_none_or(|(started, current)| current.saturating_sub(started.elapsed()) < delay)
        {
            *not_before = Some((Instant::now(), delay));
        }
    }
}

/// Random delay in `[0, RETRY_AFTER_JITTER_MS)` milliseconds. Each
/// `RandomState` gets fresh keys, so hashing a constant through one is a
/// dependency-free random source; spreading callers needs no more than that.
#[cfg(feature = "issuer")]
fn retry_after_jitter() -> Duration {
    use std::hash::BuildHasher;

    let random = std::collections::hash_map::RandomState::new().hash_one(());
    Duration::from_millis(random % RETRY_AFTER_JITTER_MS)
}

/// Errors that can occur during Alpaca API operations.
#[cfg(feature = "issuer")]
#[derive(Debug, thiserror::Error)]
pub enum AlpacaError {
    #[error(transparent)]
    InvalidUrl(#[from] EndpointError),
    #[error("issuer request id is not a valid Idempotency-Key header value")]
    InvalidIdempotencyKey(#[source] reqwest::header::InvalidHeaderValue),
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Jwt(#[from] KmsJwtError),
    /// Failed to parse response after 200 OK - NOT retryable
    #[error("Failed to parse response: {source}")]
    Parse {
        body: String,
        #[source]
        source: serde_json::Error,
    },
    /// Authentication failed (401/403 response)
    #[error("Authentication failed: {0}")]
    Auth(String),
    /// Alpaca API returned an error response
    #[error("API error {status_code}: {body}")]
    Api { status_code: u16, body: String },
    #[error("API rate limited the request: {body}")]
    RateLimited {
        body: String,
        retry_after: Option<Duration>,
    },
    /// HTTP 404 from the keyed endpoint. Treated as "definitively absent" for
    /// recovery purposes (empirically verified 2026-06-12, not a published
    /// Alpaca guarantee). `body` contains the raw 404 response for operator
    /// diagnostics.
    #[error("Tokenization request not found: {id}, body: {body}")]
    RequestNotFound {
        id: TokenizationRequestId,
        body: String,
    },
    /// The keyed endpoint returned a request whose id differs from what was
    /// requested — non-retryable data integrity violation.
    #[error("Response id mismatch: requested {requested}, returned {returned}")]
    ResponseIdMismatch {
        requested: TokenizationRequestId,
        returned: TokenizationRequestId,
    },
    /// Local preflight: the network is not on the ITN network list
    /// (`issuer::itn`), so the request is refused before any HTTP call.
    #[error(
        "Network {network} is not on the Alpaca ITN TokenizationNetwork list -- see {reference}"
    )]
    UnsupportedTokenizationNetwork {
        network: Network,
        reference: &'static str,
    },
    #[error(transparent)]
    NotSent(#[from] GateClosed),
}

#[cfg(feature = "issuer")]
impl From<SendError> for AlpacaError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::Http(source) => Self::Reqwest(source),
            SendError::NotSent(closed) => Self::NotSent(closed),
        }
    }
}

#[cfg(feature = "issuer")]
impl AlpacaError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Reqwest(error) => !error.is_decode(),
            Self::Api { status_code, .. } => {
                matches!(status_code, 500..=599 | 429)
            }
            Self::RateLimited { .. } => true,
            Self::Jwt(error) => !error.is_deterministic(),
            Self::InvalidUrl(_)
            | Self::InvalidIdempotencyKey(_)
            | Self::UnsupportedTokenizationNetwork { .. }
            | Self::Parse { .. }
            | Self::Auth(_)
            | Self::RequestNotFound { .. }
            | Self::ResponseIdMismatch { .. }
            | Self::NotSent(_) => false,
        }
    }

    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match self {
            Self::RateLimited { retry_after, .. } => Some(Backpressure {
                retry_after: *retry_after,
            }),
            Self::Jwt(error) if error.is_rate_limited() => Some(Backpressure {
                retry_after: error.retry_after(),
            }),
            _ => None,
        }
    }

    #[must_use]
    pub fn permanence(&self) -> Permanence {
        match self {
            Self::Reqwest(error) if error.is_builder() || error.is_decode() => {
                Permanence::Permanent
            }
            Self::Jwt(error) if error.is_deterministic() => Permanence::Permanent,
            Self::Reqwest(_) | Self::Jwt(_) | Self::RateLimited { .. } => Permanence::Transient,
            Self::Api { status_code, .. } => status_permanence(*status_code),
            Self::InvalidUrl(_)
            | Self::InvalidIdempotencyKey(_)
            | Self::UnsupportedTokenizationNetwork { .. }
            | Self::Parse { .. }
            | Self::Auth(_)
            | Self::RequestNotFound { .. }
            | Self::ResponseIdMismatch { .. }
            // A closed send gate stays closed for the rest of its scope.
            | Self::NotSent(_) => Permanence::Permanent,
        }
    }
}

/// A failed issuer call, with whether it may have written at Alpaca and
/// what Alpaca last answered.
#[cfg(feature = "issuer")]
#[derive(Debug, thiserror::Error)]
#[error("Alpaca issuer call failed")]
pub struct IssuerCallError {
    /// `false` for a read, and for a POST when no attempt left this process
    /// or Alpaca answered each one with a definite rejection (a 4xx other
    /// than 408, and other than the status the endpoint also gives after
    /// it may have applied the request: 400 for a mint callback, 422 for a
    /// redeem). `true` once a POST attempt may have been written: any
    /// other answer, an answer lost after the request could have left, or a
    /// 2xx that did not read back.
    pub written: bool,
    /// The status of the last answer Alpaca gave any attempt of the call;
    /// `None` when no attempt got one (a local `Retry-After` hold, a
    /// connect failure, a credential mint that failed).
    pub alpaca_status: Option<u16>,
    #[source]
    pub error: AlpacaError,
}

#[cfg(feature = "issuer")]
impl IssuerCallError {
    /// Marks the call written when Alpaca's final answer is `status`, which
    /// the endpoint also gives once the request may have been applied. A
    /// 4xx ends the retry loop, so the final answer is the last attempt's.
    pub(crate) fn written_on(self, status: u16) -> Self {
        let written = self.written
            || matches!(self.error, AlpacaError::Api { status_code, .. } if status_code == status);
        Self { written, ..self }
    }
}

/// Whether a failed attempt may have reached Alpaca, by the rule of
/// [`IssuerCallError::written`].
#[cfg(feature = "issuer")]
fn may_have_reached_alpaca(error: &AlpacaError) -> bool {
    match error {
        AlpacaError::Api { status_code, .. } => {
            !(400..500).contains(status_code) || *status_code == 408
        }
        AlpacaError::Reqwest(source) => !(source.is_builder() || source.is_connect()),
        AlpacaError::Parse { .. } | AlpacaError::ResponseIdMismatch { .. } => true,
        AlpacaError::InvalidUrl(_)
        | AlpacaError::InvalidIdempotencyKey(_)
        | AlpacaError::Jwt(_)
        | AlpacaError::NotSent(_)
        | AlpacaError::UnsupportedTokenizationNetwork { .. }
        | AlpacaError::Auth(_)
        | AlpacaError::RateLimited { .. }
        | AlpacaError::RequestNotFound { .. } => false,
    }
}

/// A failure on the hop between a gateway client and the Alpaca gateway: no
/// answer, a timeout, the gateway itself unavailable, or a refusal the
/// gateway decided without relaying an Alpaca answer. Every error type a
/// gateway client can produce carries it as a `Gateway` variant. It is
/// backpressure only when the gateway asked for a wait
/// ([`retry_after`](Self::retry_after)), and its
/// [`permanence`](Self::permanence) follows the gateway's own classification,
/// so a definite refusal is not retried as if a fresh call could repair it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Alpaca gateway: {message}")]
pub struct GatewayHopError {
    /// What failed, for logs.
    pub message: String,
    /// A later call can succeed: the gateway said so, or no answer came.
    pub retryable: bool,
    /// A mutation may have reached Alpaca: reconcile before acting on it.
    pub outcome_unknown: bool,
    /// Resending the same mutation with the same idempotency key is safe.
    pub retryable_with_same_key: bool,
    /// How long the gateway asked callers to hold off before the next call,
    /// when it relayed a wait (a throttled credential mint, say).
    pub retry_after: Option<Duration>,
}

impl GatewayHopError {
    /// A hop that got no answer from the gateway. Retryable, with no wait;
    /// whether a mutation's outcome is unknown, and whether a same key resend
    /// is safe, is for the caller to set, since only it knows what it sent.
    #[must_use]
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            outcome_unknown: false,
            retryable_with_same_key: false,
            retry_after: None,
        }
    }

    /// Transient when a later call can succeed: the gateway said a fresh
    /// call may, or said resending with the same idempotency key is safe
    /// (an unknown outcome on a keyed mutation, which the direct transport
    /// reports as a transient transport failure). Permanent otherwise.
    #[must_use]
    pub fn permanence(&self) -> Permanence {
        if self.retryable || self.retryable_with_same_key {
            Permanence::Transient
        } else {
            Permanence::Permanent
        }
    }

    /// Backpressure carrying the gateway's wait, when it asked for one, so a
    /// caller holds off as long as it would on Alpaca's own `Retry-After`.
    /// `None` for a hop without a wait.
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        self.retry_after.map(|retry_after| Backpressure {
            retry_after: Some(retry_after),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backpressure {
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permanence {
    Permanent,
    Transient,
}

/// The one place that decides which HTTP statuses clear on their own for the
/// broker, market-data, wallet, and tokenization clients: 5xx is server-side,
/// 408 is a request timeout, and 429 is rate limiting, all of which can pass on
/// a later attempt. Every other 4xx is the account or request itself being
/// rejected. A 3xx is permanent too: the clients never follow redirects, so
/// the same request is redirected again. Shared by every such error type's
/// `permanence()` so the policy cannot drift between them.
#[cfg(feature = "broker")]
pub(crate) fn response_status_permanence(status: reqwest::StatusCode) -> Permanence {
    match status {
        reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            Permanence::Transient
        }
        status if status.is_client_error() || status.is_redirection() => Permanence::Permanent,
        _ => Permanence::Transient,
    }
}

#[cfg(feature = "issuer")]
const fn status_permanence(status_code: u16) -> Permanence {
    if status_code == 408 || status_code == 429 || status_code >= 500 {
        Permanence::Transient
    } else {
        Permanence::Permanent
    }
}

/// Alpaca-assigned identifier for a tokenization request. Server-generated
/// (a UUID in practice), opaque to consumers, and a plain string on the wire.
#[cfg(feature = "issuer")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenizationRequestId(pub String);

#[cfg(feature = "issuer")]
impl TokenizationRequestId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[cfg(feature = "issuer")]
impl std::fmt::Display for TokenizationRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Blockchain network a tokenized asset lives on.
///
/// A closed set of networks currently issued by st0x and accepted by Alpaca.
/// Modeling it as an enum (rather than an
/// opaque `String`) means an unsupported network is a deserialization error
/// instead of a value that silently flows through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Base,
    Ethereum,
    #[serde(rename = "hyperevm")]
    HyperEvm,
    Robinhood,
    #[serde(rename = "binance")]
    BnbSmartChain,
}

impl Network {
    /// The lowercase wire string for this network, matching the serde
    /// encoding.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Ethereum => "ethereum",
            Self::HyperEvm => "hyperevm",
            Self::Robinhood => "robinhood",
            Self::BnbSmartChain => "binance",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "issuer")]
    use httpmock::prelude::*;

    use super::*;

    #[cfg(feature = "broker")]
    #[test]
    fn status_permanence_classifies_retryable_and_permanent_statuses() {
        assert_eq!(
            response_status_permanence(reqwest::StatusCode::REQUEST_TIMEOUT),
            Permanence::Transient
        );
        assert_eq!(
            response_status_permanence(reqwest::StatusCode::TOO_MANY_REQUESTS),
            Permanence::Transient
        );
        assert_eq!(
            response_status_permanence(reqwest::StatusCode::BAD_GATEWAY),
            Permanence::Transient
        );
        assert_eq!(
            response_status_permanence(reqwest::StatusCode::FORBIDDEN),
            Permanence::Permanent
        );
        assert_eq!(
            response_status_permanence(reqwest::StatusCode::FOUND),
            Permanence::Permanent
        );
    }

    #[test]
    fn issued_networks_use_alpaca_wire_names() {
        for (network, wire) in [
            (Network::Base, "base"),
            (Network::Ethereum, "ethereum"),
            (Network::HyperEvm, "hyperevm"),
            (Network::Robinhood, "robinhood"),
            (Network::BnbSmartChain, "binance"),
        ] {
            assert_eq!(
                serde_json::to_string(&network).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<Network>(&format!("\"{wire}\"")).unwrap(),
                network
            );
        }
    }

    #[test]
    fn auth_variants_deserialize_from_flattened_consumer_config() {
        assert!(matches!(
            serde_json::from_value::<AlpacaAuth>(serde_json::json!({
                "api_key": "key",
                "api_secret": "secret"
            }))
            .unwrap(),
            AlpacaAuth::Basic { .. }
        ));
        assert!(matches!(
            serde_json::from_value::<AlpacaAuth>(serde_json::json!({
                "client_id": "client",
                "kms_key_version": "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1"
            }))
            .unwrap(),
            AlpacaAuth::KmsJwt { .. }
        ));
        assert!(matches!(
            serde_json::from_value::<AlpacaAuth>(serde_json::json!({
                "client_id": "client",
                "private_key_pem": "pem"
            }))
            .unwrap(),
            AlpacaAuth::PrivateKeyJwt { .. }
        ));
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn client_rejects_insecure_base_urls_before_building() {
        for rejected in [
            "http://broker-api.alpaca.markets",
            "https://user:pass@broker-api.alpaca.markets",
            "https://broker-api.alpaca.markets?token=secret",
        ] {
            assert!(
                matches!(
                    AlpacaClient::new(
                        rejected,
                        "account".into(),
                        "key".into(),
                        "secret".into(),
                        Duration::from_secs(2),
                        Duration::from_secs(2),
                    ),
                    Err(AlpacaError::InvalidUrl(_))
                ),
                "{rejected}"
            );
        }
    }

    #[cfg(feature = "issuer")]
    #[tokio::test]
    async fn credentialed_requests_stay_on_the_origin_and_do_not_follow_redirects() {
        let server = MockServer::start();
        let redirect = server.mock(|when, then| {
            when.method(GET).path("/redirect");
            then.status(302)
                .header("location", "http://example.invalid/collect");
        });
        let client = AlpacaClient::new(
            &server.base_url(),
            "account".into(),
            "key".into(),
            "secret".into(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();

        let url =
            resolve_segments(&client.base_url, &["http://example.invalid", "collect"]).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), "/http:%2F%2Fexample.invalid/collect");

        let response = client
            .get(&["redirect"])
            .await
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        redirect.assert();
    }
    #[cfg(feature = "issuer")]
    fn retry_client() -> AlpacaClient {
        AlpacaClient::new(
            "http://localhost",
            "account".into(),
            "key".into(),
            "secret".into(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap()
    }

    #[cfg(feature = "issuer")]
    #[tokio::test]
    async fn retry_after_delays_the_next_attempt() {
        let client = retry_client();
        let started = Instant::now();
        let mut attempts = 0;
        client
            .with_retry(|| {
                attempts += 1;
                std::future::ready(if attempts == 1 {
                    Err(AlpacaError::RateLimited {
                        body: String::new(),
                        retry_after: Some(Duration::from_secs(2)),
                    })
                } else {
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(attempts, 2);
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    #[cfg(feature = "issuer")]
    #[tokio::test]
    async fn long_retry_after_blocks_clones_without_an_attempt() {
        let client = retry_client();
        let started = Instant::now();
        let error = client
            .with_retry(|| async {
                Err::<(), _>(AlpacaError::RateLimited {
                    body: String::new(),
                    retry_after: Some(Duration::from_secs(300)),
                })
            })
            .await
            .unwrap_err();
        assert!(error.is_retryable());
        let mut attempts = 0;
        let error = client
            .clone()
            .with_retry(|| {
                attempts += 1;
                std::future::ready(Ok(()))
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AlpacaError::RateLimited { .. }));
        assert_eq!(attempts, 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[cfg(feature = "issuer")]
    #[tokio::test]
    async fn concurrent_deadline_extension_is_rechecked_before_attempting() {
        let client = retry_client();
        client.hold_for(Duration::from_millis(100));
        let other = client.clone();
        let extend = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            other.hold_for(Duration::from_millis(250));
        });
        let started = Instant::now();
        client
            .with_retry(|| std::future::ready(Ok(())))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        extend.await.unwrap();
    }

    /// A hold another caller takes while this call backs off refuses the
    /// next attempt locally; the status stays the one Alpaca last answered.
    #[cfg(feature = "issuer")]
    #[tokio::test(start_paused = true)]
    async fn a_call_answered_500_then_held_locally_reports_500() {
        let client = retry_client();
        let mut attempts = 0;
        let failure = client
            .with_retry_reporting(|| {
                attempts += 1;
                let other = client.clone();
                async move {
                    request_id::record(
                        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        &reqwest::header::HeaderMap::new(),
                    );
                    tokio::spawn(async move { other.hold_for(Duration::from_secs(300)) });
                    Err::<(), _>(AlpacaError::Api {
                        status_code: 500,
                        body: String::new(),
                    })
                }
            })
            .await
            .unwrap_err();

        assert_eq!(attempts, 1);
        assert!(
            matches!(failure.error, AlpacaError::RateLimited { .. }),
            "{:?}",
            failure.error
        );
        assert!(failure.written);
        assert_eq!(failure.alpaca_status, Some(500));
    }

    #[cfg(feature = "issuer")]
    #[tokio::test]
    async fn a_call_held_locally_before_any_attempt_reports_no_status() {
        let client = retry_client();
        client.hold_for(Duration::from_secs(300));
        let mut attempts = 0;

        let failure = client
            .with_retry_reporting(|| {
                attempts += 1;
                std::future::ready(Ok(()))
            })
            .await
            .unwrap_err();

        assert_eq!(attempts, 0);
        assert!(
            matches!(failure.error, AlpacaError::RateLimited { .. }),
            "{:?}",
            failure.error
        );
        assert!(!failure.written);
        assert_eq!(failure.alpaca_status, None);
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn retry_after_hold_is_capped() {
        let client = retry_client();
        client.hold_for(Duration::from_hours(24));
        let wait = client.backpressure_wait().unwrap();
        assert!(wait <= MAX_RETRY_AFTER_HOLD);
        assert!(wait + Duration::from_secs(1) > MAX_RETRY_AFTER_HOLD);
    }
}
