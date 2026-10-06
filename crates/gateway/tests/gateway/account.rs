//! `activities.list` and its page cap.

use axum::http::StatusCode;
use st0x_alpaca_gateway_api::Tier;

use crate::common::{ACTIVITY_PAGE_SIZE, Harness, activity_id, serve_activity_pages};

#[tokio::test]
async fn a_human_activities_list_stops_at_its_page_cap_and_asks_for_a_narrower_window() {
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
    // One Alpaca request per page the admission paid for, none after.
    for page in &pages[..10] {
        page.assert_calls(1);
    }
    for page in &pages[10..] {
        page.assert_calls(0);
    }
}

#[tokio::test]
async fn the_bot_reads_a_history_longer_than_the_human_page_cap() {
    let harness = Harness::start().await;
    let pages = serve_activity_pages(&harness, 11);

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
    for page in &pages {
        page.assert_calls(1);
    }
}
