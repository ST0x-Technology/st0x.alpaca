//! Mints to pinned recipients, their idempotent replay, and the request
//! lookups.

use axum::http::StatusCode;
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AuditPhase, ErrorCode, Operation, Outcome, RejectionReason, Tier};

use crate::common::{ACCOUNT_ID, BOT_WALLET, Harness, MARKET_MAKER_WALLET};

const ISSUER_REQUEST_ID: &str = "6b1d0a7e-3f2c-4c55-9a4e-2f1b9b2c7d10";
const REDEMPTION_TX: &str = "0xabababababababababababababababababababababababababababababababab";

fn mint_path() -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/tokenization/mint")
}

fn requests_path() -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests")
}

fn mint_body(wallet: &str, network: &str, reason: Option<&str>) -> Value {
    let mut body = json!({
        "issuerRequestId": ISSUER_REQUEST_ID,
        "symbol": "AAPL",
        "quantity": "2.5",
        "walletAddress": wallet,
        "network": network,
    });
    if let Some(reason) = reason {
        body["reason"] = json!(reason);
    }
    body
}

/// A tokenization request as Alpaca reports it.
fn alpaca_request(id: &str, kind: &str, status: &str) -> Value {
    json!({
        "tokenization_request_id": id,
        "type": kind,
        "status": status,
        "underlying_symbol": "AAPL",
        "token_symbol": "tAAPL",
        "qty": "2.5",
        "issuer": "st0x",
        "network": "base",
        "wallet_address": BOT_WALLET,
        "client_request_id": ISSUER_REQUEST_ID,
        "created_at": "2026-10-05T10:30:00Z"
    })
}

/// The mint Alpaca must receive for [`mint_body`] to [`BOT_WALLET`] on base.
fn expect_mint(harness: &Harness, status: u16, answer: Value) -> httpmock::Mock<'_> {
    harness.alpaca.mock(|when, then| {
        when.method(POST)
            .path(mint_path())
            .header("Idempotency-Key", ISSUER_REQUEST_ID)
            .json_body(json!({
                "underlying_symbol": "AAPL",
                "qty": "2.5",
                "issuer": "st0x",
                "network": "base",
                "wallet_address": BOT_WALLET,
                "client_request_id": ISSUER_REQUEST_ID,
            }));
        then.status(status).json_body(answer);
    })
}

#[tokio::test]
async fn bot_mint_to_the_pinned_recipient_is_applied_and_audited() {
    let harness = Harness::start().await;
    let mint = expect_mint(
        &harness,
        200,
        alpaca_request("tok_req_1", "mint", "pending"),
    );

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/tokenization/mints",
            Some(mint_body(BOT_WALLET, "base", None)),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "tok_req_1");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["clientRequestId"], ISSUER_REQUEST_ID);
    mint.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.operation, Operation::TokenizationMint);
    assert_eq!(event.phase, AuditPhase::Answered);
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("tok_req_1"));
    assert_eq!(event.key.as_deref(), Some(ISSUER_REQUEST_ID));
    assert_eq!(event.summary["recipient"], BOT_WALLET);
    assert_eq!(event.summary["symbol"], "AAPL");
    assert_eq!(event.summary["quantity"], "2.5");
    assert_eq!(event.summary["network"], "base");
}

#[tokio::test]
async fn a_replayed_mint_sends_the_same_idempotency_key_and_client_request_id() {
    let harness = Harness::start().await;
    // Matches only a POST carrying the issuer request id as both the
    // `Idempotency-Key` header and `client_request_id`; anything else gets
    // httpmock's 404 and fails the call.
    let mint = expect_mint(
        &harness,
        200,
        alpaca_request("tok_req_1", "mint", "pending"),
    );

    for _ in 0..2 {
        let (status, body) = harness
            .call(
                Tier::Bot,
                "POST",
                "/tokenization/mints",
                Some(mint_body(BOT_WALLET, "base", None)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], "tok_req_1");
    }

    mint.assert_calls(2);
}

#[tokio::test]
async fn a_mint_outside_the_pinned_recipients_is_refused_on_every_tier() {
    let harness = Harness::start().await;
    let mint = harness.alpaca.mock(|when, then| {
        when.method(POST).path(mint_path());
        then.status(200)
            .json_body(alpaca_request("tok_req_1", "mint", "pending"));
    });

    for (tier, reason) in [
        (Tier::Bot, None),
        (Tier::Write, Some("move shares onchain")),
    ] {
        let (status, body) = harness
            .call(
                tier,
                "POST",
                "/tokenization/mints",
                Some(mint_body(MARKET_MAKER_WALLET, "base", reason)),
            )
            .await;

        assert_eq!(status, StatusCode::FORBIDDEN, "{tier:?}: {body}");
        assert_eq!(body["code"], "forbidden");
        assert_eq!(body["reason"], "destination_not_allowed");
        assert_eq!(body["outcome"], "not_applied");
    }

    mint.assert_calls(0);
    let events = harness.audit_events();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event.outcome, Some(Outcome::NotApplied));
        assert_eq!(
            event.rejection,
            Some(RejectionReason::DestinationNotAllowed)
        );
        assert_eq!(event.summary["recipient"], MARKET_MAKER_WALLET);
    }
}

#[tokio::test]
async fn a_definitive_mint_rejection_answers_rejected_and_not_applied() {
    let harness = Harness::start().await;
    let mint = expect_mint(
        &harness,
        422,
        json!({ "message": "No positions found for AAPL" }),
    );

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/tokenization/mints",
            Some(mint_body(BOT_WALLET, "base", Some("move shares onchain"))),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "insufficient_position");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["retryableWithSameKey"], false);
    mint.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, Some(Outcome::NotApplied));
    assert_eq!(
        events[0].rejection,
        Some(RejectionReason::InsufficientPosition)
    );
    assert_eq!(events[0].reason.as_deref(), Some("move shares onchain"));
}

#[tokio::test]
async fn a_server_error_on_the_mint_is_outcome_unknown_and_resendable_with_the_same_key() {
    let harness = Harness::start().await;
    let mint = expect_mint(&harness, 500, json!({ "message": "internal error" }));

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/tokenization/mints",
            Some(mint_body(BOT_WALLET, "base", None)),
        )
        .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["retryable"], false);
    assert_eq!(body["retryableWithSameKey"], true);
    mint.assert_calls(1);

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].outcome, Some(Outcome::Unknown));
    assert_eq!(events[0].code, Some(ErrorCode::OutcomeUnknown));
}

#[tokio::test]
async fn an_unconfigured_network_is_refused_before_anything_is_sent() {
    let harness = Harness::start().await;
    let mint = harness.alpaca.mock(|when, then| {
        when.method(POST).path(mint_path());
        then.status(200)
            .json_body(alpaca_request("tok_req_1", "mint", "pending"));
    });
    let lookups = harness.alpaca.mock(|when, then| {
        when.method(GET).path(requests_path());
        then.status(200).json_body(json!([]));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/tokenization/mints",
            Some(mint_body(BOT_WALLET, "ethereum", None)),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "unsupported_network");
    assert_eq!(body["outcome"], "not_applied");

    let (status, body) = harness
        .call(
            Tier::Read,
            "GET",
            "/tokenization/requests/tok_req_1?network=ethereum",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["reason"], "unsupported_network");

    mint.assert_calls(0);
    lookups.assert_calls(0);
}

#[tokio::test]
async fn pending_only_lists_just_the_requests_alpaca_reports_pending() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("status", "pending");
        // Alpaca has been seen ignoring the filter; the crate drops the rest.
        then.status(200).json_body(json!([
            alpaca_request("tok_req_pending", "mint", "pending"),
            alpaca_request("tok_req_done", "mint", "completed"),
        ]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param_missing("status");
        then.status(200).json_body(json!([
            alpaca_request("tok_req_pending", "mint", "pending"),
            alpaca_request("tok_req_done", "mint", "completed"),
        ]));
    });

    let (status, body) = harness
        .call(
            Tier::Read,
            "GET",
            "/tokenization/requests?pendingOnly=true",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["tok_req_pending"]);

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/tokenization/requests", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["requests"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn lookups_answer_the_matching_request_or_null() {
    let harness = Harness::start().await;
    let mut redemption = alpaca_request("tok_req_redeem", "redeem", "pending");
    redemption["tx_hash"] = json!(REDEMPTION_TX);
    redemption["client_request_id"] = Value::Null;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("type", "mint");
        then.status(200)
            .json_body(json!([alpaca_request("tok_req_1", "mint", "completed")]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("type", "redeem");
        then.status(200).json_body(json!([redemption]));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/tokenization/mints/by-issuer-request-id/{ISSUER_REQUEST_ID}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["request"]["id"], "tok_req_1");

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/tokenization/mints/by-issuer-request-id/0d6f3a52-1c1e-4f43-8a51-7b9e4f2f9a01",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["request"], Value::Null);

    let (status, body) = harness
        .call(
            Tier::Read,
            "GET",
            &format!("/tokenization/redemptions/by-tx/{REDEMPTION_TX}?network=base"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["request"]["id"], "tok_req_redeem");
    assert_eq!(body["request"]["txHash"], REDEMPTION_TX);
}

#[tokio::test]
async fn an_unknown_request_id_is_a_definite_not_found() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(requests_path());
        then.status(200)
            .json_body(json!([alpaca_request("tok_req_1", "mint", "completed")]));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/tokenization/requests/tok_req_1?network=base",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "completed");

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/tokenization/requests/tok_req_missing?network=base",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "request_not_found");
    assert_eq!(body["retryable"], false);
    // Alpaca answered the list with 200; no Alpaca 404 is invented.
    assert!(body["alpacaStatus"].is_null(), "{body}");
}

#[tokio::test]
async fn a_lookup_alpaca_answers_off_the_bound_network_is_rejected_naming_the_request() {
    let harness = Harness::start().await;
    let mut foreign = alpaca_request("tok_req_1", "mint", "completed");
    foreign["network"] = json!("ethereum");
    let mut unnamed = alpaca_request("tok_req_2", "mint", "completed");
    unnamed["network"] = Value::Null;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(requests_path());
        then.status(200).json_body(json!([foreign, unnamed]));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/tokenization/requests/tok_req_1?network=base",
            None,
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "rejected");
    assert_eq!(body["reason"], "wrong_network");
    assert_eq!(body["network"], "ethereum");
    assert_eq!(body["alpacaObjectIds"], json!(["tok_req_1"]));
    assert_eq!(body["retryable"], false);

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/tokenization/requests/tok_req_2?network=base",
            None,
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["reason"], "network_missing");
    assert!(body["network"].is_null(), "{body}");
    assert_eq!(body["alpacaObjectIds"], json!(["tok_req_2"]));

    // Both refusals were decided after Alpaca answered 200, and the audit
    // keeps that status.
    let events = harness.audit_events();
    let rejections: Vec<_> = events.iter().map(|event| event.rejection).collect();
    assert_eq!(
        rejections,
        [
            Some(RejectionReason::WrongNetwork),
            Some(RejectionReason::NetworkMissing)
        ]
    );
    for event in &events {
        assert_eq!(event.alpaca_status, Some(200), "{event:?}");
    }
}

#[tokio::test]
async fn a_mint_alpaca_answers_on_another_network_is_outcome_unknown() {
    let harness = Harness::start().await;
    let mut foreign = alpaca_request("tok_req_1", "mint", "pending");
    foreign["network"] = json!("ethereum");
    let mint = expect_mint(&harness, 200, foreign);

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/tokenization/mints",
            Some(mint_body(BOT_WALLET, "base", None)),
        )
        .await;

    // Alpaca accepted a mint; its answer naming another network cannot
    // make that mint not exist, and the audit names it.
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["alpacaObjectIds"], json!(["tok_req_1"]));
    mint.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("tok_req_1"));
    assert_eq!(event.alpaca_status, Some(200));
}
