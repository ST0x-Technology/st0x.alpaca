//! Health, startup dependency checks, identity, the human budget, deadlines,
//! extractor and identity refusals, shutdown and the capability switch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca::broker::AlpacaBrokerApiError;
use st0x_alpaca_gateway::audit::MemorySink;
use st0x_alpaca_gateway::auth::UNVERIFIED;
use st0x_alpaca_gateway::config::GatewayConfig;
use st0x_alpaca_gateway::state::{AppState, StartupError};
use st0x_alpaca_gateway_api::{AuditEvent, AuditPhase, ErrorCode, Operation, Outcome, Tier};

use crate::common::{
    ACCOUNT_ID, ACCOUNT_NUMBER, ACCOUNT_REQUEST_ID, BOT_AUDIENCE, BOT_SUBJECT, Harness,
    READ_AUDIENCE, WRITE_AUDIENCE, account_body, assert_read_audited, bot_token, config_text,
    iap_token, until_called,
};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";

#[tokio::test]
async fn health_and_readiness_answer_without_credentials() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .send(Request::get("/healthz").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "status": "ok" }));

    let (status, body) = harness
        .send(Request::get("/readyz").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "status": "ready", "environment": "staging", "version": "test" })
    );
}

#[tokio::test]
async fn startup_fails_closed_on_an_inactive_or_another_account_or_an_unreachable_alpaca() {
    let inactive: fn(&StartupError) -> bool = |error| {
        matches!(
            error,
            StartupError::Broker(AlpacaBrokerApiError::AccountNotActive { .. })
        )
    };
    let mismatch: fn(&StartupError) -> bool = |error| matches!(error, StartupError::AccountMismatch { reported, .. } if reported.as_deref() == Some("S01-0001"));
    let unreachable: fn(&StartupError) -> bool = |error| matches!(error, StartupError::Broker(_));
    let mut submitted = account_body(ACCOUNT_NUMBER);
    submitted["status"] = json!("SUBMITTED");
    for (status, answer, refused) in [
        (200, submitted, inactive),
        (200, account_body("S01-0001"), mismatch),
        (503, json!({ "message": "unavailable" }), unreachable),
    ] {
        let alpaca = MockServer::start_async().await;
        alpaca.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/account"));
            then.status(status).json_body(answer);
        });

        let config = GatewayConfig::parse(&config_text(&alpaca, "", "")).unwrap();
        let error = AppState::connect(config, Arc::new(MemorySink::default()), "test".into())
            .await
            .err()
            .unwrap();

        assert!(refused(&error), "{status}: {error}");
    }
}

#[tokio::test]
async fn the_bot_reads_its_funds_and_withdrawable_cash_and_each_read_is_audited() {
    // The account answer has cash 1500.25 and withdrawable cash 1000.10;
    // the library reads buying power from the cash.
    for (path, operation, expected) in [
        (
            "/account/funds",
            Operation::AccountFunds,
            json!({ "balance": 150_025, "buyingPower": 150_025, "withdrawable": 100_010 }),
        ),
        (
            "/account/withdrawable-cash",
            Operation::AccountWithdrawableCash,
            json!({ "withdrawableCents": 100_010 }),
        ),
    ] {
        let harness = Harness::start().await;

        let (status, body) = harness.call(Tier::Bot, "GET", path, None).await;

        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body, expected, "{path}");
        assert_read_audited(&harness, operation, Tier::Bot, None, &[ACCOUNT_REQUEST_ID]);
    }
}

#[tokio::test]
async fn a_request_without_its_tier_credential_is_unauthenticated() {
    let harness = Harness::start().await;
    let bearer = |audience| format!("Bearer {}", bot_token(BOT_SUBJECT, audience));
    for (path, authorization) in [
        ("/bot/v1/account/funds", None),
        (
            "/bot/v1/account/funds",
            Some(bearer("https://other.run.app")),
        ),
        ("/alpaca-read/v1/account/funds", Some(bearer(BOT_AUDIENCE))),
    ] {
        let mut request = Request::get(path);
        if let Some(authorization) = &authorization {
            request = request.header("authorization", authorization);
        }

        let (status, body) = harness.send(request.body(Body::empty()).unwrap()).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {authorization:?}");
        assert_eq!(body["code"], "unauthenticated", "{path} {authorization:?}");
    }
}

#[tokio::test]
async fn a_service_account_outside_bot_principals_is_forbidden_and_audited() {
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
    let event = assert_refusal_audited(
        &harness,
        &body,
        Operation::AccountFunds,
        ErrorCode::Forbidden,
        None,
    );
    assert_eq!(event.principal, "999");
    assert_eq!(event.tier, Tier::Bot);
}

#[tokio::test]
async fn an_unauthenticated_mutation_answers_not_applied_and_is_audited() {
    let harness = Harness::start().await;
    let request = Request::post(format!("/bot/v1/orders/{ORDER_ID}/cancel"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let (status, body) = harness.send(request).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "unauthenticated");
    assert_eq!(body["outcome"], "not_applied");
    let event = assert_refusal_audited(
        &harness,
        &body,
        Operation::OrdersCancel,
        ErrorCode::Unauthenticated,
        Some(Outcome::NotApplied),
    );
    assert_eq!(event.principal, UNVERIFIED);
    assert_eq!(event.principal_email, None);
}

/// A read tier assertion replayed against a write tier withdrawal is
/// refused before the handler runs, and the refusal is audited; the same
/// request with the write tier's own assertion reaches Alpaca.
#[tokio::test]
async fn a_read_tier_assertion_does_not_open_the_write_tier() {
    let harness = Harness::start().await;
    let whitelist = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/wallets/whitelists"));
        then.status(200).json_body(json!([]));
    });
    let withdrawal = json!({
        "amount": "250.5",
        "asset": "USDC",
        "address": "0x3333333333333333333333333333333333333333",
        "operationId": "3d9b1a7e-5f2c-4e8d-a6b0-9c1d2e3f4a5b",
        "reason": "move inventory"
    });
    let request = |assertion: String| {
        Request::post("/alpaca-write/v1/wallet/withdrawals")
            .header("x-goog-iap-jwt-assertion", assertion)
            .header("content-type", "application/json")
            .body(Body::from(withdrawal.to_string()))
            .unwrap()
    };

    let (status, body) = harness.send(request(iap_token(READ_AUDIENCE))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "unauthenticated");
    assert_eq!(body["outcome"], "not_applied");
    let event = assert_refusal_audited(
        &harness,
        &body,
        Operation::WalletWithdraw,
        ErrorCode::Unauthenticated,
        Some(Outcome::NotApplied),
    );
    assert_eq!(event.principal, UNVERIFIED);
    assert_eq!(event.tier, Tier::Write);
    whitelist.assert_calls(0);

    harness.send(request(iap_token(WRITE_AUDIENCE))).await;
    whitelist.assert_calls(1);
}

#[tokio::test]
async fn readers_have_no_mutation_routes() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(Tier::Read, "POST", "/wallet/withdrawals", Some(json!({})))
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "unknown_operation");
    assert_eq!(body["outcome"], "not_applied");
    assert!(harness.audit_events().is_empty());
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
    let harness = Harness::start_with("human_budget_per_minute = 10", "").await;

    for _ in 0..10 {
        let (status, body) = harness
            .call(Tier::Read, "GET", "/account/funds", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, body) = harness
        .call(Tier::Read, "GET", "/account/funds", None)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["retryable"], true);
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

/// The one audit record of a refused request: answered, under the request
/// id the caller saw.
fn assert_refusal_audited(
    harness: &Harness,
    body: &Value,
    operation: Operation,
    code: ErrorCode,
    outcome: Option<Outcome>,
) -> AuditEvent {
    let mut events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    let event = events.remove(0);
    assert_eq!(event.operation, operation);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert_eq!(event.code, Some(code));
    assert_eq!(event.outcome, outcome);
    assert_eq!(body["requestId"], event.request_id.to_string());
    event
}

#[tokio::test]
async fn malformed_input_is_refused_and_audited() {
    for (method, path, body, operation, outcome) in [
        (
            "POST",
            "/orders/market",
            Some(json!({ "symbol": 5 })),
            Operation::OrdersPlaceMarket,
            Some(Outcome::NotApplied),
        ),
        (
            "POST",
            "/orders/not-a-uuid/cancel",
            Some(json!({})),
            Operation::OrdersCancel,
            Some(Outcome::NotApplied),
        ),
        (
            "GET",
            "/orders/not-a-uuid",
            None,
            Operation::OrdersGet,
            None,
        ),
    ] {
        let harness = Harness::start().await;

        let (status, answer) = harness.call(Tier::Bot, method, path, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {answer}");
        assert_eq!(answer["code"], "invalid_request", "{path}");
        assert_eq!(answer["outcome"], json!(outcome), "{path}");
        assert_refusal_audited(
            &harness,
            &answer,
            operation,
            ErrorCode::InvalidRequest,
            outcome,
        );
    }
}

/// The cancel has left for Alpaca when shutdown starts: one the send gate
/// held back would have nothing in flight. The caller hears
/// `outcome_unknown` at once, and `run` returns within the shutdown grace
/// although the cancel's detached work still waits for its answer.
#[tokio::test]
async fn a_mutation_in_flight_at_shutdown_answers_outcome_unknown_at_once() {
    let harness = Harness::start().await;
    let cancel = harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(format!(
            "/v1/trading/accounts/{ACCOUNT_ID}/orders/{ORDER_ID}"
        ));
        then.status(204).delay(Duration::from_secs(30));
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(st0x_alpaca_gateway::run(
        harness.state.clone(),
        listener,
        async move {
            let _ = shutdown.await;
        },
    ));

    let request = reqwest::Client::new()
        .post(format!("http://{address}/bot/v1/orders/{ORDER_ID}/cancel"))
        .bearer_auth(bot_token(BOT_SUBJECT, BOT_AUDIENCE))
        .json(&json!({}))
        .send();
    let mut answer = tokio::spawn(request);
    until_called(&cancel, &mut answer).await;

    let signalled = Instant::now();
    signal.send(()).unwrap();
    let response = answer.await.unwrap().unwrap();

    assert!(signalled.elapsed() < Duration::from_secs(1));
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].phase, AuditPhase::Answered);
    assert_eq!(events[0].outcome, Some(Outcome::Unknown));

    // The eight second grace, then exit, with the cancel's answer still 20
    // seconds away and its work still running.
    tokio::time::timeout(Duration::from_secs(9), served)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(signalled.elapsed() < Duration::from_secs(9));
    assert_eq!(harness.state.tasks.len(), 1);
    assert_eq!(harness.audit_events().len(), 1);
}
