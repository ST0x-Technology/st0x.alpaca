//! Shared Alpaca transport: HTTP client construction, the dual
//! authentication scheme (HTTP Basic plus the legacy `APCA-API-KEY-ID` /
//! `APCA-API-SECRET-KEY` headers), the retry policy, the error taxonomy,
//! and wire types shared across surface modules.

use backon::{ExponentialBuilder, Retryable};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Alpaca API credentials applied to every request.
#[derive(Clone)]
pub struct AlpacaAuth {
    pub api_key: String,
    pub api_secret: String,
}

impl std::fmt::Debug for AlpacaAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlpacaAuth")
            .field("api_key", &"<redacted>")
            .field("api_secret", &"<redacted>")
            .finish()
    }
}

/// HTTP client for Alpaca's APIs.
///
/// Carries the base URL, account id, credentials, and retry policy shared
/// by every surface module. Surface modules build endpoint URLs from
/// [`Self::base_url`] and [`Self::account_id`], send requests through the
/// authenticated [`Self::get`] / [`Self::post`] builders, and wrap
/// idempotent calls in [`Self::with_retry`].
#[derive(Debug, Clone)]
pub struct AlpacaClient {
    http: reqwest::Client,
    base_url: String,
    account_id: String,
    auth: AlpacaAuth,
    max_retries: usize,
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
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            http,
            base_url,
            account_id,
            auth: AlpacaAuth {
                api_key,
                api_secret,
            },
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
    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.authenticate(self.http.get(url))
    }

    /// Authenticated POST request builder for the given URL.
    pub fn post(&self, url: &str) -> reqwest::RequestBuilder {
        self.authenticate(self.http.post(url))
    }

    /// Authenticated DELETE request builder for the given URL.
    pub fn delete(&self, url: &str) -> reqwest::RequestBuilder {
        self.authenticate(self.http.delete(url))
    }

    /// Authenticated PATCH request builder for the given URL.
    pub fn patch(&self, url: &str) -> reqwest::RequestBuilder {
        self.authenticate(self.http.patch(url))
    }

    /// Applies Alpaca's dual authentication to a request: HTTP Basic auth
    /// plus the legacy `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` headers.
    /// Alpaca's tokenization endpoints require both.
    pub fn authenticate(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .basic_auth(&self.auth.api_key, Some(&self.auth.api_secret))
            .header("APCA-API-KEY-ID", &self.auth.api_key)
            .header("APCA-API-SECRET-KEY", &self.auth.api_secret)
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
            Self::Parse { .. }
            | Self::Auth(_)
            | Self::RequestNotFound { .. }
            | Self::ResponseIdMismatch { .. } => false,
        }
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
/// A closed set -- only `base` is supported today. Serialized as the
/// lowercase wire string (`"base"`). Modeling it as an enum (rather than an
/// opaque `String`) means an unsupported network is a deserialization error
/// instead of a value that silently flows through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    Base,
}

impl Network {
    /// The lowercase wire string for this network, matching the serde
    /// encoding.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Base => "base",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
