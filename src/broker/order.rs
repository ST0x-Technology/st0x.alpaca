//! Equity order placement, lookup, and cancellation against the Broker
//! API, plus the order wire types shared with the crypto conversion
//! surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use st0x_finance::{FractionalShares, Usd};
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

use super::{BrokerApiError, Symbol, delete, get_json, post_json};
use crate::core::{AlpacaClient, AlpacaError};

/// Caller-supplied idempotency/correlation key for order placement.
///
/// Alpaca rejects a re-used `client_order_id` on an active order with a
/// 422 (`client_order_id` must be unique); callers reconcile via
/// [`get_order_by_client_order_id`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientOrderId(pub String);

impl std::fmt::Display for ClientOrderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Time-in-force specifies how long an order remains active before it
/// expires. Serializes to Alpaca's wire strings (`day`, `cls`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Day order - expires at the end of the regular trading day
    #[default]
    #[serde(rename = "day")]
    Day,
    /// Market-on-close - executes at or near the market close price.
    /// Orders placed between 3:50pm-7:00pm ET are rejected.
    /// Orders after 7pm ET are queued for the next trading day.
    #[serde(rename = "cls")]
    MarketOnClose,
}

impl std::fmt::Display for TimeInForce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Day => write!(formatter, "day"),
            Self::MarketOnClose => write!(formatter, "market-on-close"),
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid time-in-force: {time_in_force_provided}")]
pub struct ParseTimeInForceError {
    time_in_force_provided: String,
}

impl FromStr for TimeInForce {
    type Err = ParseTimeInForceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "day" => Ok(Self::Day),
            "market-on-close" | "market_on_close" | "cls" => Ok(Self::MarketOnClose),
            _ => Err(ParseTimeInForceError {
                time_in_force_provided: value.to_string(),
            }),
        }
    }
}

/// Order side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Order status from the Alpaca Broker API
/// (<https://docs.alpaca.markets/reference/getorderforaccount>).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    New,
    PendingNew,
    PartiallyFilled,
    Filled,
    DoneForDay,
    Canceled,
    Expired,
    Replaced,
    PendingCancel,
    PendingReplace,
    Rejected,
    Suspended,
    Calculated,
    Stopped,
    AcceptedForBidding,
    Accepted,
}

/// Request body for placing a market order.
///
/// The `quantity` field must already be truncated to Alpaca's decimal
/// precision before constructing this struct.
#[derive(Debug, Serialize)]
pub struct OrderRequest {
    pub symbol: Symbol,
    #[serde(rename = "qty")]
    pub quantity: FractionalShares,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub time_in_force: TimeInForce,
    /// Alpaca only allows `extended_hours: true` for limit orders, not
    /// market orders.
    pub extended_hours: bool,
    pub client_order_id: ClientOrderId,
}

/// Request body for placing a limit order.
///
/// The `quantity` field must already be truncated to Alpaca's decimal
/// precision, and `limit_price` to Alpaca's price precision, before
/// constructing this struct.
#[derive(Debug, Serialize)]
pub struct LimitOrderRequest {
    pub symbol: Symbol,
    #[serde(rename = "qty")]
    pub quantity: FractionalShares,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub limit_price: Usd,
    pub time_in_force: TimeInForce,
    pub extended_hours: bool,
    pub client_order_id: ClientOrderId,
}

/// Order response from the Alpaca Broker API
/// (<https://docs.alpaca.markets/reference/getorderforaccount>).
#[derive(Debug, Deserialize)]
pub struct OrderResponse {
    pub id: Uuid,
    pub symbol: Symbol,
    #[serde(rename = "qty")]
    pub quantity: FractionalShares,
    #[serde(rename = "filled_qty")]
    pub filled_quantity: Option<FractionalShares>,
    pub side: OrderSide,
    pub status: OrderStatus,
    #[serde(rename = "filled_avg_price")]
    pub filled_average_price: Option<Usd>,
    /// Whether the broker holds this as an extended-hours order. `Option`
    /// so an omitted echo is distinguishable from a real `false`: adoption
    /// paths fall back to the request's terms when the broker omits the
    /// field.
    pub extended_hours: Option<bool>,
    /// The broker-held limit price, present for limit orders.
    pub limit_price: Option<Usd>,
    /// Broker-side timestamps from the documented order entity. The order
    /// entity marks every timestamp nullable, so callers must handle
    /// omissions explicitly rather than assume presence.
    pub updated_at: Option<DateTime<Utc>>,
    pub filled_at: Option<DateTime<Utc>>,
    pub canceled_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
}

/// Outcome of a cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationOutcome {
    /// The broker accepted the cancellation request.
    Requested,
    /// The broker does not recognise the order id (404); the caller must
    /// resolve the order as terminal instead of waiting for a cancellation
    /// confirmation that a status poll (which would also 404) can never
    /// deliver.
    OrderNotFound,
}

/// Places an order (market or limit, per the request body).
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses (including the 422 duplicate-`client_order_id` rejection),
/// and unparseable response bodies.
pub async fn place_order(
    client: &AlpacaClient,
    request: &OrderRequest,
) -> Result<OrderResponse, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders",
        client.base_url(),
        client.account_id()
    );

    post_json(client, &url, request).await
}

/// Places a limit order.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses (including the 422 duplicate-`client_order_id` rejection),
/// and unparseable response bodies.
pub async fn place_limit_order(
    client: &AlpacaClient,
    request: &LimitOrderRequest,
) -> Result<OrderResponse, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders",
        client.base_url(),
        client.account_id()
    );

    post_json(client, &url, request).await
}

/// Gets an order by its Alpaca-assigned id.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses, and unparseable response bodies.
pub async fn get_order(
    client: &AlpacaClient,
    order_id: Uuid,
) -> Result<OrderResponse, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders/{order_id}",
        client.base_url(),
        client.account_id()
    );

    get_json(client, &url).await
}

/// Gets an order by its `client_order_id`. Returns `None` if Alpaca has no
/// such order (404) -- i.e. it was never recorded. Used to reconcile a
/// placement that the broker rejected as a duplicate `client_order_id`
/// (it already accepted the original attempt, whose response was lost).
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses other than 404, and unparseable response bodies.
pub async fn get_order_by_client_order_id(
    client: &AlpacaClient,
    client_order_id: &ClientOrderId,
) -> Result<Option<OrderResponse>, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders:by_client_order_id?client_order_id={client_order_id}",
        client.base_url(),
        client.account_id()
    );

    match get_json::<OrderResponse>(client, &url).await {
        Ok(order) => Ok(Some(order)),
        Err(BrokerApiError::Alpaca(AlpacaError::Api {
            status_code: 404, ..
        })) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Cancels an order by id.
///
/// A 404 (the broker does not recognise the order id) is surfaced as
/// [`CancellationOutcome::OrderNotFound`] rather than collapsed into
/// success.
///
/// NOTE: 404 means "order id not found" ONLY. An order that exists but is
/// in a non-cancelable terminal state (filled / expired / already
/// canceled) returns 422, which is propagated as an error here. Per the
/// endpoint reference
/// (<https://docs.alpaca.markets/reference/deleteorderforaccount>):
/// 204 No Content on success, 404 "Resource does not exist", 422 when the
/// order is no longer cancelable.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures and non-2xx
/// API responses other than 404.
pub async fn cancel_order(
    client: &AlpacaClient,
    order_id: Uuid,
) -> Result<CancellationOutcome, BrokerApiError> {
    let url = format!(
        "{}/v1/trading/accounts/{}/orders/{order_id}",
        client.base_url(),
        client.account_id()
    );

    match delete(client, &url).await {
        Ok(()) => Ok(CancellationOutcome::Requested),
        Err(BrokerApiError::Alpaca(AlpacaError::Api {
            status_code: 404, ..
        })) => Ok(CancellationOutcome::OrderNotFound),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::uuid;

    use super::super::{TEST_ACCOUNT_ID, test_client};
    use super::*;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap_or_else(|error| panic!("invalid test symbol: {error}"))
    }

    fn shares(value: &str) -> FractionalShares {
        value.parse().unwrap()
    }

    fn usd(value: &str) -> Usd {
        value.parse().unwrap()
    }

    #[tokio::test]
    async fn place_order_posts_market_order_and_parses_response() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders"))
                .json_body(json!({
                    "symbol": "AAPL",
                    "qty": "100",
                    "side": "buy",
                    "type": "market",
                    "time_in_force": "day",
                    "extended_hours": false,
                    "client_order_id": "33333333-3333-4333-8333-333333333333"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "AAPL",
                    "qty": "100",
                    "side": "buy",
                    "status": "new",
                    "filled_avg_price": null
                }));
        });

        let client = test_client(server.base_url());
        let request = OrderRequest {
            symbol: symbol("AAPL"),
            quantity: shares("100"),
            side: OrderSide::Buy,
            order_type: "market",
            time_in_force: TimeInForce::Day,
            extended_hours: false,
            client_order_id: ClientOrderId("33333333-3333-4333-8333-333333333333".to_string()),
        };

        let response = place_order(&client, &request).await.unwrap();

        mock.assert();
        assert_eq!(response.id, uuid!("904837e3-3b76-47ec-b432-046db621571b"));
        assert_eq!(response.symbol, symbol("AAPL"));
        assert_eq!(response.quantity, shares("100"));
        assert_eq!(response.side, OrderSide::Buy);
        assert_eq!(response.status, OrderStatus::New);
        assert_eq!(response.filled_average_price, None);
    }

    #[tokio::test]
    async fn place_order_propagates_duplicate_client_order_id_422() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders"));
            then.status(422)
                .header("content-type", "application/json")
                .json_body(json!({"message": "client_order_id must be unique"}));
        });

        let client = test_client(server.base_url());
        let request = OrderRequest {
            symbol: symbol("AAPL"),
            quantity: shares("10"),
            side: OrderSide::Buy,
            order_type: "market",
            time_in_force: TimeInForce::Day,
            extended_hours: false,
            client_order_id: ClientOrderId("66666666-6666-4666-8666-666666666666".to_string()),
        };

        let error = place_order(&client, &request).await.unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, body }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 422);
        assert!(body.contains("client_order_id must be unique"));
    }

    #[tokio::test]
    async fn place_limit_order_posts_extended_hours_limit_order() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders"))
                .json_body(json!({
                    "symbol": "AAPL",
                    "qty": "10",
                    "side": "sell",
                    "type": "limit",
                    "limit_price": "195.25",
                    "time_in_force": "day",
                    "extended_hours": true,
                    "client_order_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
                }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "AAPL",
                    "qty": "10",
                    "side": "sell",
                    "status": "new",
                    "filled_avg_price": null,
                    "type": "limit",
                    "limit_price": "195.25",
                    "extended_hours": true
                }));
        });

        let client = test_client(server.base_url());
        let request = LimitOrderRequest {
            symbol: symbol("AAPL"),
            quantity: shares("10"),
            side: OrderSide::Sell,
            order_type: "limit",
            limit_price: usd("195.25"),
            time_in_force: TimeInForce::Day,
            extended_hours: true,
            client_order_id: ClientOrderId("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()),
        };

        let response = place_limit_order(&client, &request).await.unwrap();

        mock.assert();
        assert_eq!(response.extended_hours, Some(true));
        assert_eq!(response.limit_price, Some(usd("195.25")));
    }

    #[tokio::test]
    async fn get_order_parses_filled_order_with_timestamps() {
        let server = MockServer::start();
        let order_id = uuid!("11111111-1111-4111-8111-111111111111");

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": order_id,
                    "symbol": "AAPL",
                    "qty": "100",
                    "filled_qty": "100",
                    "side": "buy",
                    "status": "filled",
                    "filled_avg_price": "150.25",
                    "filled_at": "2025-01-06T15:30:00Z",
                    "updated_at": "2025-01-06T15:30:01Z"
                }));
        });

        let client = test_client(server.base_url());
        let response = get_order(&client, order_id).await.unwrap();

        mock.assert();
        assert_eq!(response.status, OrderStatus::Filled);
        assert_eq!(response.filled_quantity, Some(shares("100")));
        assert_eq!(response.filled_average_price, Some(usd("150.25")));
        assert_eq!(
            response.filled_at.unwrap().to_rfc3339(),
            "2025-01-06T15:30:00+00:00"
        );
        assert_eq!(response.canceled_at, None);
    }

    #[tokio::test]
    async fn get_order_accepts_numeric_quantities() {
        let server = MockServer::start();
        let order_id = uuid!("22222222-2222-4222-8222-222222222222");

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
            ));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": order_id,
                    "symbol": "AAPL",
                    "qty": 10.5,
                    "filled_qty": 10.5,
                    "side": "buy",
                    "status": "partially_filled",
                    "filled_avg_price": 150.25
                }));
        });

        let client = test_client(server.base_url());
        let response = get_order(&client, order_id).await.unwrap();

        mock.assert();
        assert_eq!(response.quantity, shares("10.5"));
        assert_eq!(response.filled_quantity, Some(shares("10.5")));
        assert_eq!(response.status, OrderStatus::PartiallyFilled);
    }

    #[tokio::test]
    async fn get_order_by_client_order_id_returns_order_when_found() {
        let server = MockServer::start();
        let client_order_id = ClientOrderId("66666666-6666-4666-8666-666666666666".to_string());

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!(
                    "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders:by_client_order_id"
                ))
                .query_param("client_order_id", client_order_id.0.as_str());
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "symbol": "AAPL",
                    "qty": "7",
                    "side": "buy",
                    "status": "new",
                    "filled_avg_price": null
                }));
        });

        let client = test_client(server.base_url());
        let response = get_order_by_client_order_id(&client, &client_order_id)
            .await
            .unwrap()
            .unwrap();

        mock.assert();
        assert_eq!(response.quantity, shares("7"));
        assert_eq!(response.extended_hours, None);
        assert_eq!(response.limit_price, None);
    }

    #[tokio::test]
    async fn get_order_by_client_order_id_maps_404_to_none() {
        let server = MockServer::start();
        let client_order_id = ClientOrderId("77777777-7777-4777-8777-777777777777".to_string());

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders:by_client_order_id"
            ));
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({"code": 40_410_000_u64, "message": "order not found"}));
        });

        let client = test_client(server.base_url());
        let response = get_order_by_client_order_id(&client, &client_order_id)
            .await
            .unwrap();

        mock.assert();
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn cancel_order_succeeds_on_2xx() {
        let server = MockServer::start();
        let order_id = uuid!("11111111-1111-1111-1111-111111111111");

        let mock = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
            ));
            then.status(204);
        });

        let client = test_client(server.base_url());
        let outcome = cancel_order(&client, order_id).await.unwrap();

        mock.assert();
        assert_eq!(outcome, CancellationOutcome::Requested);
    }

    #[tokio::test]
    async fn cancel_order_maps_404_to_order_not_found() {
        // A 404 means the broker no longer recognises the id. It must surface
        // as a distinct outcome -- not success (the order would wait forever
        // for a cancellation confirmation) and not a retryable error (the
        // DELETE can never succeed).
        let server = MockServer::start();
        let order_id = uuid!("22222222-2222-2222-2222-222222222222");

        let mock = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
            ));
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({ "code": 40_410_000, "message": "order not found" }));
        });

        let client = test_client(server.base_url());
        let outcome = cancel_order(&client, order_id).await.unwrap();

        mock.assert();
        assert_eq!(outcome, CancellationOutcome::OrderNotFound);
    }

    #[tokio::test]
    async fn cancel_order_propagates_non_404_error() {
        // A 5xx (or 422 non-cancelable) must NOT be swallowed -- only 404 is
        // idempotent. The caller's pre-cancel reconcile handles terminal
        // states.
        let server = MockServer::start();
        let order_id = uuid!("33333333-3333-3333-3333-333333333333");

        let mock = server.mock(|when, then| {
            when.method(DELETE).path(format!(
                "/v1/trading/accounts/{TEST_ACCOUNT_ID}/orders/{order_id}"
            ));
            then.status(500)
                .header("content-type", "application/json")
                .json_body(json!({ "message": "internal error" }));
        });

        let client = test_client(server.base_url());
        let error = cancel_order(&client, order_id).await.unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, .. }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 500);
    }

    #[test]
    fn time_in_force_serializes_to_wire_strings() {
        assert_eq!(serde_json::to_string(&TimeInForce::Day).unwrap(), "\"day\"");
        assert_eq!(
            serde_json::to_string(&TimeInForce::MarketOnClose).unwrap(),
            "\"cls\""
        );
    }

    #[test]
    fn time_in_force_parses_config_spellings() {
        assert_eq!("day".parse::<TimeInForce>().unwrap(), TimeInForce::Day);
        assert_eq!(
            "market-on-close".parse::<TimeInForce>().unwrap(),
            TimeInForce::MarketOnClose
        );
        assert_eq!(
            "market_on_close".parse::<TimeInForce>().unwrap(),
            TimeInForce::MarketOnClose
        );
        assert_eq!(
            "cls".parse::<TimeInForce>().unwrap(),
            TimeInForce::MarketOnClose
        );

        let error = "fortnight".parse::<TimeInForce>().unwrap_err();
        assert!(error.to_string().contains("fortnight"));
    }
}
