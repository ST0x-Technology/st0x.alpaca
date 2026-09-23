//! Replay positions for opening the corporate-action stream: the durable
//! `since_id` cursor, the operator-approved `since` bootstrap instant, and
//! the inclusive `until` bound of a finite history window.

use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, Utc};

use super::event::CorporateActionEventId;

/// An operator-approved lower bound for an authenticated corporate-action feed
/// that has no durable cursor.
///
/// Parsing accepts non-future RFC3339 timestamps and normalizes them to UTC for
/// the Alpaca `since` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorporateActionBootstrapSince(DateTime<Utc>);

impl CorporateActionBootstrapSince {
    /// # Errors
    ///
    /// Returns [`CorporateActionBootstrapSinceError::Future`] when `instant`
    /// is later than the current time.
    pub fn try_from_instant(
        instant: DateTime<Utc>,
    ) -> Result<Self, CorporateActionBootstrapSinceError> {
        if instant > Utc::now() {
            return Err(CorporateActionBootstrapSinceError::Future(instant));
        }
        Ok(Self(instant))
    }

    /// The `since` query value (RFC3339, UTC, `Z` suffix).
    #[must_use]
    pub fn query_value(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

/// An error returned when validating a corporate-action bootstrap boundary.
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionBootstrapSinceError {
    /// The configured value is not a valid RFC3339 timestamp.
    #[error("invalid corporate-action bootstrap timestamp")]
    Parse(#[from] chrono::ParseError),
    /// The configured timestamp is later than the current time.
    #[error("corporate-action bootstrap timestamp {0} is in the future")]
    Future(DateTime<Utc>),
}

impl FromStr for CorporateActionBootstrapSince {
    type Err = CorporateActionBootstrapSinceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let instant = DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc);
        Self::try_from_instant(instant)
    }
}

/// The inclusive upper bound of a finite history replay. Alpaca closes a
/// `since` + `until` stream after this bound, so a clean EOF proves the
/// bounded replay completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorporateActionReplayUntil(DateTime<Utc>);

impl CorporateActionReplayUntil {
    #[must_use]
    pub const fn at(instant: DateTime<Utc>) -> Self {
        Self(instant)
    }

    /// The `until` query value (RFC3339, UTC, `Z` suffix).
    #[must_use]
    pub fn query_value(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

/// Where a stream connection starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorporateActionReplay {
    /// No replay query: the stream starts at live events.
    Live,
    /// `since_id=<cursor>`: resume after (and re-deliver) the durable cursor.
    SinceId(CorporateActionEventId),
    /// `since=<instant>`: open-ended replay from a bootstrap instant.
    Since(CorporateActionBootstrapSince),
    /// `since=<instant>&until=<instant>`: a finite history window.
    Window {
        since: CorporateActionBootstrapSince,
        until: CorporateActionReplayUntil,
    },
}

impl CorporateActionReplay {
    /// The replay query pairs, in the order Alpaca receives them.
    pub(super) fn query_pairs(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Live => Vec::new(),
            Self::SinceId(cursor) => vec![("since_id", cursor.as_str().to_string())],
            Self::Since(since) => vec![("since", since.query_value())],
            Self::Window { since, until } => vec![
                ("since", since.query_value()),
                ("until", until.query_value()),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_normalizes_an_explicit_corporate_action_bootstrap_instant() {
        let since = "2026-08-30T21:00:00-03:00"
            .parse::<CorporateActionBootstrapSince>()
            .unwrap();

        assert_eq!(since.query_value(), "2026-08-31T00:00:00Z");
    }

    #[test]
    fn rejects_a_malformed_corporate_action_bootstrap_instant() {
        assert!(matches!(
            "yesterday".parse::<CorporateActionBootstrapSince>(),
            Err(CorporateActionBootstrapSinceError::Parse(_))
        ));
    }

    #[test]
    fn rejects_a_future_corporate_action_bootstrap_instant() {
        assert!(matches!(
            "2999-01-01T00:00:00Z".parse::<CorporateActionBootstrapSince>(),
            Err(CorporateActionBootstrapSinceError::Future(_))
        ));
    }

    #[test]
    fn fallible_bootstrap_constructor_rejects_a_future_instant() {
        let future = Utc::now() + chrono::Duration::days(1);

        assert!(matches!(
            CorporateActionBootstrapSince::try_from_instant(future),
            Err(CorporateActionBootstrapSinceError::Future(rejected))
                if rejected == future
        ));
    }

    #[test]
    fn replay_positions_map_to_their_alpaca_query_parameters() {
        let since = "2026-08-31T00:00:00Z"
            .parse::<CorporateActionBootstrapSince>()
            .unwrap();
        let until = CorporateActionReplayUntil::at(
            DateTime::parse_from_rfc3339("2026-09-01T23:59:59Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let cursor = CorporateActionEventId::new("01J9RPMV5TKB8WX3M4F1KZ7QH2").unwrap();

        assert_eq!(CorporateActionReplay::Live.query_pairs(), vec![]);
        assert_eq!(
            CorporateActionReplay::SinceId(cursor).query_pairs(),
            vec![("since_id", "01J9RPMV5TKB8WX3M4F1KZ7QH2".to_string())]
        );
        assert_eq!(
            CorporateActionReplay::Since(since.clone()).query_pairs(),
            vec![("since", "2026-08-31T00:00:00Z".to_string())]
        );
        assert_eq!(
            CorporateActionReplay::Window { since, until }.query_pairs(),
            vec![
                ("since", "2026-08-31T00:00:00Z".to_string()),
                ("until", "2026-09-01T23:59:59Z".to_string()),
            ]
        );
    }
}
