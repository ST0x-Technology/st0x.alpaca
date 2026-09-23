//! Shared Alpaca transport: HTTP client construction, the dual
//! authentication scheme (HTTP Basic plus the legacy `APCA-API-KEY-ID` /
//! `APCA-API-SECRET-KEY` headers), the retry policy, the error taxonomy,
//! and wire types shared across surface modules.

#[cfg(feature = "issuer")]
use backon::{ExponentialBuilder, Retryable};
use serde::{Deserialize, Serialize};
use std::time::Duration;
#[cfg(feature = "issuer")]
use url::Url;

#[cfg(feature = "issuer")]
use crate::auth::AuthRuntime;
#[cfg(feature = "issuer")]
use crate::auth::KmsJwtError;
#[cfg(feature = "issuer")]
use crate::endpoint::{EndpointError, EndpointRole, resolve_path, validate_origin};

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

/// HTTP client for Alpaca's issuer APIs.
///
/// Carries the configured base URL, account id, credentials, and retry
/// policy. Surface modules send requests through the crate-private
/// authenticated builders, which resolve an absolute request path against
/// the configured origin and refuse anything else, and wrap idempotent calls
/// in [`Self::with_retry`].
#[cfg(feature = "issuer")]
#[derive(Clone)]
pub struct AlpacaClient {
    http: reqwest::Client,
    base_url: Url,
    account_id: String,
    auth: AuthRuntime,
    max_retries: usize,
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

    /// Authenticated GET for an absolute `path` on the configured origin.
    pub(crate) async fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        let url = resolve_path(&self.base_url, path)?;
        self.authenticate(self.http.get(url)).await
    }

    /// Authenticated POST for an absolute `path` on the configured origin.
    pub(crate) async fn post(&self, path: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        let url = resolve_path(&self.base_url, path)?;
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
    /// # Errors
    ///
    /// Returns the final [`AlpacaError`] produced by `operation` once the
    /// error is non-retryable or the retry budget is exhausted.
    pub async fn with_retry<Value, Fut, Operation>(
        &self,
        operation: Operation,
    ) -> Result<Value, AlpacaError>
    where
        Operation: FnMut() -> Fut,
        Fut: Future<Output = Result<Value, AlpacaError>>,
    {
        operation
            .retry(
                ExponentialBuilder::default()
                    .with_max_times(self.max_retries)
                    .with_jitter(),
            )
            .when(AlpacaError::is_retryable)
            .await
    }
}

/// Errors that can occur during Alpaca API operations.
#[cfg(feature = "issuer")]
#[derive(Debug, thiserror::Error)]
pub enum AlpacaError {
    #[error(transparent)]
    InvalidUrl(#[from] EndpointError),
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
    /// Local preflight: the network is not a published Alpaca ITN
    /// `TokenizationNetwork` value, so the request is refused before any
    /// HTTP call.
    #[error(
        "Network {network} is not a published Alpaca TokenizationNetwork value -- see {reference}"
    )]
    UnsupportedTokenizationNetwork {
        network: Network,
        reference: &'static str,
    },
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
            | Self::UnsupportedTokenizationNetwork { .. }
            | Self::Parse { .. }
            | Self::Auth(_)
            | Self::RequestNotFound { .. }
            | Self::ResponseIdMismatch { .. } => false,
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
            | Self::UnsupportedTokenizationNetwork { .. }
            | Self::Parse { .. }
            | Self::Auth(_)
            | Self::RequestNotFound { .. }
            | Self::ResponseIdMismatch { .. } => Permanence::Permanent,
        }
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
/// rejected. Shared by every such error type's `permanence()` so the policy
/// cannot drift between them.
#[cfg(feature = "broker")]
pub(crate) fn response_status_permanence(status: reqwest::StatusCode) -> Permanence {
    match status {
        reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            Permanence::Transient
        }
        status if status.is_client_error() => Permanence::Permanent,
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

#[cfg(feature = "issuer")]
pub use st0x_finance::Symbol;

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
    async fn credentialed_requests_reject_insecure_targets_and_do_not_follow_redirects() {
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

        for foreign in [
            "http://example.invalid/collect",
            "https://example.invalid/collect",
            "//example.invalid/collect",
        ] {
            assert!(
                matches!(
                    client.get(foreign).await,
                    Err(AlpacaError::InvalidUrl(EndpointError::ForeignPath { .. }))
                ),
                "{foreign}"
            );
            assert!(
                matches!(
                    client.post(foreign).await,
                    Err(AlpacaError::InvalidUrl(EndpointError::ForeignPath { .. }))
                ),
                "{foreign}"
            );
        }
        let response = client.get("/redirect").await.unwrap().send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        redirect.assert();
    }
}
