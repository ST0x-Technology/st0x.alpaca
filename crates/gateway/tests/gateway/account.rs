//! The position mark read, and `activities.list` with its per tier page cap.

use axum::http::StatusCode;
use httpmock::prelude::*;
use serde_json::json;
use st0x_alpaca_gateway_api::{Operation, Tier};

use crate::common::{
    ACCOUNT_ID, ACTIVITY_PAGE_SIZE, Harness, activity_id, assert_read_audited, serve_activity_pages,
};

const POSITION_REQUEST_ID: &str = "req-position";

#[tokio::test]
async fn the_bot_reads_the_mark_of_a_held_position_and_the_read_is_audited() {
    let harness = Harness::start().await;
    let position = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/positions/AAPL"));
        then.status(200)
            .header("x-request-id", POSITION_REQUEST_ID)
            .json_body(json!({
                "symbol": "AAPL",
                "asset_class": "us_equity",
                "exchange": "NASDAQ",
                "qty_available": "5",
                "qty": "5",
                "market_value": "936.25",
                "current_price": "187.25"
            }));
    });

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/account/positions/AAPL/mark", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "mark": "187.25" }));
    position.assert_calls(1);
    assert_read_audited(
        &harness,
        Operation::AccountPositionMark,
        Tier::Bot,
        Some("AAPL"),
        &[POSITION_REQUEST_ID],
    );
}

/// Eleven full pages: a human reads ten and is asked for a narrower window,
/// the bot reads them all.
#[tokio::test]
async fn activities_list_reads_ten_pages_for_a_human_and_every_page_for_the_bot() {
    let harness = Harness::start().await;
    let pages = serve_activity_pages(&harness, 11);

    let (status, body) = harness
        .call(Tier::Read, "GET", "/activities?types=FEE", None)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert!(
        body["message"].as_str().unwrap().contains("narrow"),
        "{body}"
    );
    for page in &pages[..10] {
        page.assert_calls(1);
    }
    for page in &pages[10..] {
        page.assert_calls(0);
    }

    let (status, body) = harness
        .call(Tier::Bot, "GET", "/activities?types=FEE", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let activities = body["activities"].as_array().unwrap();
    assert_eq!(activities.len(), 11 * ACTIVITY_PAGE_SIZE);
    assert_eq!(activities[0]["id"], activity_id(0, 0));
    assert_eq!(
        activities[11 * ACTIVITY_PAGE_SIZE - 1]["id"],
        activity_id(10, ACTIVITY_PAGE_SIZE - 1)
    );
    for page in &pages[..10] {
        page.assert_calls(2);
    }
    for page in &pages[10..] {
        page.assert_calls(1);
    }
}
