//! Health, startup dependency checks, identity, the human budget, deadlines,
//! extractor and identity refusals, shutdown and the capability switch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway::audit::MemorySink;
use st0x_alpaca_gateway::auth::UNVERIFIED;
use st0x_alpaca_gateway::config::GatewayConfig;
use st0x_alpaca_gateway::routes::{Verifiers, app};
use st0x_alpaca_gateway::state::{AppState, StartupError};
use st0x_alpaca_gateway_api::{AuditEvent, AuditPhase, ErrorCode, Operation, Outcome, Tier};
use tower::ServiceExt as _;

use crate::common::{
    ACCOUNT_ID, ACCOUNT_NUMBER, BOT_AUDIENCE, BOT_SUBJECT, Harness, READ_AUDIENCE, account_body,
    authorized, bot_token, config_text, iap_token, serve_activity_pages, serve_keys,
};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";

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
    assert_eq!(event.alpaca_status, Some(200));
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
    // The smallest budget config accepts: one `activities.list` reservation.
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
async fn an_operation_costing_more_than_the_whole_human_budget_is_refused_as_never_admissible() {
    let alpaca = MockServer::start_async().await;
    let keys = MockServer::start_async().await;
    serve_keys(&keys);
    alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/account"));
        then.status(200).json_body(account_body(ACCOUNT_NUMBER));
    });
    let activities = alpaca.mock(|when, then| {
        when.method(GET).path("/v1/accounts/activities");
        then.status(200).json_body(json!([]));
    });
    // Config validation refuses this budget; a state built around it still
    // must not tell callers to wait for a window that never fits the call.
    let mut config = GatewayConfig::parse(&config_text(&alpaca, &keys, "", "")).unwrap();
    config.human_budget_per_minute = 3;
    let state = AppState::connect(config, Arc::new(MemorySink::default()), "test".into())
        .await
        .unwrap();
    let app = app(state.clone(), &Verifiers::from_state(&state).unwrap());

    let request = authorized(
        Tier::Read,
        "GET",
        "/alpaca-read/v1/activities?types=FEE",
        None,
    );
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get("retry-after").is_none());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["retryable"], false);
    assert!(body["retryAfterSecs"].is_null(), "{body}");
    activities.assert_calls(0);
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
async fn a_malformed_mutation_body_answers_not_applied_and_is_audited() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/market",
            Some(json!({ "symbol": 5 })),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["outcome"], "not_applied");
    assert_refusal_audited(
        &harness,
        &body,
        Operation::OrdersPlaceMarket,
        ErrorCode::InvalidRequest,
        Some(Outcome::NotApplied),
    );
}

#[tokio::test]
async fn a_malformed_mutation_path_answers_not_applied_and_is_audited() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/orders/not-a-uuid/cancel",
            Some(json!({})),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["outcome"], "not_applied");
    assert_refusal_audited(
        &harness,
        &body,
        Operation::OrdersCancel,
        ErrorCode::InvalidRequest,
        Some(Outcome::NotApplied),
    );
}

#[tokio::test]
async fn a_malformed_read_path_is_refused_and_audited() {
    let harness = Harness::start().await;

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/orders/not-a-uuid", None)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["outcome"], Value::Null);
    assert_refusal_audited(
        &harness,
        &body,
        Operation::OrdersGet,
        ErrorCode::InvalidRequest,
        None,
    );
}

#[tokio::test]
async fn a_human_request_refused_before_alpaca_gives_its_budget_back() {
    let harness = Harness::start_with("human_budget_per_minute = 10", "").await;
    let journal = harness.alpaca.mock(|when, then| {
        when.method(POST).path("/v1/journals");
        then.status(200);
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/accounts/activities");
        then.status(200).json_body(json!([]));
    });

    // No reason: refused by the gateway before the work runs.
    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            &format!("/orders/{ORDER_ID}/cancel"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Refused inside the detached work, before its one request.
    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/journals",
            Some(json!({
                "counterparty": "stranger",
                "symbol": "AAPL",
                "qty": "5",
                "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
                "reason": "test"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    journal.assert_calls(0);

    // Reserves the whole budget, so it fits only if both units came back.
    let (status, body) = harness
        .call(Tier::Read, "GET", "/activities?types=FEE", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_human_read_that_sent_fewer_requests_than_reserved_gets_the_rest_back() {
    // Room for one ten page reservation plus the one page each list sends.
    let harness = Harness::start_with("human_budget_per_minute = 11", "").await;
    let page = harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/accounts/activities");
        then.status(200)
            .json_body(json!([{ "id": "act-1", "activity_type": "FEE" }]));
    });

    for _ in 0..2 {
        let (status, body) = harness
            .call(Tier::Read, "GET", "/activities?types=FEE", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    page.assert_calls(2);
}

#[tokio::test]
async fn a_read_its_caller_abandons_gives_back_what_it_did_not_send_and_is_audited() {
    // Room for one ten page reservation plus the one page the abandoned
    // list sent.
    let harness = Harness::start_with("human_budget_per_minute = 11", "").await;
    let stalled = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path("/v1/accounts/activities")
            .query_param("activity_types", "FEE");
        then.status(200)
            .delay(Duration::from_secs(30))
            .json_body(json!([]));
    });
    let answered = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path("/v1/accounts/activities")
            .query_param("activity_types", "DIV");
        then.status(200).json_body(json!([]));
    });

    // The caller goes away once the first page is on its way: the server
    // drops the request future, as hyper does on a closed connection.
    tokio::select! {
        answer = harness.call(Tier::Read, "GET", "/activities?types=FEE", None) => {
            panic!("the stalled read answered: {answer:?}");
        }
        () = async {
            while stalled.calls_async().await == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => {}
    }

    let events = harness.audit_events();
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event.operation, Operation::ActivitiesList);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert!(event.abandoned, "{event:?}");
    assert_eq!(event.code, None);
    assert_eq!(event.alpaca_status, None);

    // Reserves ten units: they fit only if the abandoned list kept just
    // the one page it sent.
    let (status, body) = harness
        .call(Tier::Read, "GET", "/activities?types=DIV", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    answered.assert_calls(1);
}

#[tokio::test]
async fn a_human_page_cap_refusal_keeps_the_budget_its_pages_spent() {
    let harness = Harness::start_with("human_budget_per_minute = 10", "").await;
    serve_activity_pages(&harness, 11);

    let (status, body) = harness
        .call(Tier::Read, "GET", "/activities?types=FEE", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = harness
        .call(Tier::Read, "GET", "/account/funds", None)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
}

#[tokio::test]
async fn a_mutation_in_flight_at_shutdown_answers_outcome_unknown_at_once() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(format!(
            "/v1/trading/accounts/{ACCOUNT_ID}/orders/{ORDER_ID}"
        ));
        then.status(204).delay(Duration::from_secs(30));
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(st0x_alpaca_gateway::run(
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
    let answer = tokio::spawn(request);
    while harness.state.tasks.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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
}
