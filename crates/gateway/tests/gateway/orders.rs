//! Order placement, lookup, recovery and cancel, and USDC/USD conversions.

use axum::http::StatusCode;
use httpmock::Mock;
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AuditPhase, ErrorCode, Operation, Outcome, Tier};

use crate::common::{ACCOUNT_ID, Harness};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";
const CLIENT_UUID: &str = "66666666-6666-4666-8666-666666666666";

fn orders_path() -> String {
    format!("/v1/trading/accounts/{ACCOUNT_ID}/orders")
}

fn serve_tradable_asset(harness: &Harness) {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": true
        }));
    });
}

fn alpaca_order(client_order_id: &str) -> Value {
    json!({
        "id": ORDER_ID,
        "client_order_id": client_order_id,
        "symbol": "AAPL",
        "qty": "7",
        "side": "buy",
        "status": "new",
        "created_at": "2026-09-17T10:15:29Z",
        "filled_qty": "0",
        "filled_avg_price": null
    })
}

fn market_order(client_order_id: &str) -> Value {
    json!({
        "symbol": "AAPL",
        "shares": "7",
        "direction": "buy",
        "clientOrderId": client_order_id
    })
}

fn accept_orders<'a>(harness: &'a Harness, client_order_id: &str) -> Mock<'a> {
    let body = alpaca_order(client_order_id);
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(200).json_body(body);
    })
}

#[tokio::test]
async fn a_bot_market_order_is_placed_and_audited_as_applied() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    let place = accept_orders(&harness, CLIENT_UUID);

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["orderId"], ORDER_ID);
    assert_eq!(body["direction"], "buy");
    assert_eq!(body["extendedHours"], false);
    place.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.operation, Operation::OrdersPlaceMarket);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert_eq!(event.key.as_deref(), Some(CLIENT_UUID));
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some(ORDER_ID));
    assert_eq!(
        event.summary.get("symbol").map(String::as_str),
        Some("AAPL")
    );
    assert_eq!(event.summary.get("side").map(String::as_str), Some("BUY"));
}

#[tokio::test]
async fn replaying_a_bot_placement_adopts_the_order_alpaca_already_holds() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    let mut first = accept_orders(&harness, CLIENT_UUID);

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    first.assert_calls(1);
    first.delete();

    // Alpaca refuses a second order under a key it already holds.
    let duplicate = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(422).json_body(json!({
            "code": 40_010_001,
            "message": "client_order_id must be unique"
        }));
    });
    let lookup = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!(
                "/v1/trading/accounts/{ACCOUNT_ID}/orders:by_client_order_id"
            ))
            .query_param("client_order_id", CLIENT_UUID);
        then.status(200).json_body(alpaca_order(CLIENT_UUID));
    });

    let (status, replay) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["orderId"], body["orderId"]);
    duplicate.assert_calls(1);
    lookup.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].outcome, Some(Outcome::Applied));
    assert_eq!(events[1].alpaca_object_id.as_deref(), Some(ORDER_ID));
}

#[tokio::test]
async fn a_key_in_the_other_tier_form_is_refused_before_anything_is_sent() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    let place = accept_orders(&harness, CLIENT_UUID);

    let mut human = market_order(CLIENT_UUID);
    human["reason"] = json!("manual hedge");
    let (status, body) = harness
        .call(Tier::Write, "POST", "/orders/market", Some(human))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["outcome"], "not_applied");

    let cli_key = format!("cli-{CLIENT_UUID}");
    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(&cli_key)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["outcome"], "not_applied");

    place.assert_calls(0);
    let events = harness.audit_events();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event.outcome, Some(Outcome::NotApplied));
        assert_eq!(event.code, Some(ErrorCode::InvalidRequest));
    }
    assert_eq!(events[1].key.as_deref(), Some(cli_key.as_str()));
}

#[tokio::test]
async fn a_human_order_carries_the_cli_key_and_a_reason() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    let cli_key = format!("cli-{CLIENT_UUID}");
    let place = accept_orders(&harness, &cli_key);

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/orders/market",
            Some(market_order(&cli_key)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    place.assert_calls(0);

    let mut reasoned = market_order(&cli_key);
    reasoned["reason"] = json!("manual hedge");
    let (status, body) = harness
        .call(Tier::Write, "POST", "/orders/market", Some(reasoned))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    place.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events[1].reason.as_deref(), Some("manual hedge"));
    assert_eq!(events[1].outcome, Some(Outcome::Applied));
}

#[tokio::test]
async fn an_alpaca_failure_on_the_order_post_leaves_the_outcome_unknown() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(500)
            .json_body(json!({ "message": "internal error" }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["retryable"], false);
    assert_eq!(body["retryableWithSameKey"], true);
    assert_eq!(harness.audit_events()[0].outcome, Some(Outcome::Unknown));
}

#[tokio::test]
async fn a_conversion_refused_for_insufficient_balance_is_not_applied() {
    let harness = Harness::start().await;
    let place = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(403).json_body(json!({
            "code": 40_310_000,
            "message": "insufficient balance for USD"
        }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/conversions",
            Some(json!({
                "clientOrderId": CLIENT_UUID,
                "conversion": { "direction": "buy_with_usd", "notional": "1000" }
            })),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "insufficient_balance");
    assert_eq!(body["outcome"], "not_applied");
    place.assert_calls(1);
    assert_eq!(harness.audit_events()[0].outcome, Some(Outcome::NotApplied));
}

#[tokio::test]
async fn an_accepted_conversion_answers_the_alpaca_order() {
    let harness = Harness::start().await;
    let place = harness.alpaca.mock(|when, then| {
        when.method(POST)
            .path(orders_path())
            .json_body_includes(r#"{"symbol":"USDCUSD","side":"sell","qty":"25.5"}"#);
        then.status(200).json_body(json!({
            "id": ORDER_ID,
            "symbol": "USDCUSD",
            "qty": "25.5",
            "status": "new",
            "filled_qty": "0",
            "created_at": "2026-09-17T10:15:29Z"
        }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/conversions",
            Some(json!({
                "clientOrderId": CLIENT_UUID,
                "conversion": { "direction": "sell_usdc", "quantity": "25.5" }
            })),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], ORDER_ID);
    assert_eq!(body["status"], "new");
    assert_eq!(body["quantity"], "25.5");
    place.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.alpaca_object_id.as_deref(), Some(ORDER_ID));
    assert_eq!(event.summary.get("asset").map(String::as_str), Some("USDC"));
}

#[tokio::test]
async fn cancelling_an_order_alpaca_does_not_know_answers_order_not_found() {
    let harness = Harness::start().await;
    let cancel = harness.alpaca.mock(|when, then| {
        when.method(DELETE)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(404)
            .json_body(json!({ "message": "order not found" }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            &format!("/orders/{ORDER_ID}/cancel"),
            Some(json!({})),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "order_not_found");
    cancel.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.key.as_deref(), Some(ORDER_ID));
    assert_eq!(event.outcome, Some(Outcome::Applied));
}

#[tokio::test]
async fn a_filled_order_reads_back_with_its_fill() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(200).json_body(json!({
            "id": ORDER_ID,
            "symbol": "AAPL",
            "qty": "7",
            "side": "buy",
            "status": "filled",
            "filled_qty": "7",
            "filled_avg_price": "187.25",
            "created_at": "2026-09-17T10:15:29Z",
            "updated_at": "2026-09-17T10:15:30Z",
            "filled_at": "2026-09-17T10:15:30Z"
        }));
    });

    let (status, body) = harness
        .call(Tier::Read, "GET", &format!("/orders/{ORDER_ID}"), None)
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "filled");
    assert_eq!(body["orderId"], ORDER_ID);
    assert_eq!(body["sharesFilled"], "7");
    assert_eq!(body["price"], "187.25");
}

#[tokio::test]
async fn finding_an_unknown_client_order_id_answers_no_order() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!(
                "/v1/trading/accounts/{ACCOUNT_ID}/orders:by_client_order_id"
            ))
            .query_param("client_order_id", CLIENT_UUID);
        then.status(404)
            .json_body(json!({ "message": "order not found" }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/orders/by-client-order-id/{CLIENT_UUID}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["order"], Value::Null);
}

#[tokio::test]
async fn readers_cannot_place_orders() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(
            Tier::Read,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "unknown_operation");
}
