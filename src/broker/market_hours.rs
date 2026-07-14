//! Market-session classification from Alpaca's trading calendar.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::America::New_York;
use serde::Deserialize;
use std::cmp::Ordering;

use super::{BrokerApiError, get_json};
use crate::core::AlpacaClient;

/// The market session a given instant falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketSession {
    /// Regular trading hours (typically 09:30-16:00 ET).
    Regular,
    /// Extended hours: pre-market and after-hours (typically
    /// 04:00-09:30 and 16:00-20:00 ET). Alpaca only allows
    /// `extended_hours: true` on limit orders, not market orders.
    Extended,
    /// Outside every trading session, including non-trading days.
    Closed,
}

/// Response from the Alpaca calendar endpoint
/// (<https://docs.alpaca.markets/reference/getcalendar-1>).
///
/// `date` identifies the trading day the entry describes, so callers can
/// verify the broker answered for the day they actually queried.
/// `open`/`close` are the regular trading hours (typically 09:30-16:00
/// ET). `session_open`/`session_close` span the full extended session
/// including pre-market and after-hours (typically 04:00-20:00 ET).
///
/// CONTRACT RISK: Alpaca's reference does not define `session_open`/
/// `session_close` semantics, and their observed values have changed over
/// time (community reports show 07:00/19:00 historically, 04:00/20:00
/// currently -- forum.alpaca.markets/t/2400). This module assumes they
/// span exactly the window in which Alpaca accepts `extended_hours: true`
/// limit orders, i.e. the 4:00-9:30/16:00-20:00 windows described in
/// <https://docs.alpaca.markets/docs/orders-at-alpaca#extended-hours-trading>.
/// If Alpaca redefines the session bounds (e.g. for 24/5 overnight
/// trading), `Extended` classification may cover times where
/// extended-hours limit orders are rejected; the failure mode is broker
/// rejections of the order, not silent misclassification of money
/// amounts.
#[derive(Debug, Clone, Deserialize)]
struct CalendarDay {
    date: NaiveDate,
    #[serde(deserialize_with = "deserialize_time")]
    open: NaiveTime,
    #[serde(deserialize_with = "deserialize_time")]
    close: NaiveTime,
    #[serde(deserialize_with = "deserialize_time")]
    session_open: NaiveTime,
    #[serde(deserialize_with = "deserialize_time")]
    session_close: NaiveTime,
}

fn deserialize_time<'de, D>(deserializer: D) -> Result<NaiveTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    NaiveTime::parse_from_str(&raw, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(&raw, "%H%M"))
        .map_err(serde::de::Error::custom)
}

/// Returns true if the market is currently open for regular trading.
///
/// # Errors
///
/// Returns the same errors as [`market_session_at`].
pub async fn is_market_open(client: &AlpacaClient) -> Result<bool, BrokerApiError> {
    is_market_open_at(client, Utc::now()).await
}

/// Returns the current market session (regular, extended, or closed).
///
/// # Errors
///
/// Returns the same errors as [`market_session_at`].
pub async fn market_session(client: &AlpacaClient) -> Result<MarketSession, BrokerApiError> {
    market_session_at(client, Utc::now()).await
}

/// Returns the market session at the given time.
///
/// The broker may answer a non-trading-day query with the NEAREST trading
/// day instead of an empty list. A LATER date is positive evidence the
/// queried day has no trading session, so it classifies as
/// [`MarketSession::Closed`] -- erroring would turn every weekend/holiday
/// query into an error instead of "closed". An EARLIER date proves nothing
/// about the queried day and indicates a broken response, so it fails fast
/// rather than classify against another day's session windows.
///
/// # Errors
///
/// Returns [`BrokerApiError::CalendarDateMismatch`] when the calendar
/// answers with an earlier date than queried, and
/// [`BrokerApiError::Alpaca`] on transport or API failures.
pub async fn market_session_at(
    client: &AlpacaClient,
    now: DateTime<Utc>,
) -> Result<MarketSession, BrokerApiError> {
    let now_et = now.with_timezone(&New_York);
    let today = now_et.date_naive();

    let calendar = get_calendar(client, today, today).await?;

    let Some(today_calendar) = calendar.into_iter().next() else {
        return Ok(MarketSession::Closed);
    };

    match today_calendar.date.cmp(&today) {
        Ordering::Greater => return Ok(MarketSession::Closed),
        Ordering::Less => {
            return Err(BrokerApiError::CalendarDateMismatch {
                queried: today,
                returned: today_calendar.date,
            });
        }
        Ordering::Equal => {}
    }

    let now_time = now_et.time();

    let session = if now_time >= today_calendar.open && now_time < today_calendar.close {
        MarketSession::Regular
    } else if now_time >= today_calendar.session_open && now_time < today_calendar.session_close {
        MarketSession::Extended
    } else {
        MarketSession::Closed
    };

    Ok(session)
}

/// Returns true if the market is open for regular trading at the given
/// time.
///
/// Derived from [`market_session_at`] so the regular-hours predicate
/// cannot drift from the session classification: it is true exactly when
/// the session is [`MarketSession::Regular`].
async fn is_market_open_at(
    client: &AlpacaClient,
    now: DateTime<Utc>,
) -> Result<bool, BrokerApiError> {
    Ok(market_session_at(client, now).await? == MarketSession::Regular)
}

async fn get_calendar(
    client: &AlpacaClient,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<CalendarDay>, BrokerApiError> {
    let url = format!(
        "{}/v1/calendar?start={}&end={}",
        client.base_url(),
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d")
    );

    get_json(client, &url).await
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::super::test_client;
    use super::*;

    fn mock_calendar_day(
        server: &MockServer,
        date: &str,
        open: &str,
        close: &str,
        session_open: &str,
        session_close: &str,
    ) {
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/calendar")
                .query_param("start", date)
                .query_param("end", date);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "date": date,
                        "open": open,
                        "close": close,
                        "session_open": session_open,
                        "session_close": session_close
                    }
                ]));
        });
    }

    fn mock_trading_day(server: &MockServer, date: &str) {
        mock_calendar_day(server, date, "09:30", "16:00", "0400", "2000");
    }

    fn mock_non_trading_day(server: &MockServer, date: &str) {
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/calendar")
                .query_param("start", date)
                .query_param("end", date);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([]));
        });
    }

    /// Constructs a UTC timestamp for a specific ET time on a given date.
    fn et_time_as_utc(date: &str, hour: u32, minute: u32) -> DateTime<Utc> {
        let naive_date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        let naive_time = NaiveTime::from_hms_opt(hour, minute, 0).unwrap();
        let naive_datetime = naive_date.and_time(naive_time);
        naive_datetime
            .and_local_timezone(New_York)
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn get_calendar_returns_market_hours() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let date = NaiveDate::from_ymd_opt(2025, 1, 6).unwrap();
        let calendar = get_calendar(&client, date, date).await.unwrap();

        assert_eq!(calendar.len(), 1);
        assert_eq!(calendar[0].open, NaiveTime::from_hms_opt(9, 30, 0).unwrap());
        assert_eq!(
            calendar[0].close,
            NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        );
    }

    #[test]
    fn calendar_day_deserializes_real_api_format() {
        let json = r#"{
            "date": "2025-01-06",
            "open": "09:30",
            "close": "16:00",
            "session_open": "0400",
            "session_close": "2000"
        }"#;

        let day: CalendarDay = serde_json::from_str(json).unwrap();

        assert_eq!(day.open, NaiveTime::from_hms_opt(9, 30, 0).unwrap());
        assert_eq!(day.close, NaiveTime::from_hms_opt(16, 0, 0).unwrap());
        assert_eq!(day.session_open, NaiveTime::from_hms_opt(4, 0, 0).unwrap());
        assert_eq!(
            day.session_close,
            NaiveTime::from_hms_opt(20, 0, 0).unwrap()
        );
    }

    #[test]
    fn calendar_day_accepts_hhmm_format_without_colon() {
        let json = r#"{
            "date": "2025-01-06",
            "open": "0930",
            "close": "1600",
            "session_open": "0400",
            "session_close": "2000"
        }"#;

        let day: CalendarDay = serde_json::from_str(json).unwrap();

        assert_eq!(day.open, NaiveTime::from_hms_opt(9, 30, 0).unwrap());
        assert_eq!(day.close, NaiveTime::from_hms_opt(16, 0, 0).unwrap());
    }

    #[test]
    fn calendar_day_rejects_invalid_hour() {
        let json = r#"{
            "date": "2025-01-06",
            "open": "25:30",
            "close": "16:00",
            "session_open": "0400",
            "session_close": "2000"
        }"#;

        let error = serde_json::from_str::<CalendarDay>(json).unwrap_err();
        assert!(
            error.to_string().contains("out of range"),
            "expected out of range error for hour 25, got: {error}"
        );
    }

    #[test]
    fn calendar_day_rejects_invalid_minute() {
        let json = r#"{
            "date": "2025-01-06",
            "open": "09:60",
            "close": "16:00",
            "session_open": "0400",
            "session_close": "2000"
        }"#;

        let error = serde_json::from_str::<CalendarDay>(json).unwrap_err();
        assert!(
            error.to_string().contains("out of range")
                || error.to_string().contains("invalid characters"),
            "expected parse error for minute 60, got: {error}"
        );
    }

    #[tokio::test]
    async fn is_market_open_during_trading_hours() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let midday = et_time_as_utc("2025-01-06", 12, 0);

        assert!(is_market_open_at(&client, midday).await.unwrap());
    }

    #[tokio::test]
    async fn is_market_closed_before_open() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let before_open = et_time_as_utc("2025-01-06", 9, 0);

        assert!(!is_market_open_at(&client, before_open).await.unwrap());
    }

    #[tokio::test]
    async fn is_market_open_false_during_extended_hours() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let pre_market = et_time_as_utc("2025-01-06", 7, 0);

        assert!(
            !is_market_open_at(&client, pre_market).await.unwrap(),
            "is_market_open must be true only during the Regular session, not pre-market"
        );
    }

    #[tokio::test]
    async fn is_market_closed_on_non_trading_day() {
        let server = MockServer::start();
        mock_non_trading_day(&server, "2025-01-04");

        let client = test_client(server.base_url());
        let saturday = et_time_as_utc("2025-01-04", 12, 0);

        assert!(!is_market_open_at(&client, saturday).await.unwrap());
    }

    #[tokio::test]
    async fn market_session_is_closed_when_calendar_returns_a_later_trading_day() {
        let server = MockServer::start();

        // Saturday query answered with Monday's entry (the nearest trading
        // day). A later date is positive evidence Saturday has no trading
        // session, so the session is Closed -- NOT an error, and NOT a
        // classification against Monday's session windows.
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/calendar")
                .query_param("start", "2025-01-04")
                .query_param("end", "2025-01-04");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "date": "2025-01-06",
                        "open": "09:30",
                        "close": "16:00",
                        "session_open": "0400",
                        "session_close": "2000"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        // 18:00 ET Saturday would classify Extended against Monday's
        // session windows if the date guard were missing.
        let saturday_evening = et_time_as_utc("2025-01-04", 18, 0);

        let session = market_session_at(&client, saturday_evening).await.unwrap();

        assert_eq!(session, MarketSession::Closed);
    }

    #[tokio::test]
    async fn market_session_errors_when_calendar_returns_an_earlier_date() {
        let server = MockServer::start();

        // An EARLIER date proves nothing about the queried day -- the
        // response is broken, so classification must fail fast rather than
        // trust another day's session windows.
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/calendar")
                .query_param("start", "2025-01-07")
                .query_param("end", "2025-01-07");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!([
                    {
                        "date": "2025-01-06",
                        "open": "09:30",
                        "close": "16:00",
                        "session_open": "0400",
                        "session_close": "2000"
                    }
                ]));
        });

        let client = test_client(server.base_url());
        let tuesday_midday = et_time_as_utc("2025-01-07", 12, 0);

        let error = market_session_at(&client, tuesday_midday)
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                BrokerApiError::CalendarDateMismatch { queried, returned }
                    if queried == NaiveDate::from_ymd_opt(2025, 1, 7).unwrap()
                        && returned == NaiveDate::from_ymd_opt(2025, 1, 6).unwrap()
            ),
            "expected CalendarDateMismatch, got: {error:?}"
        );
    }

    #[tokio::test]
    async fn market_session_regular_during_trading_hours() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let midday = et_time_as_utc("2025-01-06", 12, 0);

        assert_eq!(
            market_session_at(&client, midday).await.unwrap(),
            MarketSession::Regular
        );
    }

    #[tokio::test]
    async fn market_session_extended_pre_market() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let pre_market = et_time_as_utc("2025-01-06", 7, 0);

        assert_eq!(
            market_session_at(&client, pre_market).await.unwrap(),
            MarketSession::Extended,
            "7:00 AM ET is pre-market (between session_open 4:00 and open 9:30)"
        );
    }

    #[tokio::test]
    async fn market_session_extended_after_hours() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let after_hours = et_time_as_utc("2025-01-06", 18, 0);

        assert_eq!(
            market_session_at(&client, after_hours).await.unwrap(),
            MarketSession::Extended,
            "6:00 PM ET is after-hours (between close 16:00 and session_close 20:00)"
        );
    }

    #[tokio::test]
    async fn market_session_closed_before_extended_session() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let overnight = et_time_as_utc("2025-01-06", 3, 0);

        assert_eq!(
            market_session_at(&client, overnight).await.unwrap(),
            MarketSession::Closed,
            "3:00 AM ET is before session_open (4:00), should be Closed"
        );
    }

    #[tokio::test]
    async fn market_session_closed_after_extended_session() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let late_night = et_time_as_utc("2025-01-06", 21, 0);

        assert_eq!(
            market_session_at(&client, late_night).await.unwrap(),
            MarketSession::Closed,
            "9:00 PM ET is after session_close (20:00), should be Closed"
        );
    }

    #[tokio::test]
    async fn market_session_uses_early_close_calendar_boundaries() {
        let server = MockServer::start();
        mock_calendar_day(&server, "2025-07-03", "09:30", "13:00", "0400", "1700");

        let client = test_client(server.base_url());
        let after_regular_close = et_time_as_utc("2025-07-03", 13, 1);
        let session_close = et_time_as_utc("2025-07-03", 17, 0);

        assert_eq!(
            market_session_at(&client, after_regular_close)
                .await
                .unwrap(),
            MarketSession::Extended,
            "After an early regular close should be Extended until session_close"
        );
        assert_eq!(
            market_session_at(&client, session_close).await.unwrap(),
            MarketSession::Closed,
            "Exactly at early session_close should be Closed"
        );
    }

    #[tokio::test]
    async fn market_session_closed_on_non_trading_day() {
        let server = MockServer::start();
        mock_non_trading_day(&server, "2025-01-04");

        let client = test_client(server.base_url());
        let saturday = et_time_as_utc("2025-01-04", 12, 0);

        assert_eq!(
            market_session_at(&client, saturday).await.unwrap(),
            MarketSession::Closed
        );
    }

    #[tokio::test]
    async fn market_session_extended_at_session_open_boundary() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let at_session_open = et_time_as_utc("2025-01-06", 4, 0);

        assert_eq!(
            market_session_at(&client, at_session_open).await.unwrap(),
            MarketSession::Extended,
            "Exactly at session_open should be Extended"
        );
    }

    #[tokio::test]
    async fn market_session_regular_at_regular_open_boundary() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let at_open = et_time_as_utc("2025-01-06", 9, 30);

        assert_eq!(
            market_session_at(&client, at_open).await.unwrap(),
            MarketSession::Regular,
            "Exactly at regular open should be Regular"
        );
    }

    #[tokio::test]
    async fn market_session_extended_at_regular_close_boundary() {
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let at_close = et_time_as_utc("2025-01-06", 16, 0);

        assert_eq!(
            market_session_at(&client, at_close).await.unwrap(),
            MarketSession::Extended,
            "Exactly at regular close transitions to Extended (after-hours)"
        );
    }

    #[tokio::test]
    async fn market_session_closed_at_session_close_boundary() {
        // The extended window is half-open: `now < session_close`, so 20:00
        // ET exactly (the documented after-hours close) is already Closed.
        // Pins the top edge of the session so a `<=` regression would be
        // caught.
        let server = MockServer::start();
        mock_trading_day(&server, "2025-01-06");

        let client = test_client(server.base_url());
        let at_session_close = et_time_as_utc("2025-01-06", 20, 0);

        assert_eq!(
            market_session_at(&client, at_session_close).await.unwrap(),
            MarketSession::Closed,
            "Exactly at session_close (20:00 ET) the extended session has ended -> Closed"
        );
    }
}
