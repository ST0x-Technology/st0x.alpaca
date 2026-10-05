//! Health, startup dependency checks, identity, the human budget, deadlines
//! and the capability switch.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::json;
use st0x_alpaca_gateway::audit::MemorySink;
use st0x_alpaca_gateway::config::GatewayConfig;
use st0x_alpaca_gateway::state::{AppState, StartupError};
use st0x_alpaca_gateway_api::{AuditPhase, Operation, Tier};

use crate::common::{
    ACCOUNT_ID, BOT_AUDIENCE, BOT_SUBJECT, Harness, READ_AUDIENCE, account_body, bot_token,
    config_text, iap_token,
};

#[tokio::test]
async fn health_and_readiness_answer_without_credentials() {
    let harness = Harness::start().await;

    for path in ["/healthz", "/readyz"] {
        let request = Request::get(path).body(Body::empty()).unwrap();
        let (status, _) = harness.send(request).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn startup_refuses_an_account_with_another_number() {
    let alpaca = MockServer::start_async().await;
    let keys = MockServer::start_async().await;
    alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/account"));
        then.status(200).json_body(account_body("S01-0001"));
    });

    let config = GatewayConfig::parse(&config_text(&alpaca, &keys, "", "")).unwrap();
    let error = AppState::connect(config, Arc::new(MemorySink::default()), "test".into())
        .await
        .err()
        .unwrap();

    assert!(
        matches!(error, StartupError::AccountMismatch { ref reported, .. } if reported.as_deref() == Some("S01-0001")),
        "{error}"
    );
}

#[tokio::test]
async fn startup_fails_closed_when_alpaca_is_unreachable() {
    let alpaca = MockServer::start_async().await;
    let keys = MockServer::start_async().await;
    alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/account"));
        then.status(503).body("unavailable");
    });

    let config = GatewayConfig::parse(&config_text(&alpaca, &keys, "", "")).unwrap();
    let error = AppState::connect(config, Arc::new(MemorySink::default()), "test".into())
        .await
        .err()
        .unwrap();

    assert!(matches!(error, StartupError::Broker(_)), "{error}");
}

#[tokio::test]
async fn bot_reads_funds_and_the_read_is_audited() {
    let harness = Harness::start().await;

    let (status, body) = harness.call(Tier::Bot, "GET", "/account/funds", None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["balanceCents"], 150_025);
    assert_eq!(body["withdrawableCents"], 100_010);

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.operation, Operation::AccountFunds);
    assert_eq!(event.tier, Tier::Bot);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert_eq!(event.account_id, ACCOUNT_ID);
    assert_eq!(event.outcome, None);
    assert_eq!(event.code, None);
}

#[tokio::test]
async fn a_bot_request_without_a_token_is_unauthenticated() {
    let harness = Harness::start().await;
    let request = Request::get("/bot/v1/account/funds")
        .body(Body::empty())
        .unwrap();

    let (status, body) = harness.send(request).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthenticated");
}

#[tokio::test]
async fn a_bot_token_for_another_audience_is_rejected() {
    let harness = Harness::start().await;
    let request = Request::get("/bot/v1/account/funds")
        .header(
            "authorization",
            format!("Bearer {}", bot_token(BOT_SUBJECT, "https://other.run.app")),
        )
        .body(Body::empty())
        .unwrap();

    let (status, _) = harness.send(request).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_service_account_outside_bot_principals_is_forbidden() {
    let harness = Harness::start().await;
    let request = Request::get("/bot/v1/account/funds")
        .header(
            "authorization",
            format!("Bearer {}", bot_token("999", BOT_AUDIENCE)),
        )
        .body(Body::empty())
        .unwrap();

    let (status, body) = harness.send(request).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
}

#[tokio::test]
async fn a_read_tier_assertion_does_not_open_the_write_tier() {
    let harness = Harness::start().await;
    let request = Request::post("/alpaca-write/v1/orders/abc/cancel")
        .header("x-goog-iap-jwt-assertion", iap_token(READ_AUDIENCE))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let (status, _) = harness.send(request).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_human_prefix_without_iap_is_unauthenticated() {
    let harness = Harness::start().await;
    let request = Request::get("/alpaca-read/v1/account/funds")
        .header(
            "authorization",
            format!("Bearer {}", bot_token(BOT_SUBJECT, BOT_AUDIENCE)),
        )
        .body(Body::empty())
        .unwrap();

    let (status, _) = harness.send(request).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn readers_have_no_mutation_routes() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(Tier::Read, "POST", "/wallet/withdrawals", Some(json!({})))
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "unknown_operation");
}

#[tokio::test]
async fn an_account_id_in_a_request_is_refused() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/activities?types=FEE&accountId={ACCOUNT_ID}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn a_disabled_operation_is_switched_off_without_touching_the_others() {
    let harness = Harness::start_with("disabled_operations = [\"account.inventory\"]", "").await;

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/account/inventory", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "capability_disabled");

    let (status, _) = harness.call(Tier::Bot, "GET", "/account/funds", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_human_budget_throttles_humans_and_never_the_bot() {
    let harness = Harness::start_with("human_budget_per_minute = 1", "").await;

    let (status, _) = harness
        .call(Tier::Read, "GET", "/account/funds", None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = harness
        .call(Tier::Read, "GET", "/account/funds", None)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "backpressure");
    assert!(body["retryAfterSecs"].as_u64().unwrap() >= 1);

    let (status, _) = harness.call(Tier::Bot, "GET", "/account/funds", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn alpaca_rate_limits_come_back_as_backpressure() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/positions"));
        then.status(429)
            .header("retry-after", "7")
            .json_body(json!({ "message": "rate limit exceeded" }));
    });

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/account/inventory", None)
        .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["retryAfterSecs"], 7);
    assert_eq!(body["retryable"], true);
}

#[tokio::test]
async fn a_read_past_its_deadline_answers_upstream_transient() {
    let harness = Harness::start_with_deadline(Duration::from_millis(100)).await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/calendar");
        then.status(200)
            .delay(Duration::from_secs(2))
            .json_body(json!([]));
    });

    let (status, body) = harness.call(Tier::Bot, "GET", "/market/open", None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_transient");
    assert_eq!(body["retryable"], true);
    assert_eq!(body["outcome"], serde_json::Value::Null);
}
