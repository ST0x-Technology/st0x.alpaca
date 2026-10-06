//! The typed client against the real router over HTTP: each method's request
//! and response types must match what the gateway serves.

use httpmock::prelude::*;
use serde_json::json;
use st0x_alpaca::broker::ClientOrderId;
use st0x_alpaca::core::Network;
use st0x_alpaca::tokenization::IssuerRequestId;
use st0x_alpaca_gateway_api::client::ClientError;
use st0x_alpaca_gateway_api::dto::orders::{MarketOrderRequest, OrderStateResponse};
use st0x_alpaca_gateway_api::dto::wallet::WithdrawRequest;
use st0x_alpaca_gateway_api::{ErrorCode, Outcome};
use uuid::Uuid;

use crate::common::{ACCOUNT_ID, BOT_WALLET, Harness};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";
const CLIENT_UUID: &str = "66666666-6666-4666-8666-666666666666";
const ISSUER_REQUEST_ID: &str = "6b1d0a7e-3f2c-4c55-9a4e-2f1b9b2c7d10";

fn alpaca_order(status: &str) -> serde_json::Value {
    json!({
        "id": ORDER_ID,
        "client_order_id": CLIENT_UUID,
        "symbol": "AAPL",
        "qty": "7",
        "side": "buy",
        "status": status,
        "created_at": "2026-09-17T10:15:29Z",
        "filled_qty": "0",
        "filled_avg_price": null
    })
}

#[tokio::test]
async fn account_reads_decode_into_their_types() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/positions"));
        then.status(200).json_body(json!([]));
    });
    let client = harness.bot_client().await;

    let funds = client.account_funds().await.unwrap();
    assert_eq!(funds.balance_cents, 150_025);
    assert_eq!(funds.withdrawable_cents, Some(100_010));

    let inventory = client.inventory().await.unwrap();
    assert!(inventory.positions.is_empty());
    assert_eq!(inventory.usd_balance_cents, 150_025);
}

#[tokio::test]
async fn an_order_placed_through_the_client_reads_back_by_id_and_by_key() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": true
        }));
    });
    harness.alpaca.mock(|when, then| {
        when.method(POST)
            .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/orders"));
        then.status(200).json_body(alpaca_order("new"));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(format!(
            "/v1/trading/accounts/{ACCOUNT_ID}/orders/{ORDER_ID}"
        ));
        then.status(200).json_body(alpaca_order("new"));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!(
                "/v1/trading/accounts/{ACCOUNT_ID}/orders:by_client_order_id"
            ))
            .query_param("client_order_id", CLIENT_UUID);
        then.status(200).json_body(alpaca_order("new"));
    });
    let client = harness.bot_client().await;
    let key = ClientOrderId::from_uuid(CLIENT_UUID.parse().unwrap());
    let order: MarketOrderRequest = serde_json::from_value(json!({
        "symbol": "AAPL",
        "shares": "7",
        "direction": "buy",
        "clientOrderId": CLIENT_UUID
    }))
    .unwrap();

    let placement = client.place_market_order(&order).await.unwrap();
    assert_eq!(placement.order_id, ORDER_ID);

    let state = client.order(ORDER_ID.parse().unwrap()).await.unwrap();
    assert!(
        matches!(state, OrderStateResponse::Submitted { .. }),
        "{state:?}"
    );

    let found = client.find_order(&key).await.unwrap();
    assert_eq!(
        found.order.map(|order| order.order_id).as_deref(),
        Some(ORDER_ID)
    );
}

#[tokio::test]
async fn a_refusal_comes_back_as_the_typed_error_body() {
    let harness = Harness::start().await;
    let client = harness.bot_client().await;
    let withdrawal: WithdrawRequest = serde_json::from_value(json!({
        "amount": "10",
        "asset": "USDC",
        "address": "0x3333333333333333333333333333333333333333",
        "operationId": Uuid::new_v4(),
    }))
    .unwrap();

    let error = client.withdraw(&withdrawal).await.unwrap_err();

    let ClientError::Gateway { status, body } = error else {
        panic!("expected a gateway answer, got {error}");
    };
    assert_eq!(status, 403);
    assert_eq!(body.code, ErrorCode::Forbidden);
    assert_eq!(body.outcome, Some(Outcome::NotApplied));
}

#[tokio::test]
async fn acting_for_a_human_names_them_in_the_audit_record() {
    let harness = Harness::start().await;
    let client = harness
        .bot_client()
        .await
        .acting_for("operator@t0trade.com");

    client.account_funds().await.unwrap();

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].on_behalf_of.as_deref(),
        Some("operator@t0trade.com")
    );
}

#[tokio::test]
async fn tokenization_lookups_send_their_network_and_decode() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests"))
            .query_param("type", "mint");
        then.status(200).json_body(json!([{
            "tokenization_request_id": "tok_req_1",
            "type": "mint",
            "status": "completed",
            "underlying_symbol": "AAPL",
            "token_symbol": "tAAPL",
            "qty": "2.5",
            "issuer": "st0x",
            "network": "base",
            "wallet_address": BOT_WALLET,
            "client_request_id": ISSUER_REQUEST_ID,
            "created_at": "2026-10-05T10:30:00Z"
        }]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests"))
            .query_param("type", "redeem");
        then.status(200).json_body(json!([]));
    });
    let client = harness.bot_client().await;
    let issuer_request_id = IssuerRequestId(ISSUER_REQUEST_ID.parse().unwrap());

    let mint = client.find_mint(&issuer_request_id).await.unwrap();
    assert_eq!(
        mint.request
            .map(|request| request.id.to_string())
            .as_deref(),
        Some("tok_req_1")
    );

    let redemption = client
        .find_redemption(&alloy_primitives::TxHash::repeat_byte(0xab), Network::Base)
        .await
        .unwrap();
    assert!(redemption.request.is_none());
}
