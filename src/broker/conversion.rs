//! USDC/USD conversion orders on Alpaca's crypto trading pair.
//!
//! Converting USDC to USD buying power sells `USDCUSD`; converting USD
//! buying power to USDC buys it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_finance::{HasZero, Symbol, Usd, Usdc};
use uuid::Uuid;

use super::order::{ClientOrderId, OrderSide, OrderStatus};
use super::{BrokerApiError, get_json, post_json};
use crate::core::{AlpacaClient, AlpacaError};

const ALPACA_CRYPTO_MAX_DECIMAL_PLACES: u32 = 6;

/// Direction for USDC/USD conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionDirection {
    /// Convert USDC to USD buying power (sell USDC/USD)
    UsdcToUsd,
    /// Convert USD buying power to USDC (buy USDC/USD)
    UsdToUsdc,
}

/// Order request for crypto trading (e.g., USDC/USD conversion).
/// Uses decimal quantity and trading pair symbol format.
#[derive(Debug, Serialize)]
pub struct CryptoOrderRequest {
    /// Trading pair symbol (e.g., "USDCUSD" for USDC/USD)
    pub symbol: Symbol,
    /// Quantity of the base asset (e.g., USDC amount)
    #[serde(rename = "qty")]
    pub quantity: Usdc,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub time_in_force: &'static str,
    /// Caller-supplied idempotency/correlation key. Recorded before
    /// placement so a crashed conversion can be looked up by this key on
    /// resume.
    pub client_order_id: ClientOrderId,
}

/// Response from a crypto order placement
#[derive(Debug, Clone, Deserialize)]
pub struct CryptoOrderResponse {
    pub id: Uuid,
    pub symbol: Symbol,
    #[serde(rename = "qty")]
    pub quantity: Usdc,
    status: OrderStatus,
    #[serde(rename = "filled_avg_price")]
    pub filled_average_price: Option<Usd>,
    #[serde(rename = "filled_qty")]
    pub filled_quantity: Option<Usdc>,
    pub created_at: DateTime<Utc>,
}

/// Terminal failure states for crypto orders.
///
/// Every non-fill terminal [`OrderStatus`] maps to one of these so a
/// conversion resume path never treats an unexpected terminal status as
/// still-pending (which would retry forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoOrderFailureReason {
    Canceled,
    Expired,
    Rejected,
    DoneForDay,
    Replaced,
    Suspended,
    Calculated,
}

impl std::fmt::Display for CryptoOrderFailureReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canceled => formatter.write_str("Canceled"),
            Self::Expired => formatter.write_str("Expired"),
            Self::Rejected => formatter.write_str("Rejected"),
            Self::DoneForDay => formatter.write_str("DoneForDay"),
            Self::Replaced => formatter.write_str("Replaced"),
            Self::Suspended => formatter.write_str("Suspended"),
            Self::Calculated => formatter.write_str("Calculated"),
        }
    }
}

/// Terminal/intermediate decision for a crypto order, exposing the outcome
/// without leaking the raw broker status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoOrderOutcome {
    Filled,
    Pending,
    Failed(CryptoOrderFailureReason),
}

impl CryptoOrderResponse {
    /// Returns the status as a display-friendly string.
    #[must_use]
    pub fn status_display(&self) -> &'static str {
        match self.status {
            OrderStatus::Filled => "filled",
            OrderStatus::New => "new",
            OrderStatus::PendingNew => "pending_new",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Canceled => "canceled",
            OrderStatus::Expired => "expired",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Accepted => "accepted",
            _ => "other",
        }
    }

    /// Classifies the order's current status into a fill/pending/failed
    /// outcome.
    ///
    /// The match is exhaustive (no wildcard) so a newly added Alpaca status
    /// forces a compile error here rather than silently mapping to
    /// `Pending` and retrying forever.
    #[must_use]
    pub fn classify(&self) -> CryptoOrderOutcome {
        let reason = match self.status {
            OrderStatus::Filled => return CryptoOrderOutcome::Filled,
            OrderStatus::New
            | OrderStatus::PendingNew
            | OrderStatus::PartiallyFilled
            | OrderStatus::Accepted
            | OrderStatus::AcceptedForBidding
            | OrderStatus::PendingCancel
            | OrderStatus::PendingReplace
            | OrderStatus::Stopped => return CryptoOrderOutcome::Pending,
            OrderStatus::Canceled => CryptoOrderFailureReason::Canceled,
            OrderStatus::Expired => CryptoOrderFailureReason::Expired,
            OrderStatus::Rejected => CryptoOrderFailureReason::Rejected,
            OrderStatus::DoneForDay => CryptoOrderFailureReason::DoneForDay,
            OrderStatus::Replaced => CryptoOrderFailureReason::Replaced,
            OrderStatus::Suspended => CryptoOrderFailureReason::Suspended,
            OrderStatus::Calculated => CryptoOrderFailureReason::Calculated,
        };

        CryptoOrderOutcome::Failed(reason)
    }
}

/// Places a crypto order (e.g., USDC/USD conversion).
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses, and unparseable response bodies.
pub async fn place_crypto_order(
    client: &AlpacaClient,
    request: &CryptoOrderRequest,
) -> Result<CryptoOrderResponse, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders",
        client.base_url(),
        client.account_id()
    );

    post_json(client, &url, request).await
}

/// Gets a crypto order by its Alpaca-assigned id.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses, and unparseable response bodies.
pub async fn get_crypto_order(
    client: &AlpacaClient,
    order_id: Uuid,
) -> Result<CryptoOrderResponse, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders/{order_id}",
        client.base_url(),
        client.account_id()
    );

    get_json(client, &url).await
}

/// Gets a crypto order by its `client_order_id`. Returns `None` only on a
/// 404 (Alpaca's documented not-found response for this lookup) -- i.e.
/// the order was never placed.
///
/// Every other error status is propagated rather than mapped to `None`.
/// This is deliberate: a transient failure (5xx, rate limit) on an order
/// that WAS placed must retry, not be mistaken for "never placed" --
/// mapping it to `None` would wrongly fail a still-settling conversion and
/// lose the converted USDC.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses other than 404, and unparseable response bodies.
pub async fn get_crypto_order_by_client_order_id(
    client: &AlpacaClient,
    client_order_id: &ClientOrderId,
) -> Result<Option<CryptoOrderResponse>, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders:by_client_order_id?client_order_id={client_order_id}",
        client.base_url(),
        client.account_id()
    );

    match get_json::<CryptoOrderResponse>(client, &url).await {
        Ok(order) => Ok(Some(order)),
        Err(BrokerApiError::Alpaca(AlpacaError::Api {
            status_code: 404, ..
        })) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Converts USDC to/from USD on Alpaca via a market order on the USDC/USD
/// trading pair.
///
/// # Errors
///
/// Returns [`BrokerApiError::UsdcBelowPrecision`] /
/// [`BrokerApiError::UsdcPrecisionExceeded`] when `amount` does not fit
/// Alpaca's 6-decimal-place crypto precision, and
/// [`BrokerApiError::Alpaca`] on transport or API failures.
pub async fn convert_usdc_usd(
    client: &AlpacaClient,
    amount: Usdc,
    direction: ConversionDirection,
    client_order_id: &ClientOrderId,
) -> Result<CryptoOrderResponse, BrokerApiError> {
    let placed_amount = validate_usdc_amount_for_alpaca_precision(amount)?;
    let side = match direction {
        ConversionDirection::UsdcToUsd => OrderSide::Sell,
        ConversionDirection::UsdToUsdc => OrderSide::Buy,
    };

    let request = CryptoOrderRequest {
        symbol: Symbol::new("USDCUSD")?,
        quantity: placed_amount,
        side,
        order_type: "market",
        time_in_force: "gtc",
        client_order_id: client_order_id.clone(),
    };

    place_crypto_order(client, &request).await
}

/// Polls a crypto order's status until it reaches a terminal state.
///
/// # Errors
///
/// Returns [`BrokerApiError::CryptoOrderFailed`] when the order reaches any
/// terminal failure state, and [`BrokerApiError::Alpaca`] on transport or API
/// failures during polling.
pub async fn poll_crypto_order_until_filled(
    client: &AlpacaClient,
    order_id: Uuid,
) -> Result<CryptoOrderResponse, BrokerApiError> {
    loop {
        let order = get_crypto_order(client, order_id).await?;

        match order.classify() {
            CryptoOrderOutcome::Filled => return Ok(order),
            CryptoOrderOutcome::Pending => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            CryptoOrderOutcome::Failed(reason) => {
                return Err(BrokerApiError::CryptoOrderFailed { order_id, reason });
            }
        }
    }
}

fn validate_usdc_amount_for_alpaca_precision(amount: Usdc) -> Result<Usdc, BrokerApiError> {
    if amount.is_zero()? || amount.is_negative()? {
        return Err(BrokerApiError::UsdcNonPositive { amount });
    }

    let (truncated, dust) = amount.truncate_to_decimals(ALPACA_CRYPTO_MAX_DECIMAL_PLACES)?;

    if truncated == Usdc::ZERO {
        return Err(BrokerApiError::UsdcBelowPrecision {
            amount,
            max_decimals: ALPACA_CRYPTO_MAX_DECIMAL_PLACES,
        });
    }

    if dust != Usdc::ZERO {
        return Err(BrokerApiError::UsdcPrecisionExceeded {
            amount,
            max_decimals: ALPACA_CRYPTO_MAX_DECIMAL_PLACES,
        });
    }

    Ok(amount)
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::uuid;

    use super::super::{TEST_ACCOUNT_ID, test_client};
    use super::*;

    fn usdc(value: &str) -> Usdc {
        value.parse().unwrap()
    }

    #[test]
    fn validate_usdc_amount_rejects_negative_amounts() {
        let amount = usdc("-1");

        assert!(matches!(
            validate_usdc_amount_for_alpaca_precision(amount),
            Err(BrokerApiError::UsdcNonPositive { amount: rejected }) if rejected == amount
        ));
    }

    #[test]
    fn classify_maps_every_broker_status_to_its_outcome() {
        let cases = [
            ("filled", CryptoOrderOutcome::Filled),
            ("new", CryptoOrderOutcome::Pending),
            ("pending_new", CryptoOrderOutcome::Pending),
            ("partially_filled", CryptoOrderOutcome::Pending),
            ("accepted", CryptoOrderOutcome::Pending),
            ("accepted_for_bidding", CryptoOrderOutcome::Pending),
            ("pending_cancel", CryptoOrderOutcome::Pending),
            ("pending_replace", CryptoOrderOutcome::Pending),
            ("stopped", CryptoOrderOutcome::Pending),
            (
                "canceled",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Canceled),
            ),
            (
                "expired",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Expired),
            ),
            (
                "rejected",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Rejected),
            ),
            (
                "done_for_day",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::DoneForDay),
            ),
            (
                "replaced",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Replaced),
            ),
            (
                "suspended",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Suspended),
            ),
            (
                "calculated",
                CryptoOrderOutcome::Failed(CryptoOrderFailureReason::Calculated),
            ),
        ];

        for (status, expected) in cases {
            let order: CryptoOrderResponse = serde_json::from_value(json!({
                "id": "904837e3-3b76-47ec-b432-046db621571b",
                "symbol": "USDCUSD",
                "qty": "100",
                "status": status,
                "created_at": "2025-01-06T12:00:00Z"
            }))
            .unwrap();

            assert_eq!(order.classify(), expected, "status {status} misclassified");
        }
    }

    #[test]
    fn crypto_order_response_status_display() {
        let cases = [
            ("filled", "filled"),
            ("new", "new"),
            ("pending_new", "pending_new"),
            ("partially_filled", "partially_filled"),
            ("canceled", "canceled"),
            ("expired", "expired"),
            ("rejected", "rejected"),
            ("accepted", "accepted"),
            ("suspended", "other"),
        ];

        for (status, expected) in cases {
            let order: CryptoOrderResponse = serde_json::from_value(json!({
                "id": "904837e3-3b76-47ec-b432-046db621571b",
                "symbol": "USDCUSD",
                "qty": "100",
                "status": status,
                "created_at": "2025-01-06T12:00:00Z"
            }))
            .unwrap();

            assert_eq!(order.status_display(), expected);
        }
    }

    #[tokio::test]
    async fn convert_usdc_to_usd_places_sell_order() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders"))
                .json_body(json!({
                    "symbol": "USDCUSD",
                    "qty": "100",
                    "side": "sell",
                    "type": "market",
                    "time_in_force": "gtc",
                    "client_order_id": "55555555-5555-4555-8555-555555555555"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "USDCUSD",
                    "qty": "100",
                    "status": "filled",
                    "filled_avg_price": "1.00",
                    "filled_qty": "100",
                    "created_at": "2025-01-06T12:00:00Z"
                }));
        });

        let client = test_client(server.base_url());
        let client_order_id = ClientOrderId("55555555-5555-4555-8555-555555555555".to_string());
        let response = convert_usdc_usd(
            &client,
            usdc("100"),
            ConversionDirection::UsdcToUsd,
            &client_order_id,
        )
        .await
        .unwrap();

        mock.assert();
        assert_eq!(response.symbol, "USDCUSD");
        assert_eq!(response.quantity, usdc("100"));
        assert_eq!(response.classify(), CryptoOrderOutcome::Filled);
    }

    #[tokio::test]
    async fn convert_usd_to_usdc_places_buy_order() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders"))
                .json_body(json!({
                    "symbol": "USDCUSD",
                    "qty": "250.5",
                    "side": "buy",
                    "type": "market",
                    "time_in_force": "gtc",
                    "client_order_id": "66666666-6666-4666-8666-666666666666"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "USDCUSD",
                    "qty": "250.5",
                    "status": "new",
                    "created_at": "2025-01-06T12:00:00Z"
                }));
        });

        let client = test_client(server.base_url());
        let client_order_id = ClientOrderId("66666666-6666-4666-8666-666666666666".to_string());
        let response = convert_usdc_usd(
            &client,
            usdc("250.5"),
            ConversionDirection::UsdToUsdc,
            &client_order_id,
        )
        .await
        .unwrap();

        mock.assert();
        assert_eq!(response.classify(), CryptoOrderOutcome::Pending);
    }

    #[tokio::test]
    async fn convert_usdc_usd_rejects_excess_precision() {
        let server = MockServer::start();
        let client = test_client(server.base_url());
        let client_order_id = ClientOrderId("77777777-7777-4777-8777-777777777777".to_string());

        let error = convert_usdc_usd(
            &client,
            usdc("100.1234567"),
            ConversionDirection::UsdcToUsd,
            &client_order_id,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            BrokerApiError::UsdcPrecisionExceeded {
                max_decimals: 6,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn convert_usdc_usd_rejects_amount_below_precision() {
        let server = MockServer::start();
        let client = test_client(server.base_url());
        let client_order_id = ClientOrderId("88888888-8888-4888-8888-888888888888".to_string());

        let error = convert_usdc_usd(
            &client,
            usdc("0.0000001"),
            ConversionDirection::UsdcToUsd,
            &client_order_id,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            BrokerApiError::UsdcBelowPrecision {
                max_decimals: 6,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn get_crypto_order_by_client_order_id_maps_404_to_none() {
        let server = MockServer::start();
        let client_order_id = ClientOrderId("99999999-9999-4999-8999-999999999999".to_string());

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders:by_client_order_id"
            ));
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({"code": 40_410_000_u64, "message": "order not found"}));
        });

        let client = test_client(server.base_url());
        let response = get_crypto_order_by_client_order_id(&client, &client_order_id)
            .await
            .unwrap();

        mock.assert();
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn get_crypto_order_by_client_order_id_propagates_5xx() {
        let server = MockServer::start();
        let client_order_id = ClientOrderId("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string());

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders:by_client_order_id"
            ));
            then.status(500)
                .header("content-type", "application/json")
                .json_body(json!({"message": "internal error"}));
        });

        let client = test_client(server.base_url());
        let error = get_crypto_order_by_client_order_id(&client, &client_order_id)
            .await
            .unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, .. }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 500);
    }

    #[tokio::test]
    async fn poll_crypto_order_returns_failure_reason_for_every_terminal_failure() {
        let cases = [
            ("canceled", CryptoOrderFailureReason::Canceled),
            ("expired", CryptoOrderFailureReason::Expired),
            ("rejected", CryptoOrderFailureReason::Rejected),
            ("done_for_day", CryptoOrderFailureReason::DoneForDay),
            ("replaced", CryptoOrderFailureReason::Replaced),
            ("suspended", CryptoOrderFailureReason::Suspended),
            ("calculated", CryptoOrderFailureReason::Calculated),
        ];

        for (status, expected_reason) in cases {
            let server = MockServer::start();
            let order_id = uuid!("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");

            let mock = server.mock(|when, then| {
                when.method(GET).path(format!(
                    "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
                ));
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "id": order_id,
                        "symbol": "USDCUSD",
                        "qty": "100",
                        "status": status,
                        "created_at": "2025-01-06T12:00:00Z"
                    }));
            });

            let client = test_client(server.base_url());
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                poll_crypto_order_until_filled(&client, order_id),
            )
            .await;

            mock.assert();
            assert!(
                matches!(
                    result,
                    Ok(Err(BrokerApiError::CryptoOrderFailed { reason, .. }))
                        if reason == expected_reason
                ),
                "terminal status {status} did not return {expected_reason}"
            );
        }
    }
}
