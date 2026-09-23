//! Alpaca tokenization API surface used by st0x.liquidity: mint requests,
//! request history lookups, redemption detection, and polling until a
//! request reaches a terminal state.
//!
//! [`AlpacaTokenizationService`] is the entry point. Onchain actions
//! (the redemption transfer, node-sync waits, mint receipt verification)
//! are the consumer's concern; this module only speaks HTTP to Alpaca.

mod client;

use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::str::FromStr;
use uuid::Uuid;

pub use client::{
    AlpacaApiErrorMessage, AlpacaTokenizationError, AlpacaTokenizationService, TokenizationRequest,
    TokenizationRequestStatus, TokenizationRequestType,
};

/// Our internal tracking id for a tokenized equity mint, chosen at enqueue time.
///
/// A UUID so invalid ids are unrepresentable and apalis/CLI retries always
/// target the same aggregate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct IssuerRequestId(pub Uuid);

impl IssuerRequestId {
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Display for IssuerRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for IssuerRequestId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(value)?))
    }
}

/// Client-supplied correlation label returned by the tokenization provider.
///
/// This wire type is intentionally wider than our UUID-backed
/// [`IssuerRequestId`], because provider history can contain labels created by
/// other clients.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ClientRequestId(String);

/// Error parsing a [`ClientRequestId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientRequestIdError {
    #[error("client request id must be non-empty")]
    Empty,
    #[error("client request id exceeds 128 characters")]
    TooLong,
    #[error("client request id must contain only printable ASCII characters")]
    NonPrintableAscii,
    #[error("client request id must not have leading or trailing whitespace")]
    SurroundingWhitespace,
}

impl ClientRequestId {
    /// Parses a provider correlation label.
    ///
    /// # Errors
    ///
    /// Returns [`ClientRequestIdError`] when the value is empty, longer than
    /// 128 bytes, contains non-printable ASCII, or has surrounding whitespace.
    pub fn try_new(value: impl AsRef<str>) -> Result<Self, ClientRequestIdError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(ClientRequestIdError::Empty);
        }
        if value.len() > 128 {
            return Err(ClientRequestIdError::TooLong);
        }
        if !value.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
            return Err(ClientRequestIdError::NonPrintableAscii);
        }
        if value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace) {
            return Err(ClientRequestIdError::SurroundingWhitespace);
        }

        Ok(Self(value.to_owned()))
    }
}

impl From<&IssuerRequestId> for ClientRequestId {
    fn from(value: &IssuerRequestId) -> Self {
        Self(value.to_string())
    }
}

impl AsRef<str> for ClientRequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Display for ClientRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for ClientRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

/// Deterministic issuer request id for tests. Maps a human-readable label to a
/// UUID v5 so test aggregate ids stay valid [`IssuerRequestId`] values.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn issuer_request_id(label: &str) -> IssuerRequestId {
    IssuerRequestId(Uuid::new_v5(&Uuid::NAMESPACE_OID, label.as_bytes()))
}

/// Alpaca tokenization request identifier used to track the mint operation through their API.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TokenizationRequestId(String);

/// Error parsing a [`TokenizationRequestId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenizationRequestIdError {
    #[error("tokenization request id must be non-empty")]
    Empty,
}

impl TokenizationRequestId {
    /// Parses a provider-issued tokenization request id.
    ///
    /// # Errors
    ///
    /// Returns [`TokenizationRequestIdError::Empty`] for an empty value.
    pub fn try_new(value: impl AsRef<str>) -> Result<Self, TokenizationRequestIdError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(TokenizationRequestIdError::Empty);
        }
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for TokenizationRequestId {
    type Error = TokenizationRequestIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl FromStr for TokenizationRequestId {
    type Err = TokenizationRequestIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_new(value)
    }
}

impl std::fmt::Display for TokenizationRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl AsRef<str> for TokenizationRequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TokenizationRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

/// Deterministic tokenization request id for tests.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn tokenization_request_id(label: &str) -> TokenizationRequestId {
    TokenizationRequestId::try_new(label)
        .unwrap_or_else(|_| unreachable!("test tokenization request id must be non-empty"))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::from_str;

    use super::{
        ClientRequestId, ClientRequestIdError, TokenizationRequestId, TokenizationRequestIdError,
    };

    proptest! {
        #[test]
        fn printable_client_request_ids_respect_whitespace_contract(value in "[ -~]{1,128}") {
            let result = ClientRequestId::try_new(&value);
            if value.starts_with(' ') || value.ends_with(' ') {
                prop_assert_eq!(result.unwrap_err(), ClientRequestIdError::SurroundingWhitespace);
            } else {
                let id = result.unwrap();
                prop_assert_eq!(id.as_ref(), value.as_str());
            }
        }

        #[test]
        fn overlong_printable_client_request_ids_are_rejected(value in "[ -~]{129,512}") {
            prop_assert_eq!(ClientRequestId::try_new(value).unwrap_err(), ClientRequestIdError::TooLong);
        }
    }

    #[test]
    fn client_request_id_accepts_documented_provider_domain() {
        let label = ClientRequestId::try_new("my mint-ref-001").unwrap();
        assert_eq!(label.as_ref(), "my mint-ref-001");

        let maximum_length = "x".repeat(128);
        assert_eq!(
            ClientRequestId::try_new(&maximum_length).unwrap().as_ref(),
            maximum_length
        );
    }

    #[test]
    fn client_request_id_rejects_values_outside_provider_domain() {
        assert_eq!(
            ClientRequestId::try_new("").unwrap_err(),
            ClientRequestIdError::Empty
        );
        assert_eq!(
            ClientRequestId::try_new("x".repeat(129)).unwrap_err(),
            ClientRequestIdError::TooLong
        );
        assert_eq!(
            ClientRequestId::try_new("contains\nnewline").unwrap_err(),
            ClientRequestIdError::NonPrintableAscii
        );
        assert_eq!(
            ClientRequestId::try_new(" leading-space").unwrap_err(),
            ClientRequestIdError::SurroundingWhitespace
        );
        assert_eq!(
            ClientRequestId::try_new("trailing-space ").unwrap_err(),
            ClientRequestIdError::SurroundingWhitespace
        );
    }

    #[test]
    fn client_request_id_deserialize_enforces_provider_domain() {
        let id: ClientRequestId = from_str("\"my mint-ref-001\"").unwrap();
        assert_eq!(id.as_ref(), "my mint-ref-001");

        for (invalid, expected_message) in [
            ("\"\"".to_string(), "client request id must be non-empty"),
            (
                format!("\"{}\"", "x".repeat(129)),
                "client request id exceeds 128 characters",
            ),
            (
                "\"contains\\nnewline\"".to_string(),
                "client request id must contain only printable ASCII characters",
            ),
            (
                "\" leading-space\"".to_string(),
                "client request id must not have leading or trailing whitespace",
            ),
            (
                "\"trailing-space \"".to_string(),
                "client request id must not have leading or trailing whitespace",
            ),
        ] {
            let error = from_str::<ClientRequestId>(&invalid)
                .expect_err("invalid client request id must fail deserialization");
            assert!(
                error.to_string().contains(expected_message),
                "unexpected error for {invalid}: {error}"
            );
        }
    }

    #[test]
    fn tokenization_request_id_rejects_empty_string() {
        let error = TokenizationRequestId::try_new("").unwrap_err();
        assert_eq!(error, TokenizationRequestIdError::Empty);
    }

    #[test]
    fn tokenization_request_id_from_str_parses_non_empty_value() {
        let request_id = "tok-req-123".parse::<TokenizationRequestId>().unwrap();
        assert_eq!(request_id.as_ref(), "tok-req-123");
    }

    #[test]
    fn tokenization_request_id_deserialize_rejects_empty_string() {
        let error = serde_json::from_str::<TokenizationRequestId>("\"\"")
            .expect_err("empty tokenization request id must fail deserialization");
        assert!(
            error
                .to_string()
                .contains("tokenization request id must be non-empty")
        );
    }

    #[test]
    fn tokenization_request_id_deserialize_accepts_non_empty_value() {
        let request_id: TokenizationRequestId = serde_json::from_str("\"tok-req-456\"").unwrap();
        assert_eq!(request_id.as_ref(), "tok-req-456");
    }
}
