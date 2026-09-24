//! Market-session classification and validated quote types returned by the
//! Broker API calendar/clock and the Market Data API.

use chrono::{DateTime, Utc};
use rain_math_float::FloatError;
use serde::{Deserialize, Serialize};

use st0x_finance::{Positive, Usd};

/// Describes the current trading session, driving order-type selection.
///
/// - `Regular` -- standard market hours; market orders are used.
/// - `Extended` -- pre-market or after-hours; only limit orders with
///   `extended_hours: true` are allowed by the broker.
/// - `Overnight` -- 20:00-04:00 ET on the Blue Ocean ATS; only limit orders
///   with `day` time-in-force and `extended_hours: true` are allowed, priced
///   from the indicative overnight feed.
/// - `Closed` -- outside all trading sessions: weekends, holidays until 20:00
///   ET that evening (including the overnight window immediately preceding
///   the holiday), and the gap between an early close's session end and
///   20:00 ET.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketSession {
    Regular,
    Extended,
    Overnight,
    Closed,
}

/// Classifies the closure after the current extended session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCloseGap {
    /// The next trading session begins on the following calendar day.
    OrdinaryOvernight,
    /// At least one full calendar day separates this close from the next
    /// trading session, as on weekends and exchange holidays.
    MultiDayClosure,
    /// The executor could not identify the next trading session.
    Unknown,
    /// The executor does not provide post-close gap classification.
    Unavailable,
}

/// Current market-session classification, with close metadata available only
/// for an extended session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketSessionStatus {
    pub session: MarketSession,
    /// Earliest eligible broker session start for this calendar interval.
    pub session_opens_at: Option<DateTime<Utc>>,
    pub regular_session_closes_at: Option<DateTime<Utc>>,
    pub extended_session_closes_at: Option<DateTime<Utc>>,
    pub post_close_gap: PostCloseGap,
}

impl MarketSessionStatus {
    #[must_use]
    pub const fn without_close_metadata(session: MarketSession) -> Self {
        Self {
            session,
            session_opens_at: None,
            regular_session_closes_at: None,
            extended_session_closes_at: None,
            post_close_gap: PostCloseGap::Unavailable,
        }
    }

    #[must_use]
    pub const fn session(self) -> MarketSession {
        self.session
    }
}

/// Latest national best bid and offer for a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestQuote {
    bid: Positive<Usd>,
    ask: Positive<Usd>,
}

impl LatestQuote {
    /// Builds a validated quote whose bid does not exceed its ask.
    ///
    /// # Errors
    ///
    /// Returns [`LatestQuoteError::Crossed`] when the bid exceeds the ask, or
    /// the comparison error.
    pub fn new(bid: Positive<Usd>, ask: Positive<Usd>) -> Result<Self, LatestQuoteError> {
        if ask.inner().lt(&bid.inner())? {
            return Err(LatestQuoteError::Crossed { bid, ask });
        }

        Ok(Self { bid, ask })
    }

    #[must_use]
    pub const fn bid(self) -> Positive<Usd> {
        self.bid
    }

    #[must_use]
    pub const fn ask(self) -> Positive<Usd> {
        self.ask
    }
}

/// An indicative overnight quote with the broker timestamp it was generated
/// at, so consumers can judge its age before pricing from it.
///
/// The overnight feed is indicative (derived from Blue Ocean data), not a
/// firm tape quote: fills can deviate from it, and a stale indicative quote
/// must never be priced from silently. That is why the timestamp is required
/// here while the regular [`LatestQuote`] path ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndicativeQuote {
    pub quote: LatestQuote,
    pub at: DateTime<Utc>,
}

/// Error returned when constructing a latest quote.
#[derive(Debug, thiserror::Error)]
pub enum LatestQuoteError {
    #[error("quote comparison failed: {0}")]
    Float(#[from] FloatError),
    #[error("crossed quote: bid {bid} exceeds ask {ask}")]
    Crossed {
        bid: Positive<Usd>,
        ask: Positive<Usd>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_status_without_close_metadata_preserves_each_session_variant() {
        for session in [
            MarketSession::Regular,
            MarketSession::Extended,
            MarketSession::Overnight,
            MarketSession::Closed,
        ] {
            let status = MarketSessionStatus::without_close_metadata(session);
            assert_eq!(status.session(), session);
            assert!(status.session_opens_at.is_none());
            assert!(status.regular_session_closes_at.is_none());
            assert!(status.extended_session_closes_at.is_none());
            assert_eq!(status.post_close_gap, PostCloseGap::Unavailable);
        }
    }

    #[test]
    fn extended_status_keeps_close_metadata_on_the_extended_variant() {
        let closes_at = Utc::now();
        let status = MarketSessionStatus {
            session: MarketSession::Extended,
            session_opens_at: None,
            regular_session_closes_at: None,
            extended_session_closes_at: Some(closes_at),
            post_close_gap: PostCloseGap::Unknown,
        };

        assert_eq!(status.session(), MarketSession::Extended);
        assert_eq!(status.extended_session_closes_at, Some(closes_at));
        assert_eq!(status.post_close_gap, PostCloseGap::Unknown);
        assert_ne!(
            status,
            MarketSessionStatus::without_close_metadata(MarketSession::Extended),
            "a metadata-capable executor with an unknown gap must remain distinct from an executor that cannot report the gap"
        );
    }
}
