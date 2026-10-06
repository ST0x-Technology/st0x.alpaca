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
        when.method(POST).path(transfers_path()).json_body(json!({
            "amount": "250.5",
            "asset": "USDC",
            "address": MARKET_MAKER_WALLET
        }));
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
async fn a_withdrawal_that_fails_after_its_deadline_settles_outcome_unknown_with_the_status() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    serve_approved_whitelist(&harness);
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(500)
            .delay(Duration::from_secs(1))
            .json_body(json!({ "message": "internal error" }));
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

    harness.settle().await;
    post.assert_calls(1);
    let events = harness.audit_events();
    let settled = events
        .iter()
        .find(|event| event.phase == AuditPhase::Settled)
        .unwrap();
    assert_eq!(settled.outcome, Some(Outcome::Unknown));
    assert_eq!(settled.code, Some(ErrorCode::OutcomeUnknown));
    assert_eq!(settled.alpaca_status, Some(500));
    assert_eq!(settled.alpaca_object_id, None);
}

const SETTLED_TX: &str = "0xabababababababababababababababababababababababababababababababab";

/// The transfer list: a withdrawal settled under [`SETTLED_TX`] after a
/// withdrawal under `tx_hash` in a status the library does not know.
fn serve_transfers_with_a_malformed_row(harness: &Harness, tx_hash: Value) -> httpmock::Mock<'_> {
    let mut settled = transfer_json(MARKET_MAKER_WALLET);
    settled["tx_hash"] = json!(SETTLED_TX);
    settled["status"] = json!("COMPLETE");
    let mut malformed = transfer_json(OTHER_WALLET);
    malformed["id"] = json!("5c0f7d1e-2b3a-4c5d-8e6f-7a8b9c0d1e2f");
    malformed["tx_hash"] = tx_hash;
    malformed["status"] = json!("QUEUED_FOR_REVIEW");
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(transfers_path());
        then.status(200).json_body(json!([malformed, settled]));
    })
}

#[tokio::test]
async fn a_transfer_lookup_by_hash_finds_its_transfer_past_an_unrelated_malformed_row() {
    let harness = Harness::start().await;
    serve_transfers_with_a_malformed_row(&harness, json!(format!("0x{}", "cd".repeat(32))));

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/wallet/transfers/by-tx/{SETTLED_TX}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["transfer"]["id"], TRANSFER_ID);
    assert_eq!(body["transfer"]["txHash"], SETTLED_TX);
    let events = harness.audit_events();
    assert_eq!(answered(&events).unwrap().key.as_deref(), Some(SETTLED_TX));
}

#[tokio::test]
async fn an_unreadable_pending_withdrawal_fails_the_transfer_list_as_it_fails_the_direct_one() {
    let harness = Harness::start().await;
    serve_transfers_with_a_malformed_row(&harness, Value::Null);

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/wallet/transfers", None)
        .await;

    // Dropping the row would let a reconciliation miss a withdrawal.
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_transient");
    assert!(harness.state.wallet.list_all_transfers().await.is_err());
}

fn deposit_address_path() -> String {
    format!("/v1/accounts/{ACCOUNT_ID}/wallets")
}

fn deposit_address_json() -> Value {
    json!({
        "asset_id": "5d0de74f-827b-41a7-9f74-9c07c08fe55f",
        "address": BOT_WALLET,
        "created_at": "2026-10-01T12:00:00Z"
    })
}

#[tokio::test]
async fn a_deposit_address_lookup_sends_only_the_asset_and_network_asked_for() {
    let harness = Harness::start().await;
    let mut any_lookup = harness.alpaca.mock(|when, then| {
        when.method(GET).path(deposit_address_path());
        then.status(200).json_body(deposit_address_json());
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/wallet/deposit-address?asset=USDC%26account_id%3Dx&network=ethereum",
            None,
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    any_lookup.assert_calls(0);
    any_lookup.delete();

    let lookup = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(deposit_address_path())
            .query_param("asset", "USDC")
            .query_param("network", "ethereum");
        then.status(200).json_body(deposit_address_json());
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            "/wallet/deposit-address?asset=USDC&network=ethereum",
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["address"].as_str().unwrap().to_lowercase(), BOT_WALLET);
    lookup.assert_calls(1);
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
                "operationId": "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b",
                "reason": "new treasury wallet"
            })),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    create.assert_calls(1);
    assert_eq!(body["id"], "wl-new");
    assert_eq!(body["status"], "pending");
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

/// The whitelist with two entries for [`OTHER_WALLET`] and one for the
/// market maker wallet.
fn serve_whitelist_of_two(harness: &Harness) -> httpmock::Mock<'_> {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200).json_body(json!([
            whitelist_entry("wl-a", OTHER_WALLET, "APPROVED"),
            whitelist_entry("wl-mm", MARKET_MAKER_WALLET, "APPROVED"),
            whitelist_entry("wl-b", OTHER_WALLET, "PENDING"),
        ]));
    })
}

fn entry_path(id: &str) -> String {
    format!("{}/{id}", whitelist_path())
}

fn answer_delete<'a>(harness: &'a Harness, id: &str, status: u16) -> httpmock::Mock<'a> {
    let path = entry_path(id);
    harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(path);
        then.status(status).json_body(json!({}));
    })
}

async fn remove_other_wallet(harness: &Harness) -> (StatusCode, Value) {
    harness
        .call(
            Tier::Write,
            "POST",
            &format!("/wallet/whitelist/{OTHER_WALLET}/remove"),
            Some(json!({
                "operationId": "1f2e3d4c-5b6a-4978-8a9b-0c1d2e3f4a5b",
                "reason": "retire the treasury wallet"
            })),
        )
        .await
}

#[tokio::test]
async fn whitelist_remove_deletes_every_entry_of_the_address_and_audits_their_ids() {
    let harness = Harness::start().await;
    serve_whitelist_of_two(&harness);
    let first = answer_delete(&harness, "wl-a", 200);
    let second = answer_delete(&harness, "wl-b", 200);
    let other = answer_delete(&harness, "wl-mm", 200);

    let (status, body) = remove_other_wallet(&harness).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["wl-a", "wl-b"]);
    first.assert_calls(1);
    second.assert_calls(1);
    other.assert_calls(0);
    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.operation, Operation::WalletWhitelistRemove);
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("wl-a,wl-b"));
}

#[tokio::test]
async fn whitelist_remove_failing_after_one_delete_is_outcome_unknown_naming_the_deleted_entry() {
    let harness = Harness::start().await;
    serve_whitelist_of_two(&harness);
    let first = answer_delete(&harness, "wl-a", 200);
    // A definite 404 on its own would be `rejected`, but wl-a is gone.
    let second = answer_delete(&harness, "wl-b", 404);

    let (status, body) = remove_other_wallet(&harness).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["retryableWithSameKey"], true);
    assert_eq!(body["alpacaObjectIds"], json!(["wl-a"]));
    first.assert_calls(1);
    second.assert_calls(1);
    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("wl-a"));
}

#[tokio::test]
async fn whitelist_remove_failing_after_its_deadline_settles_naming_the_deleted_entry() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    serve_whitelist_of_two(&harness);
    answer_delete(&harness, "wl-a", 200);
    let path = entry_path("wl-b");
    harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(path);
        then.status(500)
            .delay(Duration::from_secs(1))
            .json_body(json!({ "message": "internal error" }));
    });

    let (status, body) = remove_other_wallet(&harness).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");

    harness.settle().await;
    let events = harness.audit_events();
    let settled = events
        .iter()
        .find(|event| event.phase == AuditPhase::Settled)
        .unwrap();
    assert_eq!(settled.outcome, Some(Outcome::Unknown));
    assert_eq!(settled.alpaca_status, Some(500));
    assert_eq!(settled.alpaca_object_id.as_deref(), Some("wl-a"));
}

#[tokio::test]
async fn whitelist_remove_with_a_failed_whitelist_read_is_not_applied() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(503).body("unavailable");
    });
    let delete = harness.alpaca.mock(|when, then| {
        when.method(DELETE);
        then.status(200);
    });

    let (status, body) = remove_other_wallet(&harness).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["code"], "upstream_transient");
    assert_eq!(body["outcome"], "not_applied");
    delete.assert_calls(0);
    let events = harness.audit_events();
    assert_eq!(
        answered(&events).unwrap().outcome,
        Some(Outcome::NotApplied)
    );
}

fn answer_patch<'a>(harness: &'a Harness, id: &str, status: u16) -> httpmock::Mock<'a> {
    let path = format!("{}/travel-rule-info", entry_path(id));
    harness.alpaca.mock(|when, then| {
        when.method(PATCH).path(path).json_body(json!({
            "travel_rule_info": {
                "beneficiary_is_self_hosted": true,
                "beneficiary_entity_name": "T0 Trade Ltd"
            }
        }));
        then.status(status).json_body(json!({}));
    })
}

fn patch_travel_rule() -> Value {
    json!({
        "operationId": "2a3b4c5d-6e7f-4809-9a1b-2c3d4e5f6a7b",
        "reason": "attach the beneficiary"
    })
}

async fn send_travel_rule_patch(harness: &Harness) -> (StatusCode, Value) {
    harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/travel-rule",
            Some(patch_travel_rule()),
        )
        .await
}

#[tokio::test]
async fn a_travel_rule_patch_the_remaining_budget_cannot_cover_is_refused_with_nothing_written() {
    // One unit for the read and three for the writes fit twice; after its
    // read the third patch has one unit left, enough for one of its three
    // writes but not all of them.
    let harness = Harness::start_with("human_budget_per_minute = 10", "").await;
    serve_whitelist_of_two(&harness);
    let patches = [
        answer_patch(&harness, "wl-a", 200),
        answer_patch(&harness, "wl-mm", 200),
        answer_patch(&harness, "wl-b", 200),
    ];

    for _ in 0..2 {
        let (status, body) = send_travel_rule_patch(&harness).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let (status, body) = send_travel_rule_patch(&harness).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["retryable"], true);
    assert!(body["retryAfterSecs"].as_u64().unwrap() >= 1, "{body}");
    assert!(body["alpacaStatus"].is_null(), "{body}");
    for patch in &patches {
        patch.assert_calls(2);
    }
    let events = harness.audit_events();
    assert_eq!(events.last().unwrap().outcome, Some(Outcome::NotApplied));
}

#[tokio::test]
async fn a_travel_rule_patch_longer_than_a_minute_of_budget_is_refused_as_never_admissible() {
    // The smallest budget config accepts, against ten entries: ten writes
    // and the read never fit in ten units.
    let harness = Harness::start_with("human_budget_per_minute = 10", "").await;
    let entries: Vec<Value> = (0..10)
        .map(|entry| whitelist_entry(&format!("wl-{entry}"), OTHER_WALLET, "APPROVED"))
        .collect();
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200).json_body(json!(entries));
    });
    let patch = harness.alpaca.mock(|when, then| {
        when.method(PATCH);
        then.status(200).json_body(json!({}));
    });

    let (status, body) = send_travel_rule_patch(&harness).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "backpressure");
    assert_eq!(body["outcome"], "not_applied");
    assert_eq!(body["retryable"], false);
    assert!(body["retryAfterSecs"].is_null(), "{body}");
    patch.assert_calls(0);
}

#[tokio::test]
async fn travel_rule_patch_sends_the_configured_beneficiary_to_every_entry() {
    let harness = Harness::start().await;
    serve_whitelist_of_two(&harness);
    let patches = [
        answer_patch(&harness, "wl-a", 200),
        answer_patch(&harness, "wl-mm", 200),
        answer_patch(&harness, "wl-b", 200),
    ];

    let (status, body) = send_travel_rule_patch(&harness).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["entries"].as_array().unwrap().len(), 3);
    for patch in &patches {
        patch.assert_calls(1);
    }
    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.outcome, Some(Outcome::Applied));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("wl-a,wl-mm,wl-b"));
    let audit = serde_json::to_string(event).unwrap();
    assert!(!audit.contains("T0 Trade Ltd"), "{audit}");
}

#[tokio::test]
async fn travel_rule_patch_failing_after_one_patch_is_outcome_unknown() {
    let harness = Harness::start().await;
    serve_whitelist_of_two(&harness);
    let first = answer_patch(&harness, "wl-a", 200);
    let second = answer_patch(&harness, "wl-mm", 404);
    let third = answer_patch(&harness, "wl-b", 200);

    let (status, body) = send_travel_rule_patch(&harness).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["retryableWithSameKey"], true);
    assert_eq!(body["alpacaObjectIds"], json!(["wl-a"]));
    first.assert_calls(1);
    second.assert_calls(1);
    third.assert_calls(0);
    let events = harness.audit_events();
    assert_eq!(answered(&events).unwrap().outcome, Some(Outcome::Unknown));
}

#[tokio::test]
async fn travel_rule_patch_with_a_failed_whitelist_read_is_not_applied() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(503).body("unavailable");
    });
    let patch = harness.alpaca.mock(|when, then| {
        when.method(PATCH);
        then.status(200);
    });

    let (status, body) = send_travel_rule_patch(&harness).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["outcome"], "not_applied");
    patch.assert_calls(0);
}
