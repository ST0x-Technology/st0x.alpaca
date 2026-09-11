//! Shared parsing and classification for Alpaca rate-limit responses.

use std::str::FromStr;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Parses either form of an HTTP `Retry-After` header.
#[must_use]
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();

    if let Ok(seconds) = u64::from_str(value) {
        return Some(Duration::from_secs(seconds));
    }

    let date = DateTime::parse_from_rfc2822(value).ok()?;
    let time: SystemTime = date.with_timezone(&Utc).into();
    time.duration_since(now).ok()
}

/// Reads a usable `Retry-After` value from response headers.
#[must_use]
pub fn retry_after_from_response_headers(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after(headers.get(RETRY_AFTER)?.to_str().ok()?, SystemTime::now())
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, RETRY_AFTER};

    use super::*;

    #[test]
    fn parses_delay_seconds() {
        assert_eq!(
            parse_retry_after("120", SystemTime::UNIX_EPOCH),
            Some(Duration::from_mins(2))
        );
    }

    #[test]
    fn parses_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_717);

        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_mins(1))
        );
    }

    #[test]
    fn reads_header() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "30".parse().unwrap());

        assert_eq!(
            retry_after_from_response_headers(&headers),
            Some(Duration::from_secs(30))
        );
    }
}
