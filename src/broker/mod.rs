//! Alpaca Broker API trading surface.
//!
//! Raw HTTP operations against the Broker API -- order placement and
//! lookup, USDC/USD conversion orders, security journals, positions,
//! account details, account activities, market hours, and asset lookup --
//! plus the wire types those calls exchange.
//!
//! Equity quantities use [`st0x_finance::FractionalShares`], USD values use
//! [`st0x_finance::Usd`], and conversion amounts use [`st0x_finance::Usdc`].
//! Heterogeneous position quantities remain asset-dependent wire decimals.
//! This module is telemetry-free: consumers wrap calls with their own
//! instrumentation.

use chrono::NaiveDate;
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use st0x_finance::{EmptySymbolError, FloatError, Usdc};
use thiserror::Error;
use uuid::Uuid;

use crate::core::{AlpacaClient, AlpacaError, Backpressure, Permanence};
use crate::rate_limit::retry_after_from_response_headers;

mod account;
mod activity;
mod asset;
mod conversion;
mod journal;
mod market_hours;
mod order;
mod positions;

pub use account::{Account, AccountDetails, AccountStatus, get_account_details, verify_account};
pub use activity::{AccountActivitiesQuery, AccountActivity, get_account_activities};
pub use asset::{Asset, AssetStatus, get_asset};
pub use conversion::{
    ConversionDirection, CryptoOrderFailureReason, CryptoOrderOutcome, CryptoOrderRequest,
    CryptoOrderResponse, convert_usdc_usd, get_crypto_order, get_crypto_order_by_client_order_id,
    place_crypto_order, poll_crypto_order_until_filled,
};
pub use journal::{JournalResponse, JournalStatus, create_journal};
pub use market_hours::{
    MarketSession, MarketSessionBounds, MarketSessionDetails, MarketSessionStatus, PostCloseGap,
    is_market_open, market_session, market_session_at, market_session_details,
    market_session_details_at, market_session_status, market_session_status_at,
};
pub use order::{
    CancellationOutcome, ClientOrderId, LimitOrderRequest, OrderRequest, OrderResponse, OrderSide,
    OrderStatus, ParseTimeInForceError, TimeInForce, cancel_order, get_order,
    get_order_by_client_order_id, place_limit_order, place_order,
};
pub use positions::{Position, list_positions};

pub use crate::core::Symbol;

/// Errors from the Broker API surface.
///
/// Transport, parse, and API-status failures are carried by the shared
/// [`AlpacaError`] taxonomy; the remaining variants are Broker-surface
/// invariants detected client-side.
#[derive(Debug, Error)]
pub enum BrokerApiError {
    #[error(transparent)]
    Alpaca(#[from] AlpacaError),

    #[error("Crypto order {order_id} failed: {reason}")]
    CryptoOrderFailed {
        order_id: Uuid,
        reason: conversion::CryptoOrderFailureReason,
    },

    #[error(
        "Calendar endpoint returned an entry for {returned} when {queried} was \
         requested; refusing to classify the market session from another day's hours"
    )]
    CalendarDateMismatch {
        queried: NaiveDate,
        returned: NaiveDate,
    },

    #[error("calendar bounds were missing for trading day {date}")]
    MissingCalendarBounds { date: NaiveDate },

    #[error("calendar date overflow after {date}")]
    CalendarDateOverflow { date: NaiveDate },

    #[error("calendar time {date} {time} is ambiguous or nonexistent in America/New_York")]
    InvalidCalendarLocalTime {
        date: NaiveDate,
        time: chrono::NaiveTime,
    },

    #[error("Invalid Alpaca account activities URL {url}")]
    InvalidAccountActivitiesUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },

    #[error("Alpaca account activities pagination returned the same page token twice")]
    AccountActivitiesPaginationInvariantViolation,

    #[error("Alpaca account activities pagination exceeded {pages} pages")]
    AccountActivitiesPageLimitExceeded { pages: usize },

    #[error(
        "USDC conversion amount {amount} is below Alpaca's \
         {max_decimals}-decimal-place precision"
    )]
    UsdcBelowPrecision { amount: Usdc, max_decimals: u32 },

    #[error(
        "USDC conversion amount {amount} exceeds Alpaca's \
         {max_decimals}-decimal-place precision"
    )]
    UsdcPrecisionExceeded { amount: Usdc, max_decimals: u32 },

    #[error("USDC conversion amount {amount} must be positive")]
    UsdcNonPositive { amount: Usdc },

    #[error("failed to validate USDC conversion precision: {0}")]
    UsdcPrecisionValidation(#[from] FloatError),

    #[error("invalid broker symbol: {0}")]
    InvalidSymbol(#[from] EmptySymbolError),
}

impl BrokerApiError {
    /// Returns broker backpressure metadata when the underlying request was
    /// rate limited.
    #[must_use]
    pub fn backpressure(&self) -> Option<Backpressure> {
        match self {
            Self::Alpaca(error) => error.backpressure(),
            _ => None,
        }
    }

    /// Classifies whether retrying the same operation can plausibly succeed.
    #[must_use]
    pub fn permanence(&self) -> Permanence {
        match self {
            Self::Alpaca(error) => error.permanence(),
            Self::CalendarDateMismatch { .. } => Permanence::Transient,
            Self::CryptoOrderFailed { .. }
            | Self::MissingCalendarBounds { .. }
            | Self::CalendarDateOverflow { .. }
            | Self::InvalidCalendarLocalTime { .. }
            | Self::InvalidAccountActivitiesUrl { .. }
            | Self::AccountActivitiesPaginationInvariantViolation
            | Self::AccountActivitiesPageLimitExceeded { .. }
            | Self::UsdcBelowPrecision { .. }
            | Self::UsdcPrecisionExceeded { .. }
            | Self::UsdcNonPositive { .. }
            | Self::UsdcPrecisionValidation(_)
            | Self::InvalidSymbol(_) => Permanence::Permanent,
        }
    }
}

/// Sends an authenticated GET and parses the JSON response body.
async fn get_json<Response: DeserializeOwned>(
    client: &AlpacaClient,
    url: &str,
) -> Result<Response, BrokerApiError> {
    request_json(client.get(url).await?).await
}

/// Sends an authenticated POST with a JSON body and parses the JSON
/// response body.
async fn post_json<Response: DeserializeOwned, Body: Serialize + Sync>(
    client: &AlpacaClient,
    url: &str,
    body: &Body,
) -> Result<Response, BrokerApiError> {
    request_json(client.post(url).await?.json(body)).await
}

/// Sends an authenticated DELETE, expecting no response body.
async fn delete(client: &AlpacaClient, url: &str) -> Result<(), BrokerApiError> {
    let response = client
        .delete(url)
        .await?
        .send()
        .await
        .map_err(AlpacaError::from)?;
    let status = response.status();
    let retry_after = retry_after_from_response_headers(response.headers());

    if status.is_success() {
        return Ok(());
    }

    let bytes = response.bytes().await.map_err(AlpacaError::from)?;

    Err(api_error(status, &bytes, retry_after))
}

async fn request_json<Response: DeserializeOwned>(
    builder: reqwest::RequestBuilder,
) -> Result<Response, BrokerApiError> {
    let response = builder.send().await.map_err(AlpacaError::from)?;
    let status = response.status();
    let retry_after = retry_after_from_response_headers(response.headers());
    // Read raw bytes and parse successful responses with `from_slice` so
    // invalid UTF-8 fails fast rather than being silently replaced by lossy
    // decoding before parse. Lossy decoding is fine for the error body only.
    let bytes = response.bytes().await.map_err(AlpacaError::from)?;

    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|source| {
            BrokerApiError::Alpaca(AlpacaError::Parse {
                body: String::from_utf8_lossy(&bytes).into_owned(),
                source,
            })
        });
    }

    Err(api_error(status, &bytes, retry_after))
}

/// Maps a non-2xx response into the shared [`AlpacaError::Api`] variant,
/// preserving the raw (lossy-decoded) body for diagnostics.
fn api_error(
    status: StatusCode,
    bytes: &[u8],
    retry_after: Option<std::time::Duration>,
) -> BrokerApiError {
    let body = String::from_utf8_lossy(bytes).into_owned();
    if status == StatusCode::TOO_MANY_REQUESTS {
        BrokerApiError::Alpaca(AlpacaError::RateLimited { body, retry_after })
    } else {
        BrokerApiError::Alpaca(AlpacaError::Api {
            status_code: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
pub(crate) const TEST_ACCOUNT_ID: &str = "904837e3-3b76-47ec-b432-046db621571b";

#[cfg(test)]
pub(crate) fn test_client(base_url: String) -> AlpacaClient {
    AlpacaClient::new(
        base_url,
        TEST_ACCOUNT_ID.to_string(),
        "test_key_id".to_string(),
        "test_secret_key".to_string(),
        std::time::Duration::from_secs(10),
        std::time::Duration::from_secs(30),
    )
    .unwrap()
}
