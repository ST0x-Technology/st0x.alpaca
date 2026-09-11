//! Shared Alpaca transport: HTTP client construction, the dual
//! authentication scheme (HTTP Basic plus the legacy `APCA-API-KEY-ID` /
//! `APCA-API-SECRET-KEY` headers), the retry policy, the error taxonomy,
//! and wire types shared across surface modules.

use backon::{ExponentialBuilder, Retryable};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::auth::{AuthRuntime, KmsJwtError};

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
                .field("api_key", &"<redacted>")
                .field("api_secret", &"<redacted>")
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
                .field("private_key_pem", &"<redacted>")
                .finish(),
        }
    }
}

/// HTTP client for Alpaca's APIs.
///
/// Carries the base URL, account id, credentials, and retry policy shared
/// by every surface module. Surface modules build endpoint URLs from
/// [`Self::base_url`] and [`Self::account_id`], send requests through the
/// authenticated [`Self::get`] / [`Self::post`] builders, and wrap
/// idempotent calls in [`Self::with_retry`].
#[derive(Clone)]
pub struct AlpacaClient {
    http: reqwest::Client,
    base_url: String,
    account_id: String,
    auth: AuthRuntime,
    max_retries: usize,
}

impl std::fmt::Debug for AlpacaClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlpacaClient")
            .field("base_url", &self.base_url)
            .field("account_id", &self.account_id)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl AlpacaClient {
    /// Builds a client with the given connection and request timeouts.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError::Reqwest`] if the underlying HTTP client
    /// cannot be constructed.
    pub fn new(
        base_url: String,
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

    /// Builds a client supporting Basic, KMS JWT, or local private-key JWT authentication.
    ///
    /// `token_url` is ignored for Basic auth and must name the environment's
    /// Alpaca authx token endpoint for either JWT variant.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client or authentication runtime cannot be built.
    pub fn with_auth(
        base_url: String,
        account_id: String,
        auth: AlpacaAuth,
        token_url: &str,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, AlpacaError> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
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

    /// Base URL with any trailing slash removed.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Alpaca account id used in endpoint paths.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Authenticated GET request builder for the given URL.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn get(&self, url: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.authenticate(self.http.get(url)).await
    }

    /// Authenticated POST request builder for the given URL.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn post(&self, url: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.authenticate(self.http.post(url)).await
    }

    /// Authenticated DELETE request builder for the given URL.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn delete(&self, url: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.authenticate(self.http.delete(url)).await
    }

    /// Authenticated PATCH request builder for the given URL.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn patch(&self, url: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.authenticate(self.http.patch(url)).await
    }

    /// Applies Alpaca's dual authentication to a request: HTTP Basic auth
    /// plus the legacy `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` headers.
    /// Alpaca's tokenization endpoints require both.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn authenticate(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.auth.apply_wallet(builder).await.map_err(Into::into)
    }

    /// Applies the Market Data API's APCA/bearer authentication.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] when authentication cannot be prepared.
    pub async fn market_data_get(&self, url: &str) -> Result<reqwest::RequestBuilder, AlpacaError> {
        self.auth
            .apply_apca(self.http.get(url))
            .await
            .map_err(Into::into)
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
#[derive(Debug, thiserror::Error)]
pub enum AlpacaError {
    #[error("Reqwest error")]
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
}

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
            Self::Parse { .. }
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
            Self::Parse { .. }
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

const fn status_permanence(status_code: u16) -> Permanence {
    if status_code == 408 || status_code == 429 || status_code >= 500 {
        Permanence::Transient
    } else {
        Permanence::Permanent
    }
}

#[cfg(any(
    feature = "broker",
    feature = "issuer",
    feature = "market-data",
    feature = "wallet"
))]
pub use st0x_finance::Symbol;

/// Alpaca-assigned identifier for a tokenization request. Server-generated
/// (a UUID in practice), opaque to consumers, and a plain string on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenizationRequestId(pub String);

impl TokenizationRequestId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

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
    use super::*;

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
}
