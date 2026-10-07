//! Order placement, lookup, recovery and cancel, the market reads, and
//! USDC/USD conversions.

use axum::http::StatusCode;
use chrono::{Days, Utc};
use httpmock::Mock;
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AuditPhase, ErrorCode, Operation, Outcome, Tier};

use crate::common::{ACCOUNT_ID, Harness, assert_read_audited, authorized};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";
const CLIENT_UUID: &str = "66666666-6666-4666-8666-666666666666";

fn orders_path() -> String {
    format!("/v1/trading/accounts/{ACCOUNT_ID}/orders")
}

fn by_client_order_id_path() -> String {
    format!("/v1/trading/accounts/{ACCOUNT_ID}/orders:by_client_order_id")
}

fn serve_asset(harness: &Harness, fractionable: bool) {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": fractionable,
            "attributes": ["fractional_eh_enabled"]
        }));
    });
}

fn serve_tradable_asset(harness: &Harness) {
    serve_asset(harness, true);
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

/// The Alpaca order [`market_order`] must turn into.
fn alpaca_market_order(client_order_id: &str) -> Value {
    json!({
        "symbol": "AAPL",
        "qty": "7",
        "side": "buy",
        "type": "market",
        "time_in_force": "day",
        "extended_hours": false,
        "client_order_id": client_order_id
    })
}

/// Accepts only the exact Alpaca order [`market_order`] must turn into.
fn accept_orders<'a>(harness: &'a Harness, client_order_id: &str) -> Mock<'a> {
    let expected = alpaca_market_order(client_order_id);
    let body = alpaca_order(client_order_id);
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path()).json_body(expected);
        then.status(200).json_body(body);
    })
}

/// Matches every Alpaca API request the mock receives after it is created,
/// and none of the signing key fetches it also serves.
fn any_alpaca_request(harness: &Harness) -> Mock<'_> {
    harness.alpaca.mock(|when, then| {
        when.path_prefix("/v");
        then.status(200).json_body(json!({}));
    })
}

/// Each placement route sends exactly its Alpaca order and is audited as
/// applied: a market order as given, a limit order truncated to the asset's
/// precision, and an exact limit order with the quantity given on an asset
/// without fractions.
#[tokio::test]
async fn each_placement_route_sends_its_alpaca_order_and_is_audited_as_applied() {
    let cli_key = format!("cli-{CLIENT_UUID}");
    let limit = json!({
        "symbol": "AAPL",
        "shares": "2.1234567891",
        "direction": "sell",
        "limitPrice": "187.25",
        "extendedHours": true,
        "clientOrderId": CLIENT_UUID
    });
    let alpaca_limit = json!({
        "symbol": "AAPL",
        "qty": "2.123456789",
        "side": "sell",
        "type": "limit",
        "limit_price": "187.25",
        "time_in_force": "day",
        "extended_hours": true,
        "client_order_id": CLIENT_UUID
    });
    let exact_limit = json!({
        "symbol": "AAPL",
        "shares": "3.5",
        "direction": "buy",
        "limitPrice": "150.5",
        "extendedHours": false,
        "clientOrderId": cli_key,
        "reason": "close the odd lot"
    });
    let alpaca_exact_limit = json!({
        "symbol": "AAPL",
        "qty": "3.5",
        "side": "buy",
        "type": "limit",
        "limit_price": "150.5",
        "time_in_force": "day",
        "extended_hours": false,
        "client_order_id": cli_key
    });

    for (tier, route, operation, fractionable, request, sent) in [
        (
            Tier::Bot,
            "/orders/market",
            Operation::OrdersPlaceMarket,
            true,
            market_order(CLIENT_UUID),
            alpaca_market_order(CLIENT_UUID),
        ),
        (
            Tier::Bot,
            "/orders/limit",
            Operation::OrdersPlaceLimit,
            true,
            limit,
            alpaca_limit,
        ),
        // Not fractionable: `orders.place_limit` would send 3 shares.
        (
            Tier::Write,
            "/orders/exact-limit",
            Operation::OrdersPlaceExactLimit,
            false,
            exact_limit,
            alpaca_exact_limit,
        ),
    ] {
        let harness = Harness::start().await;
        serve_asset(&harness, fractionable);
        let key = sent["client_order_id"].as_str().unwrap().to_string();
        let answer = alpaca_order(&key);
        let place = harness.alpaca.mock(|when, then| {
            when.method(POST)
                .path(orders_path())
                .json_body(sent.clone());
            then.status(200).json_body(answer);
        });

        let (status, body) = harness
            .call(tier, "POST", route, Some(request.clone()))
            .await;

        assert_eq!(status, StatusCode::OK, "{route}: {body}");
        assert_eq!(body["orderId"], ORDER_ID, "{route}");
        assert_eq!(body["shares"], sent["qty"], "{route}");
        assert_eq!(body["extendedHours"], sent["extended_hours"], "{route}");
        place.assert_calls(1);
        let events = harness.audit_events();
        assert_eq!(events.len(), 1, "{route}: {events:?}");
        let event = &events[0];
        assert_eq!(event.operation, operation);
        assert_eq!(event.phase, AuditPhase::Answered);
        assert_eq!(event.key.as_deref(), Some(key.as_str()));
        assert_eq!(event.reason.as_deref(), request["reason"].as_str());
        assert_eq!(event.outcome, Some(Outcome::Applied));
        assert_eq!(event.alpaca_object_id.as_deref(), Some(ORDER_ID));
        assert_eq!(event.summary["symbol"], "AAPL");
    }
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
            .path(by_client_order_id_path())
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
async fn a_failed_adoption_lookup_after_a_duplicate_key_leaves_the_outcome_unknown() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    // The duplicate key 422 proves Alpaca holds an order under the key; the
    // rate limited read back cannot make that order not exist.
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(422).json_body(json!({
            "code": 40_010_001,
            "message": "client_order_id must be unique"
        }));
    });
    let lookup = harness.alpaca.mock(|when, then| {
        when.method(GET).path(by_client_order_id_path());
        then.status(429)
            .header("retry-after", "3")
            .json_body(json!({ "message": "too many requests" }));
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
    assert_eq!(body["retryableWithSameKey"], true);
    lookup.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.code, Some(ErrorCode::OutcomeUnknown));
}

#[tokio::test]
async fn a_failed_asset_read_answers_not_applied_without_posting() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(503).body("unavailable");
    });
    let place = accept_orders(&harness, CLIENT_UUID);

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_transient");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["retryable"], true);
    place.assert_calls(0);
    assert_eq!(harness.audit_events()[0].outcome, Some(Outcome::NotApplied));
}

#[tokio::test]
async fn a_definite_rejection_of_the_order_post_is_not_applied() {
    let harness = Harness::start().await;
    serve_tradable_asset(&harness);
    let place = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(403).json_body(json!({
            "code": 40_310_000,
            "message": "insufficient buying power"
        }));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(market_order(CLIENT_UUID)),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["alpacaStatus"], 403);
    place.assert_calls(1);
    assert_eq!(harness.audit_events()[0].outcome, Some(Outcome::NotApplied));
}

/// The bot keys and looks up with bare UUIDs, a human writer places with
/// `cli-` keys and a reason; every request in the other form is refused
/// before anything is sent.
#[tokio::test]
async fn a_key_in_the_other_tier_form_is_refused_before_anything_is_sent() {
    let harness = Harness::start().await;
    let alpaca = any_alpaca_request(&harness);
    let cli_key = format!("cli-{CLIENT_UUID}");
    let mut human_with_bot_key = market_order(CLIENT_UUID);
    human_with_bot_key["reason"] = json!("manual hedge");

    let refusals = [
        (
            Tier::Write,
            "POST",
            "/orders/market".to_string(),
            Some(human_with_bot_key),
        ),
        (
            Tier::Bot,
            "POST",
            "/orders/market".to_string(),
            Some(market_order(&cli_key)),
        ),
        // A human order without a reason.
        (
            Tier::Write,
            "POST",
            "/orders/market".to_string(),
            Some(market_order(&cli_key)),
        ),
        (
            Tier::Bot,
            "GET",
            format!("/orders/by-client-order-id/{cli_key}"),
            None,
        ),
        (
            Tier::Bot,
            "POST",
            "/orders/recover".to_string(),
            Some(market_order(&cli_key)),
        ),
        (
            Tier::Bot,
            "GET",
            format!("/conversions/by-client-order-id/{cli_key}"),
            None,
        ),
    ];
    for (tier, method, path, body) in refusals.clone() {
        let (status, answer) = harness.call(tier, method, &path, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{tier:?} {path}: {answer}");
        assert_eq!(answer["code"], "invalid_request", "{tier:?} {path}");
    }

    alpaca.assert_calls(0);
    let events = harness.audit_events();
    assert_eq!(events.len(), refusals.len(), "{events:?}");
    for event in &events {
        assert_eq!(event.code, Some(ErrorCode::InvalidRequest), "{event:?}");
        let outcome = event.operation.mutates().then_some(Outcome::NotApplied);
        assert_eq!(event.outcome, outcome, "{event:?}");
    }
}

#[tokio::test]
async fn a_human_finds_an_order_under_its_cli_key() {
    let harness = Harness::start().await;
    let cli_key = format!("cli-{CLIENT_UUID}");
    let lookup = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(by_client_order_id_path())
            .query_param("client_order_id", cli_key.as_str());
        then.status(200).json_body(alpaca_order(&cli_key));
    });

    let (status, body) = harness
        .call(
            Tier::Read,
            "GET",
            &format!("/orders/by-client-order-id/{cli_key}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["order"]["orderId"], ORDER_ID);
    lookup.assert_calls(1);
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
    assert_eq!(body["alpacaStatus"], 500);
    let event = &harness.audit_events()[0];
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.alpaca_status, Some(500));
}

#[tokio::test]
async fn a_symbol_that_walks_out_of_its_path_segment_is_refused_before_alpaca_is_called() {
    let harness = Harness::start().await;
    let alpaca = any_alpaca_request(&harness);

    let (status, body) = harness
        .send(authorized(
            Tier::Bot,
            "GET",
            "/bot/v1/assets/..%2Ftrading%2Faccounts%2Fx%2Faccount",
            None,
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    alpaca.assert_calls(0);
}

#[tokio::test]
async fn a_market_data_rejection_carries_the_status_alpaca_answered() {
    let harness = Harness::start().await;
    let quote = harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v2/stocks/AAPL/quotes/latest");
        then.status(404)
            .json_body(json!({ "message": "symbol not found" }));
    });

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/market/stocks/AAPL/latest-quote", None)
        .await;

    // A definite 4xx, as the direct path's permanent `LatestQuote` error.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "alpaca_api");
    assert_eq!(body["alpacaStatus"], 404);
    assert_eq!(body["retryable"], false);
    quote.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.key.as_deref(), Some("AAPL"));
    assert_eq!(event.alpaca_status, Some(404));
}

const CALENDAR_REQUEST_ID: &str = "req-calendar";
const QUOTE_REQUEST_ID: &str = "req-quote";

/// A day without trading reads as a closed session on both session routes,
/// and a closed session status carries no session times. The read asks for
/// today and tomorrow in New York; today there is the UTC date or the day
/// before it, so both windows are served and each read sends exactly one.
#[tokio::test]
async fn a_day_without_trading_reads_as_a_closed_session_and_each_read_is_audited() {
    for (path, operation, expected) in [
        (
            "/market/session",
            Operation::MarketSession,
            json!({ "session": "Closed" }),
        ),
        (
            "/market/session-status",
            Operation::MarketSessionStatus,
            json!({
                "session": "Closed",
                "sessionOpensAt": null,
                "regularSessionClosesAt": null,
                "extendedSessionClosesAt": null,
                "postCloseGap": "unknown"
            }),
        ),
    ] {
        let harness = Harness::start().await;
        let utc_today = Utc::now().date_naive();
        let calendars: Vec<Mock<'_>> = [utc_today - Days::new(1), utc_today]
            .into_iter()
            .map(|start| {
                let end = start + Days::new(1);
                harness.alpaca.mock(|when, then| {
                    when.method(GET)
                        .path("/v1/calendar")
                        .query_param("start", start.to_string())
                        .query_param("end", end.to_string());
                    then.status(200)
                        .header("x-request-id", CALENDAR_REQUEST_ID)
                        .json_body(json!([]));
                })
            })
            .collect();

        let (status, body) = harness.call(Tier::Bot, "GET", path, None).await;

        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body, expected, "{path}");
        assert_eq!(
            calendars.iter().map(Mock::calls).sum::<usize>(),
            1,
            "{path}"
        );
        assert_read_audited(&harness, operation, Tier::Bot, None, &[CALENDAR_REQUEST_ID]);
    }
}

#[tokio::test]
async fn a_human_reads_the_overnight_quote_with_its_broker_timestamp() {
    let harness = Harness::start().await;
    let quote = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path("/v2/stocks/AAPL/quotes/latest")
            .query_param("feed", "overnight");
        then.status(200)
            .header("x-request-id", QUOTE_REQUEST_ID)
            .json_body(json!({
                "symbol": "AAPL",
                "quote": {
                    "bp": 187.1,
                    "bs": 3,
                    "ap": 187.35,
                    "as": 2,
                    "t": "2026-10-06T01:15:30.123456789Z"
                }
            }));
    });

    let (status, body) = harness
        .call(
            Tier::Read,
            "GET",
            "/market/stocks/AAPL/latest-overnight-quote",
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "bid": "187.1",
            "ask": "187.35",
            "at": "2026-10-06T01:15:30.123456789Z"
        })
    );
    quote.assert_calls(1);
    assert_read_audited(
        &harness,
        Operation::MarketLatestOvernightQuote,
        Tier::Read,
        Some("AAPL"),
        &[QUOTE_REQUEST_ID],
    );
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
    assert_eq!(body["qty"], "25.5");
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
