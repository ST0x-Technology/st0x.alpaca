//! Wire identities and decoded mutations of the Alpaca corporate-action
//! stream.
//!
//! Every identity here is untrusted stream input, so each constructor
//! validates it and deserialization goes through the same validation.

use std::fmt;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Monotonic ULID identifying one Alpaca corporate-action stream event.
///
/// The SSE `id` field, the payload `event_id`, and the `since_id` replay
/// cursor all carry this value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CorporateActionEventId(String);

impl CorporateActionEventId {
    /// Accepts only a canonical (uppercase Crockford base32, 26 characters,
    /// first character at most `7`) ULID.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let valid = value.len() == 26
            && value.bytes().enumerate().all(|(index, byte)| {
                let allowed = matches!(
                    byte,
                    b'0'..=b'9'
                        | b'A'..=b'H'
                        | b'J' | b'K' | b'M' | b'N'
                        | b'P'..=b'T'
                        | b'V'..=b'Z'
                );
                allowed && (index != 0 || byte <= b'7')
            });
        valid.then_some(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CorporateActionEventId {
    fn deserialize<Deser>(deserializer: Deser) -> Result<Self, Deser::Error>
    where
        Deser: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value)
            .ok_or_else(|| serde::de::Error::custom("invalid corporate-action event id"))
    }
}

impl fmt::Display for CorporateActionEventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Stable Alpaca identity of one corporate action across insert, update, and
/// delete mutations.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CorporateActionId(String);

impl CorporateActionId {
    const MAX_LEN: usize = 128;

    /// Accepts a non-empty id of at most 128 bytes.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && value.len() <= Self::MAX_LEN).then_some(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CorporateActionId {
    fn deserialize<Deser>(deserializer: Deser) -> Result<Self, Deser::Error>
    where
        Deser: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("invalid corporate-action id"))
    }
}

impl fmt::Display for CorporateActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The `ca.symbol` of a corporate action: the underlying equity symbol.
///
/// Surrounding whitespace is stripped, and an empty or whitespace-only
/// symbol is rejected (the same rule as st0x.issuance's `UnderlyingSymbol`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CorporateActionSymbol(String);

impl CorporateActionSymbol {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CorporateActionSymbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The documented corporate-action stream mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorporateActionMutationKind {
    Insert,
    Update,
    Delete,
}

impl CorporateActionMutationKind {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    /// The wire spelling (`insert`, `update`, `delete`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// The dividend corporate action one mutation carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividendCorporateAction {
    pub id: CorporateActionId,
    pub underlying: CorporateActionSymbol,
    pub ex_date: NaiveDate,
}

/// One decoded, identity-checked corporate-action stream mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorporateActionMutation {
    pub event_id: CorporateActionEventId,
    pub kind: CorporateActionMutationKind,
    pub action: DividendCorporateAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corporate_action_identity_bounds_untrusted_stream_input() {
        assert!(CorporateActionId::new("ca-1").is_some());
        assert!(CorporateActionId::new("").is_none());
        assert!(CorporateActionId::new("x".repeat(129)).is_none());
    }

    #[test]
    fn corporate_action_event_identity_requires_a_canonical_ulid() {
        assert!(CorporateActionEventId::new("01J9RPMV5TKB8WX3M4F1KZ7QH2").is_some());
        assert!(CorporateActionEventId::new("81J9RPMV5TKB8WX3M4F1KZ7QH2").is_none());
        assert!(CorporateActionEventId::new("01j9rpmv5tkb8wx3m4f1kz7qh2").is_none());
        assert!(CorporateActionEventId::new("01L9RPMV5TKB8WX3M4F1KZ7QH2").is_none());
    }

    #[test]
    fn corporate_action_identity_deserialization_uses_validation() {
        let id: CorporateActionId = serde_json::from_str(r#""ca-1""#).unwrap();
        assert_eq!(id.as_str(), "ca-1");

        assert!(serde_json::from_str::<CorporateActionId>(r#""""#).is_err());
        assert!(
            serde_json::from_str::<CorporateActionId>(&format!("\"{}\"", "x".repeat(129))).is_err()
        );
    }

    #[test]
    fn corporate_action_event_identity_deserialization_uses_validation() {
        let event_id: CorporateActionEventId =
            serde_json::from_str(r#""01J9RPMV5TKB8WX3M4F1KZ7QH2""#).unwrap();
        assert_eq!(event_id.as_str(), "01J9RPMV5TKB8WX3M4F1KZ7QH2");

        assert!(
            serde_json::from_str::<CorporateActionEventId>(r#""81J9RPMV5TKB8WX3M4F1KZ7QH2""#,)
                .is_err()
        );
        assert!(
            serde_json::from_str::<CorporateActionEventId>(r#""01j9rpmv5tkb8wx3m4f1kz7qh2""#,)
                .is_err()
        );
    }

    #[test]
    fn corporate_action_symbol_trims_and_rejects_blank_input() {
        assert_eq!(
            CorporateActionSymbol::new(" AAPL ").unwrap().as_str(),
            "AAPL"
        );
        assert_eq!(CorporateActionSymbol::new(""), None);
        assert_eq!(CorporateActionSymbol::new("   "), None);
    }
}
