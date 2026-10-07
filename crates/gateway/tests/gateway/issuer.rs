//! The `s01` profile: issuer calls, the corporate action stream relay, and
//! which routes each profile serves.

use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use httpmock::When;
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AuditPhase, Operation, Outcome, Tier};
use tower::ServiceExt as _;

use crate::common::{ACCOUNT_ID, Harness, STREAM_PATH, answer, authorized, until_called};

const TOKENIZATION_REQUEST_ID: &str = "tok_req_1";
const CLIENT_ID: &str = "3f7c2a1e-9b4d-4e8a-8c1f-5d6e7f8a9b0c";
const ISSUER_WALLET: &str = "0x3333333333333333333333333333333333333333";
const TX_HASH: &str = "0xabababababababababababababababababababababababababababababababab";
const EVENT_ID: &str = "01J9Z8X7W6V5T4S3R2Q1P0N9M8";

/// Frames as Alpaca sends them, a comment line included.
const FRAMES: &str =
    "id: 01J9Z8X7W6V5T4S3R2Q1P0N9M8\nevent: insert\ndata: {\"any\":1}\n\n: ping\n\n";

fn callback_path(kind: &str) -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/tokenization/callback/{kind}")
}

/// Matches only a request carrying the harness's Basic broker credential as
/// the stream client sends it.
fn credentialed(when: When) -> When {
    when.header("APCA-API-KEY-ID", "key")
        .header("APCA-API-SECRET-KEY", "secret")
}

fn mint_callback_body() -> Value {
    json!({
        "tokenizationRequestId": TOKENIZATION_REQUEST_ID,
        "clientId": CLIENT_ID,
        "walletAddress": ISSUER_WALLET,
        "txHash": TX_HASH,
        "network": "base",
    })
}

fn redeem_body(network: &str) -> Value {
    json!({
        "issuerRequestId": TX_HASH,
        "underlyingSymbol": "AAPL",
        "tokenSymbol": "tAAPL",
        "clientId": CLIENT_ID,
        "quantity": "2.50",
        "network": network,
        "walletAddress": ISSUER_WALLET,
        "txHash": TX_HASH,
    })
}

#[tokio::test]
async fn the_bot_mint_callback_is_applied_and_audited_as_s01() {
    let harness = Harness::start_s01().await;
    let callback = harness.alpaca.mock(|when, then| {
        when.method(POST)
            .path(callback_path("mint"))
            .json_body(json!({
                "tokenization_request_id": TOKENIZATION_REQUEST_ID,
                "client_id": CLIENT_ID,
                "wallet_address": ISSUER_WALLET,
                "tx_hash": TX_HASH,
                "network": "base",
            }));
        then.status(200).header("x-request-id", "req-mint");
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/issuer/mint-callbacks",
            Some(mint_callback_body()),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    callback.assert_calls(1);
    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event.deployment, "s01-alpaca");
    assert_eq!(event.operation, Operation::IssuerMintCallback);
    assert_eq!(event.tier, Tier::Bot);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.key.as_deref(), Some(TOKENIZATION_REQUEST_ID));
    assert_eq!(
        event.alpaca_object_id.as_deref(),
        Some(TOKENIZATION_REQUEST_ID)
    );
    assert_eq!(event.alpaca_status, Some(200));
    assert_eq!(event.alpaca_request_ids, ["req-mint"]);
}

/// The library resends a redeem after a server error, which may have been
/// applied, so a rejection of the resend proves nothing; a rejection of the
/// only attempt does. Either answer carries the status of the last answer
/// Alpaca gave, a refused credential's too.
#[tokio::test]
async fn a_redeem_rejected_after_a_server_error_is_outcome_unknown_and_alone_not_applied() {
    for refusal in [400, 403] {
        let harness = Harness::start_s01().await;
        let failed = harness.alpaca.mock(|when, then| {
            when.method(POST).path(callback_path("redeem"));
            then.status(500).body("internal error");
        });
        let request = authorized(
            Tier::Bot,
            "POST",
            "/bot/v1/issuer/redemptions",
            Some(redeem_body("base")),
        );
        let mut call = tokio::spawn(answer(harness.app.clone(), request));
        until_called(&failed, &mut call).await;
        failed.delete_async().await;
        let rejected = harness.alpaca.mock(|when, then| {
            when.method(POST).path(callback_path("redeem"));
            then.status(refusal).body("redemption refused");
        });

        let (status, body) = call.await.unwrap();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{refusal}: {body}");
        assert_eq!(body["code"], "outcome_unknown", "{refusal}");
        assert_eq!(body["outcome"], "unknown", "{refusal}");
        assert_eq!(body["alpacaStatus"], refusal);
        assert_eq!(body["retryableWithSameKey"], false, "{refusal}");
        rejected.assert_calls(1);

        let (status, body) = harness
            .call(
                Tier::Bot,
                "POST",
                "/issuer/redemptions",
                Some(redeem_body("base")),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{refusal}: {body}"
        );
        assert_eq!(body["code"], "rejected", "{refusal}");
        assert_eq!(body["reason"], "alpaca_api", "{refusal}");
        assert_eq!(body["outcome"], "not_applied", "{refusal}");
        assert_eq!(body["alpacaStatus"], refusal);
        rejected.assert_calls(2);
    }
}

/// Each refused issuer body answers `400 invalid_request` and reaches no
/// Alpaca endpoint.
#[tokio::test]
async fn a_bad_issuer_key_symbol_or_quantity_is_refused_before_alpaca() {
    let harness = Harness::start_s01().await;
    let callbacks = harness.alpaca.mock(|when, then| {
        when.method(POST).path_includes("/tokenization/callback/");
        then.status(200);
    });
    let reads = harness.alpaca.mock(|when, then| {
        when.method(GET).path_includes("/tokenization/requests/");
        then.status(200);
    });
    let overlong_id = "x".repeat(129);
    let overlong_symbol = "A".repeat(33);
    let mint = |value: &str| {
        let mut body = mint_callback_body();
        body["tokenizationRequestId"] = json!(value);
        ("/issuer/mint-callbacks", body)
    };
    let redeem = |field: &str, value: &str| {
        let mut body = redeem_body("base");
        body[field] = json!(value);
        ("/issuer/redemptions", body)
    };

    for (path, body) in [
        mint(""),
        mint("  "),
        mint(&overlong_id),
        redeem("issuerRequestId", ""),
        redeem("issuerRequestId", " "),
        redeem("issuerRequestId", &overlong_id),
        redeem("underlyingSymbol", "../AAPL"),
        redeem("underlyingSymbol", &overlong_symbol),
        redeem("tokenSymbol", ""),
        redeem("tokenSymbol", "tAAPL?x"),
        redeem("tokenSymbol", &overlong_symbol),
        redeem("quantity", "0"),
        redeem("quantity", "-2.5"),
    ] {
        let (status, reply) = harness
            .call(Tier::Bot, "POST", path, Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {reply}");
        assert_eq!(reply["code"], "invalid_request", "{body}");
    }
    for path in [
        "/issuer/requests/%20".to_string(),
        format!("/issuer/requests/{overlong_id}"),
    ] {
        let (status, reply) = harness.call(Tier::Bot, "GET", &path, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {reply}");
        assert_eq!(reply["code"], "invalid_request", "{path}");
    }
    callbacks.assert_calls(0);
    reads.assert_calls(0);
}

/// The request type names only networks the ITN preflight accepts, so any
/// other network is refused as the body parses.
#[tokio::test]
async fn a_redeem_on_an_unsupported_network_is_not_applied_and_nothing_is_sent() {
    let harness = Harness::start_s01().await;
    let redeem = harness.alpaca.mock(|when, then| {
        when.method(POST).path(callback_path("redeem"));
        then.status(200);
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/issuer/redemptions",
            Some(redeem_body("polygon")),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["outcome"], "not_applied");
    redeem.assert_calls(0);
}

#[tokio::test]
async fn a_request_alpaca_does_not_hold_is_request_not_found() {
    let harness = Harness::start_s01().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(format!(
            "/v1/accounts/{ACCOUNT_ID}/tokenization/requests/tok_req_9"
        ));
        then.status(404).body("not found");
    });

    let (status, body) = harness
        .call(Tier::Read, "GET", "/issuer/requests/tok_req_9", None)
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "request_not_found");
    assert_eq!(body["alpacaStatus"], 404);
}

#[tokio::test]
async fn a_refused_credential_on_a_request_read_keeps_its_alpaca_status() {
    let harness = Harness::start_s01().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(format!(
            "/v1/accounts/{ACCOUNT_ID}/tokenization/requests/tok_req_9"
        ));
        then.status(401).body("unauthorized");
    });

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/issuer/requests/tok_req_9", None)
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "alpaca_api");
    assert_eq!(body["alpacaStatus"], 401);
    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].alpaca_status, Some(401));
}

#[tokio::test]
async fn an_issuer_429_relays_its_retry_after_as_backpressure() {
    let harness = Harness::start_s01().await;
    let callback = harness.alpaca.mock(|when, then| {
        when.method(POST).path(callback_path("mint"));
        then.status(429)
            .header("retry-after", "60")
            .body("rate limit exceeded");
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/issuer/mint-callbacks",
            Some(mint_callback_body()),
        )
        .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["alpacaStatus"], 429);
    assert_eq!(body["retryAfterSecs"], 60);
    callback.assert_calls(1);

    // The client holds the next callback back for the rest of Alpaca's
    // hold, so Alpaca answers nothing and no status is named.
    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/issuer/mint-callbacks",
            Some(mint_callback_body()),
        )
        .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["alpacaStatus"], Value::Null);
    let hold = body["retryAfterSecs"].as_u64().unwrap();
    assert!((31..=60).contains(&hold), "{body}");
    callback.assert_calls(1);
    let events = harness.audit_events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].alpaca_status, None);
    assert!(events[1].alpaca_request_ids.is_empty(), "{events:?}");
}

#[tokio::test]
async fn the_stream_relays_alpaca_bytes_unchanged_from_the_replay_position() {
    let harness = Harness::start_s01().await;
    let resumed = harness.alpaca.mock(|when, then| {
        credentialed(when)
            .method(GET)
            .path(STREAM_PATH)
            .query_param("type", "cash_dividend_corporateaction_event")
            .query_param("region", "us")
            .query_param("since_id", EVENT_ID);
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(FRAMES);
    });
    let window = harness.alpaca.mock(|when, then| {
        credentialed(when)
            .method(GET)
            .path(STREAM_PATH)
            .query_param("since", "2026-10-01T00:00:00Z")
            .query_param("until", "2026-10-02T00:00:00Z")
            .query_param_missing("since_id");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(FRAMES);
    });

    for query in [
        format!("sinceId={EVENT_ID}"),
        "since=2026-10-01T00:00:00Z&until=2026-10-02T00:00:00Z".to_string(),
    ] {
        let request = authorized(
            Tier::Bot,
            "GET",
            &format!("/bot/v1/corporate-actions/stream?{query}"),
            None,
        );
        let response = harness.app.clone().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK, "{query}");
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, FRAMES.as_bytes(), "{query}");
    }
    resumed.assert_calls(1);
    window.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 2, "{events:?}");
    for event in events {
        assert_eq!(event.operation, Operation::CorporateActionsStream);
        assert_eq!(event.phase, AuditPhase::Answered);
        assert_eq!(event.alpaca_status, Some(200));
        assert_eq!(event.code, None);
    }
}

#[tokio::test]
async fn a_stream_429_relays_its_retry_after_as_backpressure() {
    let harness = Harness::start_s01().await;
    let stream = harness.alpaca.mock(|when, then| {
        credentialed(when).method(GET).path(STREAM_PATH);
        then.status(429)
            .header("retry-after", "30")
            .body("rate limit exceeded");
    });

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/corporate-actions/stream", None)
        .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["alpacaStatus"], 429);
    assert_eq!(body["retryAfterSecs"], 30);
    stream.assert_calls(1);
}

#[tokio::test]
async fn a_stream_answer_that_is_not_an_event_stream_keeps_its_alpaca_status() {
    let harness = Harness::start_s01().await;
    harness.alpaca.mock(|when, then| {
        credentialed(when).method(GET).path(STREAM_PATH);
        then.status(200)
            .header("content-type", "application/json")
            .body("{}");
    });

    let (_, body) = harness
        .call(Tier::Bot, "GET", "/corporate-actions/stream", None)
        .await;

    assert_eq!(body["code"], "upstream_transient", "{body}");
    assert_eq!(body["retryable"], false);
    assert_eq!(body["alpacaStatus"], 200);
    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].alpaca_status, Some(200));
}

/// A caller that goes away while the stream connect is pending still leaves
/// the connect's record, written `settled` with its Alpaca traffic.
#[tokio::test]
async fn a_stream_connect_its_caller_abandons_is_still_audited() {
    let harness = Harness::start_s01().await;
    let stream = harness.alpaca.mock(|when, then| {
        credentialed(when).method(GET).path(STREAM_PATH);
        then.status(200)
            .header("content-type", "text/event-stream")
            .header("x-request-id", "req-stream")
            .delay(std::time::Duration::from_millis(300))
            .body(FRAMES);
    });
    let request = authorized(Tier::Bot, "GET", "/bot/v1/corporate-actions/stream", None);
    let mut call = Box::pin(harness.app.clone().oneshot(request));

    let connect_started = async {
        while stream.calls_async().await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        response = &mut call => panic!("answered before the connect: {response:?}"),
        () = connect_started => {}
    }
    // The server drops the request future, as hyper does on a closed
    // connection.
    drop(call);

    tokio::time::timeout(std::time::Duration::from_secs(5), harness.settle())
        .await
        .unwrap();
    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].operation, Operation::CorporateActionsStream);
    assert_eq!(events[0].phase, AuditPhase::Settled);
    assert_eq!(events[0].alpaca_request_ids, ["req-stream"]);
}

#[tokio::test]
async fn humans_cannot_reach_the_issuer_posts_nor_the_s01_bot_orders() {
    let harness = Harness::start_s01().await;
    for (tier, method, path) in [
        (Tier::Read, "POST", "/issuer/mint-callbacks"),
        (Tier::Write, "POST", "/issuer/mint-callbacks"),
        (Tier::Read, "POST", "/issuer/redemptions"),
        (Tier::Write, "POST", "/issuer/redemptions"),
        (
            Tier::Bot,
            "GET",
            "/orders/7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f",
        ),
        (Tier::Bot, "POST", "/orders/market"),
    ] {
        let (status, body) = harness.call(tier, method, path, None).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{tier:?} {path}: {body}");
        assert_eq!(body["code"], "unknown_operation");
    }
}

#[tokio::test]
async fn a_t0_deployment_serves_none_of_the_issuer_routes() {
    let harness = Harness::start().await;
    for tier in Tier::ALL {
        for (method, path) in [
            ("POST", "/issuer/mint-callbacks"),
            ("POST", "/issuer/redemptions"),
            ("GET", "/issuer/requests/tok_req_1"),
            ("GET", "/corporate-actions/stream"),
        ] {
            let (status, body) = harness.call(tier, method, path, None).await;

            assert_eq!(status, StatusCode::NOT_FOUND, "{tier:?} {path}: {body}");
            assert_eq!(body["code"], "unknown_operation");
        }
    }
}
