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

/// The mint Alpaca must receive for [`mint_body`] to [`BOT_WALLET`] on base:
/// the issuer request id as both the `Idempotency-Key` header and
/// `client_request_id`. Anything else gets httpmock's 404.
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

/// A replay of the same mint sends the same idempotency key, so Alpaca
/// dedupes it.
#[tokio::test]
async fn bot_mint_to_the_pinned_recipient_is_applied_audited_and_replayed_with_its_key() {
    let harness = Harness::start().await;
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
        assert_eq!(body["tokenization_request_id"], "tok_req_1");
        assert_eq!(body["status"], "pending");
        assert_eq!(body["client_request_id"], ISSUER_REQUEST_ID);
    }
    mint.assert_calls(2);

    let events = harness.audit_events();
    assert_eq!(events.len(), 2);
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

/// A definitive rejection of the mint left Alpaca untouched. A server error
/// may have minted, and so may a mint resent under an issuer request id
/// Alpaca already holds, so both are resendable under the same key.
#[tokio::test]
async fn a_failed_mint_is_not_applied_when_rejected_and_outcome_unknown_otherwise() {
    for (alpaca_status, answer, status, code, outcome, same_key) in [
        (
            422,
            json!({ "message": "No positions found for AAPL" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::Rejected,
            Outcome::NotApplied,
            false,
        ),
        (
            500,
            json!({ "message": "internal error" }),
            StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::OutcomeUnknown,
            Outcome::Unknown,
            true,
        ),
        (
            422,
            json!({ "message": "issuer request id has already been used" }),
            StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::OutcomeUnknown,
            Outcome::Unknown,
            true,
        ),
    ] {
        let harness = Harness::start().await;
        let mint = expect_mint(&harness, alpaca_status, answer);

        let (got, body) = harness
            .call(
                Tier::Write,
                "POST",
                "/tokenization/mints",
                Some(mint_body(BOT_WALLET, "base", Some("move shares onchain"))),
            )
            .await;

        assert_eq!(got, status, "{alpaca_status}: {body}");
        assert_eq!(body["code"], json!(code), "{alpaca_status}");
        assert_eq!(body["outcome"], json!(outcome), "{alpaca_status}");
        assert_eq!(body["retryable"], false, "{alpaca_status}");
        assert_eq!(body["retryableWithSameKey"], same_key, "{alpaca_status}");
        mint.assert_calls(1);
        let events = harness.audit_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome, Some(outcome));
        assert_eq!(events[0].code, Some(code));
        assert_eq!(events[0].reason.as_deref(), Some("move shares onchain"));
    }
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

/// The ids of the tokenization requests a lookup answered: a list, one
/// request, or none.
fn request_ids(body: &Value) -> Vec<&str> {
    fn id(request: &Value) -> Option<&str> {
        request["tokenization_request_id"].as_str()
    }
    match &body["requests"] {
        Value::Array(requests) => requests.iter().filter_map(id).collect(),
        _ => id(&body["request"])
            .or_else(|| id(body))
            .into_iter()
            .collect(),
    }
}

/// Each lookup answers from the list Alpaca holds: the pending list only
/// the pending requests (Alpaca has been seen ignoring the filter), a
/// request by id or a definite not found, a mint by its issuer request id
/// or null, and a redemption by its transaction.
#[tokio::test]
async fn lookups_answer_the_matching_request_or_a_definite_absence() {
    let harness = Harness::start().await;
    let pending = alpaca_request("tok_req_pending", "mint", "pending");
    let completed = alpaca_request("tok_req_1", "mint", "completed");
    let mut redemption = alpaca_request("tok_req_redeem", "redeem", "pending");
    redemption["tx_hash"] = json!(REDEMPTION_TX);
    redemption["client_request_id"] = Value::Null;
    let every = json!([pending, completed]);
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("status", "pending");
        then.status(200).json_body(every.clone());
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("type", "mint");
        then.status(200).json_body(json!([completed]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param("type", "redeem");
        then.status(200).json_body(json!([redemption]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(requests_path())
            .query_param_missing("status")
            .query_param_missing("type");
        then.status(200).json_body(every.clone());
    });

    let unknown_mint = "0d6f3a52-1c1e-4f43-8a51-7b9e4f2f9a01";
    for (tier, path, status, reason, ids) in [
        (
            Tier::Read,
            "/tokenization/requests?pendingOnly=true".to_string(),
            StatusCode::OK,
            None,
            &["tok_req_pending"][..],
        ),
        (
            Tier::Bot,
            "/tokenization/requests".to_string(),
            StatusCode::OK,
            None,
            &["tok_req_pending", "tok_req_1"][..],
        ),
        (
            Tier::Bot,
            "/tokenization/requests/tok_req_1?network=base".to_string(),
            StatusCode::OK,
            None,
            &["tok_req_1"][..],
        ),
        // Alpaca answered the list with 200; no Alpaca 404 is invented.
        (
            Tier::Bot,
            "/tokenization/requests/tok_req_missing?network=base".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("request_not_found"),
            &[][..],
        ),
        (
            Tier::Bot,
            format!("/tokenization/mints/by-issuer-request-id/{ISSUER_REQUEST_ID}"),
            StatusCode::OK,
            None,
            &["tok_req_1"][..],
        ),
        (
            Tier::Bot,
            format!("/tokenization/mints/by-issuer-request-id/{unknown_mint}"),
            StatusCode::OK,
            None,
            &[][..],
        ),
        (
            Tier::Read,
            format!("/tokenization/redemptions/by-tx/{REDEMPTION_TX}?network=base"),
            StatusCode::OK,
            None,
            &["tok_req_redeem"][..],
        ),
    ] {
        let (got, body) = harness.call(tier, "GET", &path, None).await;

        assert_eq!(got, status, "{path}: {body}");
        assert_eq!(body["reason"], json!(reason), "{path}");
        assert!(body["alpacaStatus"].is_null(), "{path}: {body}");
        assert_eq!(request_ids(&body), ids, "{path}: {body}");
    }
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

    for (id, reason) in [
        ("tok_req_1", "wrong_network"),
        ("tok_req_2", "network_missing"),
    ] {
        let (status, body) = harness
            .call(
                Tier::Bot,
                "GET",
                &format!("/tokenization/requests/{id}?network=base"),
                None,
            )
            .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{id}: {body}");
        assert_eq!(body["code"], "rejected", "{id}");
        assert_eq!(body["reason"], reason, "{id}");
        assert_eq!(body["retryable"], false, "{id}");
    }

    let events = harness.audit_events();
    let audited: Vec<_> = events
        .iter()
        .map(|event| (event.rejection, event.alpaca_object_id.as_deref()))
        .collect();
    assert_eq!(
        audited,
        [
            (Some(RejectionReason::WrongNetwork), Some("tok_req_1")),
            (Some(RejectionReason::NetworkMissing), Some("tok_req_2")),
        ]
    );
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
    mint.assert_calls(1);
    let event = &harness.audit_events()[0];
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("tok_req_1"));
}
