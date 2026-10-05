//! Withdrawals with their pinned destinations, transfer reads, the
//! whitelist and journals.

use std::time::Duration;

use axum::http::StatusCode;
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{AuditEvent, AuditPhase, ErrorCode, Operation, Outcome, Tier};

use crate::common::{ACCOUNT_ID, BOT_WALLET, Harness, JOURNAL_COUNTERPARTY, MARKET_MAKER_WALLET};

const TRANSFER_ID: &str = "0f8a4c62-1c3e-4a5b-9d7e-2b6f8c9d0e1a";
const OTHER_WALLET: &str = "0x3333333333333333333333333333333333333333";
const JOURNAL_ID: &str = "7c1e2d3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f";

fn whitelist_path() -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/wallets/whitelists")
}

fn transfers_path() -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/wallets/transfers")
}

fn whitelist_entry(id: &str, address: &str, status: &str) -> Value {
    json!({
        "id": id,
        "address": address,
        "asset": "USDC",
        "chain": "ETH",
        "status": status,
        "created_at": "2026-10-01T12:00:00Z"
    })
}

fn transfer_json(to: &str) -> Value {
    json!({
        "id": TRANSFER_ID,
        "tx_hash": null,
        "direction": "OUTGOING",
        "amount": "250.5",
        "chain": "ETH",
        "asset": "USDC",
        "from_address": BOT_WALLET,
        "to_address": to,
        "status": "PENDING",
        "created_at": "2026-10-05T09:30:00Z"
    })
}

fn withdrawal(address: &str) -> Value {
    json!({
        "amount": "250.5",
        "asset": "USDC",
        "address": address,
        "operationId": "3d9b1a7e-5f2c-4e8d-a6b0-9c1d2e3f4a5b",
        "reason": "move inventory"
    })
}

/// Serves the whitelist with every configured address approved.
fn serve_approved_whitelist(harness: &Harness) {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200).json_body(json!([
            whitelist_entry("wl-mm", MARKET_MAKER_WALLET, "APPROVED"),
            whitelist_entry("wl-other", OTHER_WALLET, "APPROVED"),
        ]));
    });
}

fn answered(events: &[AuditEvent]) -> Option<&AuditEvent> {
    events
        .iter()
        .find(|event| event.phase == AuditPhase::Answered)
}

#[tokio::test]
async fn bot_withdrawal_to_the_market_maker_wallet_is_applied_and_audited() {
    let harness = Harness::start().await;
    serve_approved_whitelist(&harness);
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path()).json_body_includes(
            json!({ "address": MARKET_MAKER_WALLET, "asset": "USDC" }).to_string(),
        );
        then.status(200)
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(MARKET_MAKER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], TRANSFER_ID);
    assert_eq!(body["amount"], "250.5");
    assert_eq!(body["status"], "PENDING");
    post.assert_calls(1);

    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.operation, Operation::WalletWithdraw);
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some(TRANSFER_ID));
    assert_eq!(
        event.key.as_deref(),
        Some("3d9b1a7e-5f2c-4e8d-a6b0-9c1d2e3f4a5b")
    );
    assert_eq!(
        event.summary["destination"].to_lowercase(),
        MARKET_MAKER_WALLET
    );
    assert_eq!(event.summary["amount"], "250.5");
}

#[tokio::test]
async fn bot_withdrawal_to_another_approved_address_is_refused_before_sending() {
    let harness = Harness::start().await;
    serve_approved_whitelist(&harness);
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200).json_body(transfer_json(OTHER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(OTHER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "destination_not_allowed");
    assert_eq!(body["outcome"], "not_applied");
    post.assert_calls(0);
    assert_eq!(
        answered(&harness.audit_events()).unwrap().outcome,
        Some(Outcome::NotApplied)
    );
}

#[tokio::test]
async fn a_human_withdrawal_to_a_bot_destination_is_refused_before_sending() {
    let harness = Harness::start().await;
    serve_approved_whitelist(&harness);
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200)
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(MARKET_MAKER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "destination_not_allowed");
    post.assert_calls(0);
}

#[tokio::test]
async fn a_withdrawal_to_an_address_awaiting_approval_is_rejected_before_sending() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200).json_body(json!([whitelist_entry(
            "wl-other",
            OTHER_WALLET,
            "PENDING"
        )]));
    });
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200).json_body(transfer_json(OTHER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(OTHER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["reason"], "address_not_whitelisted");
    assert_eq!(body["outcome"], "not_applied");
    post.assert_calls(0);
}

#[tokio::test]
async fn a_failed_whitelist_read_answers_not_applied_without_sending() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(503).body("unavailable");
    });
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200)
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(MARKET_MAKER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_transient");
    assert_eq!(body["outcome"], "not_applied");
    post.assert_calls(0);
}

#[tokio::test]
async fn a_withdrawal_past_its_deadline_answers_outcome_unknown_and_settles_applied() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    serve_approved_whitelist(&harness);
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200)
            .delay(Duration::from_secs(1))
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(MARKET_MAKER_WALLET)),
        )
        .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["retryableWithSameKey"], false);

    harness.settle().await;
    let events = harness.audit_events();
    let settled = events
        .iter()
        .find(|event| event.phase == AuditPhase::Settled)
        .unwrap();
    assert_eq!(settled.outcome, Some(Outcome::Applied));
    assert_eq!(settled.alpaca_object_id.as_deref(), Some(TRANSFER_ID));
}

#[tokio::test]
async fn a_transfer_read_reports_the_fees_alpaca_charged() {
    let harness = Harness::start().await;
    let mut transfer = transfer_json(MARKET_MAKER_WALLET);
    transfer["network_fee"] = json!("1.25");
    transfer["fees"] = json!("0.5");
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{TRANSFER_ID}", transfers_path()));
        then.status(200).json_body(transfer);
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/wallet/transfers/{TRANSFER_ID}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], TRANSFER_ID);
    assert_eq!(body["reportedFees"], "1.75");
}

#[tokio::test]
async fn a_journal_to_the_issuer_goes_to_its_configured_account_and_answers_without_account_ids() {
    let harness = Harness::start().await;
    let journal = harness.alpaca.mock(|when, then| {
        when.method(POST).path("/v1/journals").json_body_includes(
            json!({
                "from_account": ACCOUNT_ID,
                "to_account": JOURNAL_COUNTERPARTY,
                "entry_type": "JNLS",
                "symbol": "AAPL",
                "qty": "5"
            })
            .to_string(),
        );
        then.status(200).json_body(json!({
            "id": JOURNAL_ID,
            "status": "queued",
            "symbol": "AAPL",
            "qty": "5",
            "price": "182.5",
            "from_account": ACCOUNT_ID,
            "to_account": JOURNAL_COUNTERPARTY,
            "settle_date": "2026-10-07",
            "system_date": "2026-10-05",
            "description": null
        }));
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/journals",
            Some(json!({
                "counterparty": "issuer",
                "symbol": "AAPL",
                "qty": "5",
                "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
                "reason": "return shares to the issuer"
            })),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    journal.assert_calls(1);
    assert_eq!(body["id"], JOURNAL_ID);
    assert_eq!(body["status"], "queued");
    assert_eq!(body["settleDate"], "2026-10-07");
    let text = body.to_string();
    assert!(!text.contains(ACCOUNT_ID), "{text}");
    assert!(!text.contains(JOURNAL_COUNTERPARTY), "{text}");

    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some(JOURNAL_ID));
    assert_eq!(event.summary["counterparty"], "issuer");
    let audit = serde_json::to_string(event).unwrap();
    assert!(!audit.contains(JOURNAL_COUNTERPARTY), "{audit}");
}

#[tokio::test]
async fn a_journal_to_an_unknown_counterparty_is_refused_before_sending() {
    let harness = Harness::start().await;
    let journal = harness.alpaca.mock(|when, then| {
        when.method(POST).path("/v1/journals");
        then.status(200);
    });

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
    assert_eq!(body["reason"], "destination_not_allowed");
    assert_eq!(body["outcome"], "not_applied");
    journal.assert_calls(0);
}

#[tokio::test]
async fn whitelist_create_attaches_the_configured_beneficiary() {
    let harness = Harness::start().await;
    let create = harness.alpaca.mock(|when, then| {
        when.method(POST).path(whitelist_path()).json_body_includes(
            json!({
                "address": OTHER_WALLET,
                "asset": "USDC",
                "travel_rule_info": {
                    "beneficiary_is_self_hosted": true,
                    "beneficiary_entity_name": "T0 Trade Ltd"
                }
            })
            .to_string(),
        );
        then.status(200)
            .json_body(whitelist_entry("wl-new", OTHER_WALLET, "PENDING"));
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/entries",
            Some(json!({
                "address": OTHER_WALLET,
                "asset": "USDC",
                "network": "ethereum",
                "operationId": "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b",
                "reason": "new treasury wallet"
            })),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    create.assert_calls(1);
    assert_eq!(body["id"], "wl-new");
    assert_eq!(body["status"], "PENDING");
    let events = harness.audit_events();
    assert_eq!(
        answered(&events).unwrap().alpaca_object_id.as_deref(),
        Some("wl-new")
    );
}

#[tokio::test]
async fn whitelist_create_refuses_travel_rule_fields_from_the_request() {
    let harness = Harness::start().await;
    let create = harness.alpaca.mock(|when, then| {
        when.method(POST).path(whitelist_path());
        then.status(200)
            .json_body(whitelist_entry("wl-new", OTHER_WALLET, "PENDING"));
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/entries",
            Some(json!({
                "address": OTHER_WALLET,
                "asset": "USDC",
                "network": "ethereum",
                "operationId": "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b",
                "reason": "new treasury wallet",
                "travelRuleInfo": { "beneficiaryEntityName": "Somebody Else" }
            })),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["code"],
        serde_json::to_value(ErrorCode::InvalidRequest).unwrap()
    );
    create.assert_calls(0);
}
