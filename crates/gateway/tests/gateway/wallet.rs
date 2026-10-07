//! Withdrawals with their pinned destinations, transfer reads, the
//! whitelist and journals.

use std::time::Duration;

use axum::http::{HeaderValue, StatusCode};
use httpmock::prelude::*;
use serde_json::{Value, json};
use st0x_alpaca_gateway_api::{
    AuditEvent, AuditPhase, ErrorCode, ON_BEHALF_OF_HEADER, Operation, Outcome, Tier,
};

use crate::common::{
    ACCOUNT_ID, BOT_WALLET, Harness, JOURNAL_COUNTERPARTY, MARKET_MAKER_WALLET,
    assert_read_audited, authorized,
};

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

/// The `X-Request-ID` Alpaca answers the whitelist read with.
const WHITELIST_REQUEST_ID: &str = "req-whitelist";

/// Serves the whitelist with every configured address approved.
fn serve_approved_whitelist(harness: &Harness) {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200)
            .header("x-request-id", WHITELIST_REQUEST_ID)
            .json_body(json!([
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

fn settled(events: &[AuditEvent]) -> Option<&AuditEvent> {
    events
        .iter()
        .find(|event| event.phase == AuditPhase::Settled)
}

async fn withdraw(harness: &Harness, tier: Tier, address: &str) -> (StatusCode, Value) {
    harness
        .call(
            tier,
            "POST",
            "/wallet/withdrawals",
            Some(withdrawal(address)),
        )
        .await
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
            .header("x-request-id", "req-withdrawal")
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (status, body) = withdraw(&harness, Tier::Bot, MARKET_MAKER_WALLET).await;

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
    // The whitelist read, then the transfer POST, in the order sent.
    assert_eq!(
        event.alpaca_request_ids,
        [WHITELIST_REQUEST_ID, "req-withdrawal"]
    );
}

/// The bot withdraws only to its pinned destinations, a human never to
/// them, and nobody to an address whose whitelist entry awaits approval;
/// each refusal comes before the transfer is sent.
#[tokio::test]
async fn a_withdrawal_outside_the_caller_lane_is_refused_before_sending() {
    for (tier, address, whitelist_status, status, reason) in [
        (
            Tier::Bot,
            OTHER_WALLET,
            "APPROVED",
            StatusCode::FORBIDDEN,
            "destination_not_allowed",
        ),
        (
            Tier::Write,
            MARKET_MAKER_WALLET,
            "APPROVED",
            StatusCode::FORBIDDEN,
            "destination_not_allowed",
        ),
        (
            Tier::Write,
            OTHER_WALLET,
            "PENDING",
            StatusCode::UNPROCESSABLE_ENTITY,
            "address_not_whitelisted",
        ),
    ] {
        let harness = Harness::start().await;
        harness.alpaca.mock(|when, then| {
            when.method(GET).path(whitelist_path());
            then.status(200)
                .json_body(json!([whitelist_entry("wl", address, whitelist_status)]));
        });
        let post = harness.alpaca.mock(|when, then| {
            when.method(POST).path(transfers_path());
            then.status(200).json_body(transfer_json(address));
        });

        let (got, body) = withdraw(&harness, tier, address).await;

        assert_eq!(got, status, "{tier:?} to {address}: {body}");
        assert_eq!(body["reason"], reason, "{tier:?} to {address}");
        assert_eq!(body["outcome"], "not_applied", "{tier:?} to {address}");
        post.assert_calls(0);
        assert_eq!(
            answered(&harness.audit_events()).unwrap().outcome,
            Some(Outcome::NotApplied)
        );
    }
}

/// Every audit record stays one bounded log line whatever a caller sends:
/// an oversized counterparty, asset or network fails its wire bound and
/// never reaches the record, and the reason and `X-On-Behalf-Of`, which
/// have no wire bound, keep only their first 256 characters.
#[tokio::test]
async fn oversized_caller_values_never_grow_an_audit_record() {
    let harness = Harness::start().await;
    // Long enough to dwarf every bound, short enough for one URI.
    let oversized = "a".repeat(50_000);
    let journal = json!({
        "counterparty": oversized,
        "symbol": "AAPL",
        "qty": "5",
        "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
        "reason": "return shares to the issuer"
    });
    let mut withdrawal_of_oversized_asset = withdrawal(OTHER_WALLET);
    withdrawal_of_oversized_asset["asset"] = json!(oversized);

    for (method, path, body) in [
        ("POST", "/journals".to_string(), Some(journal)),
        (
            "POST",
            "/wallet/withdrawals".to_string(),
            Some(withdrawal_of_oversized_asset),
        ),
        (
            "GET",
            format!("/wallet/deposit-address?asset=USDC&network={oversized}"),
            None,
        ),
    ] {
        let (status, body) = harness.call(Tier::Write, method, &path, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }

    let mut overlong_reason = withdrawal(MARKET_MAKER_WALLET);
    overlong_reason["reason"] = json!(format!("{}{oversized}", "é".repeat(256)));
    let mut request = authorized(
        Tier::Write,
        "POST",
        &format!("{}/wallet/withdrawals", Tier::Write.prefix()),
        Some(overlong_reason),
    );
    request.headers_mut().insert(
        ON_BEHALF_OF_HEADER,
        HeaderValue::from_str(&format!("{}{oversized}", "o".repeat(256))).unwrap(),
    );
    let (status, body) = harness.send(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let events = harness.audit_events();
    assert_eq!(events.len(), 4, "{events:?}");
    for event in &events {
        let line = serde_json::to_string(event).unwrap();
        assert!(line.len() < 4096, "a {} byte record", line.len());
    }
    let human = events.last().unwrap();
    assert_eq!(human.reason.as_deref(), Some("é".repeat(256).as_str()));
    assert_eq!(
        human.on_behalf_of.as_deref(),
        Some("o".repeat(256).as_str())
    );
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

    let (status, body) = withdraw(&harness, Tier::Bot, MARKET_MAKER_WALLET).await;

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

    let (status, body) = withdraw(&harness, Tier::Bot, MARKET_MAKER_WALLET).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["retryableWithSameKey"], false);

    harness.settle().await;
    let events = harness.audit_events();
    let answer = answered(&events).unwrap();
    assert_eq!(answer.outcome, Some(Outcome::Unknown), "{events:?}");
    assert!(answer.alpaca_request_ids.is_empty(), "{events:?}");
    let settled = settled(&events).unwrap();
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

    let (status, body) = withdraw(&harness, Tier::Bot, MARKET_MAKER_WALLET).await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");

    harness.settle().await;
    post.assert_calls(1);
    let events = harness.audit_events();
    let settled = settled(&events).unwrap();
    assert_eq!(settled.outcome, Some(Outcome::Unknown));
    assert_eq!(settled.code, Some(ErrorCode::OutcomeUnknown));
    assert_eq!(settled.alpaca_status, Some(500));
    assert_eq!(settled.alpaca_object_id, None);
}

/// The answer and the cut of the whitelist read fall on the same instant,
/// so the caller hears either `outcome_unknown` with a `settled` record
/// after it, or `not_applied` at once; either way nothing was sent.
#[tokio::test]
async fn a_withdrawal_whose_whitelist_read_outlasts_its_deadline_is_never_sent() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200)
            .delay(Duration::from_secs(30))
            .json_body(json!([whitelist_entry(
                "wl-mm",
                MARKET_MAKER_WALLET,
                "APPROVED"
            )]));
    });
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(transfers_path());
        then.status(200)
            .json_body(transfer_json(MARKET_MAKER_WALLET));
    });

    let (_, body) = withdraw(&harness, Tier::Bot, MARKET_MAKER_WALLET).await;
    assert!(
        matches!(body["outcome"].as_str(), Some("unknown" | "not_applied")),
        "{body}"
    );

    // The cut read ends the detached work long before the read would answer.
    tokio::time::timeout(Duration::from_secs(5), harness.settle())
        .await
        .unwrap();
    post.assert_calls(0);
    let events = harness.audit_events();
    let last = events.last().unwrap();
    assert_eq!(last.outcome, Some(Outcome::NotApplied), "{events:?}");
    assert_eq!(last.alpaca_object_id, None);
}

const SETTLED_TX: &str = "0xabababababababababababababababababababababababababababababababab";

/// An incoming transfer in a status the library does not know, listed
/// before the deposit: only the hash, not the direction, tells the scan to
/// skip it.
#[tokio::test]
async fn a_deposit_lookup_by_hash_finds_its_deposit_past_an_unrelated_malformed_row() {
    let harness = Harness::start().await;
    let mut deposit = transfer_json(MARKET_MAKER_WALLET);
    deposit["direction"] = json!("INCOMING");
    deposit["tx_hash"] = json!(SETTLED_TX);
    deposit["status"] = json!("COMPLETE");
    let mut malformed = transfer_json(OTHER_WALLET);
    malformed["id"] = json!("5c0f7d1e-2b3a-4c5d-8e6f-7a8b9c0d1e2f");
    malformed["direction"] = json!("INCOMING");
    malformed["tx_hash"] = json!(format!("0x{}", "cd".repeat(32)));
    malformed["status"] = json!("QUEUED_FOR_REVIEW");
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(transfers_path());
        then.status(200).json_body(json!([malformed, deposit]));
    });

    let (status, body) = harness
        .call(
            Tier::Bot,
            "GET",
            &format!("/wallet/deposits/by-tx/{SETTLED_TX}"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deposit"]["id"], TRANSFER_ID);
    assert_eq!(body["deposit"]["tx_hash"], SETTLED_TX);
    let events = harness.audit_events();
    assert_eq!(answered(&events).unwrap().key.as_deref(), Some(SETTLED_TX));
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

/// The Travel Rule beneficiary comes from config only: the create attaches
/// it, and a request carrying its own is refused.
#[tokio::test]
async fn whitelist_create_attaches_the_configured_beneficiary_and_refuses_one_from_the_request() {
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
    let mut request = json!({
        "address": OTHER_WALLET,
        "asset": "USDC",
        "operationId": "9e8d7c6b-5a4f-4e3d-8c2b-1a0f9e8d7c6b",
        "reason": "new treasury wallet"
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/entries",
            Some(request.clone()),
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

    request["travelRuleInfo"] = json!({ "beneficiaryEntityName": "Somebody Else" });
    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/entries",
            Some(request),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    create.assert_calls(1);
}

/// Serves the whitelist under [`WHITELIST_REQUEST_ID`]: two entries for
/// [`OTHER_WALLET`] and one for the market maker wallet.
fn serve_whitelist_of_two(harness: &Harness) -> httpmock::Mock<'_> {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(whitelist_path());
        then.status(200)
            .header("x-request-id", WHITELIST_REQUEST_ID)
            .json_body(json!([
                whitelist_entry("wl-a", OTHER_WALLET, "APPROVED"),
                whitelist_entry("wl-mm", MARKET_MAKER_WALLET, "APPROVED"),
                whitelist_entry("wl-b", OTHER_WALLET, "PENDING"),
            ]));
    })
}

/// The read an operator reconciles a partly applied whitelist loop from
/// answers every entry Alpaca reports.
#[tokio::test]
async fn a_reader_lists_every_whitelist_entry_and_the_read_is_audited() {
    let harness = Harness::start().await;
    let list = serve_whitelist_of_two(&harness);

    let (status, body) = harness
        .call(Tier::Read, "GET", "/wallet/whitelist", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let entries: Vec<(&str, &str)> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["id"].as_str().unwrap(),
                entry["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("wl-a", "APPROVED"),
            ("wl-mm", "APPROVED"),
            ("wl-b", "PENDING")
        ]
    );
    list.assert_calls(1);
    assert_read_audited(
        &harness,
        Operation::WalletWhitelist,
        Tier::Read,
        None,
        &[WHITELIST_REQUEST_ID],
    );
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
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("whitelist entries wl-a were already changed"),
        "{body}"
    );
    first.assert_calls(1);
    second.assert_calls(1);
    let events = harness.audit_events();
    let event = answered(&events).unwrap();
    assert_eq!(event.outcome, Some(Outcome::Unknown));
    assert_eq!(event.alpaca_object_id.as_deref(), Some("wl-a"));
}

/// The first delete answers after the deadline; the send gate, closed at
/// the answer, holds the second back, and the settled record names the
/// entry the loop changed.
#[tokio::test]
async fn whitelist_remove_sends_no_delete_once_its_deadline_passed() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    serve_whitelist_of_two(&harness);
    let path = entry_path("wl-a");
    let first = harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(path);
        then.status(200)
            .delay(Duration::from_secs(1))
            .json_body(json!({}));
    });
    let second = answer_delete(&harness, "wl-b", 200);

    let (status, body) = remove_other_wallet(&harness).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["code"], "outcome_unknown");

    harness.settle().await;
    first.assert_calls(1);
    second.assert_calls(0);
    let events = harness.audit_events();
    let settled = settled(&events).unwrap();
    assert_eq!(settled.outcome, Some(Outcome::Unknown));
    assert_eq!(settled.alpaca_object_id.as_deref(), Some("wl-a"));
}

#[tokio::test]
async fn travel_rule_patch_sends_the_configured_beneficiary_to_every_entry() {
    let harness = Harness::start().await;
    serve_whitelist_of_two(&harness);
    let patches = ["wl-a", "wl-mm", "wl-b"].map(|id| {
        let path = format!("{}/travel-rule-info", entry_path(id));
        harness.alpaca.mock(|when, then| {
            when.method(PATCH).path(path).json_body(json!({
                "travel_rule_info": {
                    "beneficiary_is_self_hosted": true,
                    "beneficiary_entity_name": "T0 Trade Ltd"
                }
            }));
            then.status(200).json_body(json!({}));
        })
    });

    let (status, body) = harness
        .call(
            Tier::Write,
            "POST",
            "/wallet/whitelist/travel-rule",
            Some(json!({
                "operationId": "2a3b4c5d-6e7f-4809-9a1b-2c3d4e5f6a7b",
                "reason": "attach the beneficiary"
            })),
        )
        .await;

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
