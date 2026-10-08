//! The client's library shaped adapters against the real router over HTTP:
//! each surface decodes what the gateway serves, and the library loops run
//! over it.

use std::time::Duration;

use httpmock::prelude::*;
use serde_json::json;
use st0x_alpaca::Permanence;
use st0x_alpaca::broker::{
    AlpacaBrokerApiError, AlpacaLimitOrder, AlpacaLimitPrice, ClientOrderId, ConversionOrder,
    ConversionOrders, CryptoOrderOutcome, Direction, MarketOrder, OrderState,
};
use st0x_alpaca::core::Network;
use st0x_alpaca::st0x_finance::Symbol;
use st0x_alpaca::tokenization::{IssuerRequestId, TokenizationLookups};
use st0x_alpaca::wallet::{
    AlpacaTransferId, AlpacaWalletError, PollingConfig, TokenSymbol, TransferStatus,
    poll_transfer_until_complete_with,
};
use st0x_alpaca_gateway_api::{AuditPhase, Operation, Outcome, Tier};
use uuid::Uuid;

use crate::common::{
    ACCOUNT_ID, BOT_WALLET, Harness, JOURNAL_COUNTERPARTY, MARKET_MAKER_WALLET, until_called,
};

const ORDER_ID: &str = "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f";
const CLIENT_UUID: &str = "66666666-6666-4666-8666-666666666666";
const ISSUER_REQUEST_ID: &str = "6b1d0a7e-3f2c-4c55-9a4e-2f1b9b2c7d10";
const TRANSFER_ID: &str = "0f8a4c62-1c3e-4a5b-9d7e-2b6f8c9d0e1a";
const JOURNAL_ID: &str = "9c1f2e3d-4b5a-4c6d-8e7f-a0b1c2d3e4f5";

fn orders_path() -> String {
    format!("/v1/trading/accounts/{ACCOUNT_ID}/orders")
}

fn alpaca_order(client_order_id: &str, qty: &str) -> serde_json::Value {
    json!({
        "id": ORDER_ID,
        "client_order_id": client_order_id,
        "symbol": "AAPL",
        "qty": qty,
        "side": "buy",
        "status": "new",
        "created_at": "2026-09-17T10:15:29Z",
        "filled_qty": "0",
        "filled_avg_price": null
    })
}

#[tokio::test]
async fn an_order_placed_through_the_broker_adapter_reads_back_by_id_and_by_key() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": true
        }));
    });
    let place = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(200).json_body(alpaca_order(CLIENT_UUID, "7"));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(200).json_body(alpaca_order(CLIENT_UUID, "7"));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!(
                "/v1/trading/accounts/{ACCOUNT_ID}/orders:by_client_order_id"
            ))
            .query_param("client_order_id", CLIENT_UUID);
        then.status(200).json_body(alpaca_order(CLIENT_UUID, "7"));
    });
    let broker = harness.bot_client().await.broker();
    let key = ClientOrderId::from_uuid(CLIENT_UUID.parse().unwrap());
    let order = MarketOrder {
        symbol: Symbol::new("AAPL").unwrap(),
        shares: serde_json::from_value(json!("7")).unwrap(),
        direction: Direction::Buy,
        client_order_id: key.clone(),
    };

    let placement = broker.place_market_order(order, None).await.unwrap();
    assert_eq!(placement.order_id, ORDER_ID);
    place.assert_calls(1);

    let state = broker.get_order_status(ORDER_ID).await.unwrap();
    assert!(
        matches!(&state, OrderState::Submitted { order_id } if order_id.to_string() == ORDER_ID),
        "{state:?}"
    );

    let found = broker.get_order_by_client_order_id(&key).await.unwrap();
    assert_eq!(found.map(|order| order.order_id).as_deref(), Some(ORDER_ID));
}

#[tokio::test]
async fn acting_for_a_human_names_them_in_the_audit_record() {
    let harness = Harness::start().await;
    let broker = harness
        .bot_client()
        .await
        .acting_for("operator@example.com")
        .broker();

    broker.account_funds().await.unwrap();

    let events = harness.audit_events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].on_behalf_of.as_deref(),
        Some("operator@example.com")
    );
}

#[tokio::test]
async fn tokenization_lookups_through_the_adapter_decode_what_the_gateway_answers() {
    let harness = Harness::start().await;
    let requests = format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests");
    harness.alpaca.mock(|when, then| {
        when.method(GET).path(&requests).query_param("type", "mint");
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
            .path(&requests)
            .query_param("type", "redeem");
        then.status(200).json_body(json!([]));
    });
    let tokenization = harness.bot_client().await.tokenization(Network::Base);

    let mint = tokenization
        .find_mint_by_issuer_request_id(&IssuerRequestId(ISSUER_REQUEST_ID.parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(
        mint.map(|request| request.id.to_string()).as_deref(),
        Some("tok_req_1")
    );

    let redemption = tokenization
        .find_redemption_by_tx(&alloy_primitives::TxHash::repeat_byte(0xab))
        .await
        .unwrap();
    assert!(redemption.is_none());
}

/// A USDC/USD conversion order as Alpaca reports it.
fn crypto_order(status: &str, filled_qty: &str) -> serde_json::Value {
    json!({
        "id": ORDER_ID,
        "client_order_id": CLIENT_UUID,
        "symbol": "USDCUSD",
        "qty": "100",
        "notional": null,
        "side": "sell",
        "status": status,
        "created_at": "2026-10-06T10:15:29Z",
        "filled_qty": filled_qty,
        "filled_avg_price": "0.9998"
    })
}

/// A conversion read the gateway answers `upstream_transient` (here the
/// gateway's own deadline on a stalled Alpaca read) is a retryable hop: the
/// adapter's conversion polls again, reads the fill, and never sends the
/// deadline cancel.
#[tokio::test]
async fn a_conversion_poll_reads_past_a_retryable_hop_without_cancelling() {
    let harness = Harness::start_with_deadline(Duration::from_millis(200)).await;
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(200).json_body(crypto_order("new", "0"));
    });
    let order_path = format!("{}/{ORDER_ID}", orders_path());
    let stalled = harness.alpaca.mock(|when, then| {
        when.method(GET).path(order_path.clone());
        then.status(200)
            .delay(Duration::from_secs(2))
            .json_body(crypto_order("filled", "100"));
    });
    let cancel = harness.alpaca.mock(|when, then| {
        when.method(DELETE).path(order_path.clone());
        then.status(204);
    });
    let broker = harness.bot_client().await.broker();
    let key = ClientOrderId::from_uuid(CLIENT_UUID.parse().unwrap());
    let quantity = serde_json::from_value(json!("100")).unwrap();

    let mut conversion = tokio::spawn(async move {
        broker
            .convert_usdc_usd(ConversionOrder::SellUsdc(quantity), &key, None)
            .await
    });
    until_called(&stalled, &mut conversion).await;
    stalled.delete_async().await;
    let filled = harness.alpaca.mock(|when, then| {
        when.method(GET).path(order_path.clone());
        then.status(200).json_body(crypto_order("filled", "100"));
    });
    let order = conversion.await.unwrap().unwrap();

    assert_eq!(order.classify(), CryptoOrderOutcome::Filled);
    assert_eq!(order.id.to_string(), ORDER_ID);
    post.assert_calls(1);
    filled.assert();
    cancel.assert_calls(0);
}

fn transfer_json(status: &str, tx_hash: Option<&str>) -> serde_json::Value {
    json!({
        "id": TRANSFER_ID,
        "tx_hash": tx_hash,
        "direction": "OUTGOING",
        "amount": "250.123456789",
        "chain": "ETH",
        "asset": "USDC",
        "from_address": BOT_WALLET,
        "to_address": "0x1111111111111111111111111111111111111111",
        "status": status,
        "created_at": "2026-10-06T09:30:00Z"
    })
}

#[tokio::test]
async fn a_withdrawal_is_polled_to_completion_by_the_library_loop_over_the_gateway() {
    let harness = Harness::start().await;
    let path = format!("/v1/accounts/{ACCOUNT_ID}/wallets/transfers/{TRANSFER_ID}");
    let processing = harness.alpaca.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .json_body(transfer_json("PROCESSING", None));
    });
    let wallet = harness.bot_client().await.wallet();
    let transfer_id = AlpacaTransferId::from(TRANSFER_ID.parse::<Uuid>().unwrap());
    let config = PollingConfig {
        interval: Duration::from_millis(20),
        timeout: Duration::from_secs(10),
        max_retries: 0,
        min_retry_delay: Duration::from_millis(10),
        max_retry_delay: Duration::from_millis(10),
    };

    let mut poll = tokio::spawn(async move {
        poll_transfer_until_complete_with(&wallet, &transfer_id, &config).await
    });
    until_called(&processing, &mut poll).await;
    processing.delete_async().await;
    let tx_hash = format!("0x{}", "ab".repeat(32));
    let complete = harness.alpaca.mock(|when, then| {
        when.method(GET).path(path.clone());
        then.status(200)
            .json_body(transfer_json("COMPLETE", Some(&tx_hash)));
    });
    let transfer = poll.await.unwrap().unwrap();

    assert_eq!(transfer.status, TransferStatus::Complete);
    assert_eq!(transfer.tx.map(|tx| tx.to_string()), Some(tx_hash));
    assert_eq!(transfer.id, transfer_id);
    assert_eq!(
        serde_json::to_value(transfer.amount).unwrap(),
        json!("250.123456789")
    );
    complete.assert();
}

/// The gateway refuses a bot withdrawal outside its pinned destinations
/// before sending anything: a definite refusal, not an outcome to reconcile
/// and not one to retry.
#[tokio::test]
async fn a_withdrawal_the_gateway_refuses_is_permanent_and_known_not_applied() {
    let harness = Harness::start().await;
    let wallet = harness.bot_client().await.wallet();

    let error = wallet
        .initiate_withdrawal(
            serde_json::from_value(json!("10")).unwrap(),
            &TokenSymbol::new("USDC"),
            &"0x3333333333333333333333333333333333333333"
                .parse()
                .unwrap(),
            Uuid::new_v4(),
            None,
        )
        .await
        .unwrap_err();

    let AlpacaWalletError::Gateway(hop) = &error else {
        panic!("expected a gateway refusal, got {error:?}");
    };
    assert_eq!(hop.permanence(), Permanence::Permanent);
    assert!(!hop.outcome_unknown, "{hop:?}");
}

/// A withdrawal Alpaca's whitelist has not approved comes back as the
/// direct path's `AddressNotWhitelisted`.
#[tokio::test]
async fn an_unapproved_withdrawal_address_comes_back_as_address_not_whitelisted() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/wallets/whitelists"));
        then.status(200).json_body(json!([]));
    });
    let wallet = harness.bot_client().await.wallet();
    let address = MARKET_MAKER_WALLET.parse().unwrap();

    let error = wallet
        .initiate_withdrawal(
            serde_json::from_value(json!("10")).unwrap(),
            &TokenSymbol::new("USDC"),
            &address,
            Uuid::new_v4(),
            None,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&error, AlpacaWalletError::AddressNotWhitelisted { address: named, .. } if *named == address),
        "{error:?}"
    );
}

/// Alpaca's 422 on a cancel comes back as its `ApiError`, which the
/// conversion poll reads as a declined deadline cancel, as on the direct
/// path.
#[tokio::test]
async fn a_cancel_alpaca_answers_422_comes_back_with_its_status() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(DELETE)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(422)
            .json_body(json!({ "message": "order is not cancelable" }));
    });
    let broker = harness.bot_client().await.broker();

    let error = ConversionOrders::cancel_order(&broker, ORDER_ID)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, AlpacaBrokerApiError::ApiError { status, .. } if status.as_u16() == 422),
        "{error:?}"
    );
}

/// Accepts exactly a 5 share journal to the issuer counterparty.
fn accept_journal(harness: &Harness) -> httpmock::Mock<'_> {
    harness.alpaca.mock(|when, then| {
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
    })
}

/// An operator tool reaches the write tier only operations through the
/// adapter: IAP in front forwards the operator's assertion, and the gateway,
/// which refuses a human mutation without a reason, applies each call and
/// records the reason it carried.
#[tokio::test]
async fn a_write_tier_adapter_places_an_exact_limit_order_and_a_journal_with_its_reason() {
    let harness = Harness::start().await;
    let cli_key = format!("cli-{CLIENT_UUID}");
    // Not fractionable, where only the exact limit order sends 3.5 shares.
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": false
        }));
    });
    let place = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path()).json_body(json!({
            "symbol": "AAPL",
            "qty": "3.5",
            "side": "buy",
            "type": "limit",
            "limit_price": "150.5",
            "time_in_force": "day",
            "extended_hours": false,
            "client_order_id": cli_key
        }));
        then.status(200).json_body(alpaca_order(&cli_key, "3.5"));
    });
    let journal = accept_journal(&harness);
    let broker = harness.write_client().await.broker();
    let symbol = Symbol::new("AAPL").unwrap();
    let order = AlpacaLimitOrder {
        symbol: symbol.clone(),
        shares: serde_json::from_value(json!("3.5")).unwrap(),
        direction: Direction::Buy,
        limit_price: AlpacaLimitPrice::try_new(serde_json::from_value(json!("150.5")).unwrap())
            .unwrap(),
        extended_hours: false,
        client_order_id: cli_key.parse().unwrap(),
    };

    let placement = broker
        .place_alpaca_limit_order(order, "close the odd lot")
        .await
        .unwrap();
    let created = broker
        .create_journal(
            "issuer",
            &symbol,
            serde_json::from_value(json!("5")).unwrap(),
            Uuid::new_v4(),
            "return shares to the issuer",
        )
        .await
        .unwrap();

    assert_eq!(placement.order_id, ORDER_ID);
    assert_eq!(created.id.to_string(), JOURNAL_ID);
    place.assert_calls(1);
    journal.assert_calls(1);
    let answered: Vec<_> = harness
        .audit_events()
        .into_iter()
        .filter(|event| event.phase == AuditPhase::Answered)
        .map(|event| {
            (
                event.operation,
                event.tier,
                event.outcome,
                event.principal_email,
                event.reason,
            )
        })
        .collect();
    let operator = Some("operator@example.com".to_string());
    assert_eq!(
        answered,
        [
            (
                Operation::OrdersPlaceExactLimit,
                Tier::Write,
                Some(Outcome::Applied),
                operator.clone(),
                Some("close the odd lot".to_string()),
            ),
            (
                Operation::JournalsCreate,
                Tier::Write,
                Some(Outcome::Applied),
                operator,
                Some("return shares to the issuer".to_string()),
            ),
        ]
    );
}
