//! The typed client against the real router over HTTP: each method's request
//! and response types must match what the gateway serves.

use std::time::Duration;

use httpmock::prelude::*;
use serde_json::json;
use st0x_alpaca::Permanence;
use st0x_alpaca::broker::{
    AlpacaBrokerApiError, AlpacaLimitOrder, AlpacaLimitPrice, AlpacaMarketDataError, AssetDetails,
    AssetStatus, ClientOrderId, ConversionOrder, ConversionOrders, CryptoOrderOutcome, Direction,
    convert_usdc_usd_with,
};
use st0x_alpaca::core::Network;
use st0x_alpaca::st0x_finance::Symbol;
use st0x_alpaca::tokenization::{
    AlpacaTokenizationError, IssuerRequestId, TokenizationLookups, TokenizationRequestId,
};
use st0x_alpaca::wallet::{
    AlpacaTransferId, AlpacaWalletError, Network as WalletNetwork, PollingConfig, TokenSymbol,
    TransferStatus, poll_deposit_by_tx_hash_with, poll_transfer_until_complete_with,
};
use st0x_alpaca_gateway_api::client::ClientError;
use st0x_alpaca_gateway_api::dto::orders::{MarketOrderRequest, OrderStateResponse};
use st0x_alpaca_gateway_api::dto::wallet::WithdrawRequest;
use st0x_alpaca_gateway_api::{AuditPhase, ErrorCode, Operation, Outcome, Tier};
use uuid::Uuid;

use crate::common::{ACCOUNT_ID, BOT_WALLET, Harness, JOURNAL_COUNTERPARTY};

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

const TRANSFER_ID: &str = "0f8a4c62-1c3e-4a5b-9d7e-2b6f8c9d0e1a";

fn orders_path() -> String {
    format!("/v1/trading/accounts/{ACCOUNT_ID}/orders")
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

/// Waits until `mock` has answered at least once.
async fn until_called(mock: &httpmock::Mock<'_>) {
    while mock.calls_async().await == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_conversion_is_polled_to_its_fill_by_the_library_loop_over_the_gateway() {
    let harness = Harness::start().await;
    let post = harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(200).json_body(crypto_order("new", "0"));
    });
    let pending = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(200)
            .json_body(crypto_order("partially_filled", "40"));
    });
    let broker = harness.bot_client().await.broker();
    let key = ClientOrderId::from_uuid(CLIENT_UUID.parse().unwrap());
    let quantity = serde_json::from_value(json!("100")).unwrap();

    let conversion = tokio::spawn(async move {
        convert_usdc_usd_with(
            &broker,
            ConversionOrder::SellUsdc(quantity),
            &key,
            Duration::from_millis(20),
        )
        .await
    });
    until_called(&pending).await;
    pending.delete_async().await;
    let filled = harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(200)
            .json_body(crypto_order("filled", "99.999999999"));
    });
    let order = conversion.await.unwrap().unwrap();

    assert_eq!(order.classify(), CryptoOrderOutcome::Filled);
    assert_eq!(order.id.to_string(), ORDER_ID);
    // Alpaca's nine decimal fill survives the hop, not only its six decimal
    // floor.
    assert_eq!(
        serde_json::to_value(order.filled_quantity).unwrap(),
        json!("99.999999999")
    );
    post.assert_calls(1);
    filled.assert();
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

    let poll = tokio::spawn(async move {
        poll_transfer_until_complete_with(&wallet, &transfer_id, &config).await
    });
    until_called(&processing).await;
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
    assert_eq!(
        transfer.id,
        AlpacaTransferId::from(TRANSFER_ID.parse::<Uuid>().unwrap())
    );
    assert_eq!(
        serde_json::to_value(transfer.amount).unwrap(),
        json!("250.123456789")
    );
    complete.assert();
}

#[tokio::test]
async fn an_insufficient_balance_and_a_rate_limit_come_back_as_the_library_variants() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(POST).path(orders_path());
        then.status(403)
            .json_body(json!({ "code": 40_310_000, "message": "insufficient balance" }));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("{}/{ORDER_ID}", orders_path()));
        then.status(429)
            .header("Retry-After", "7")
            .json_body(json!({ "message": "too many requests" }));
    });
    let broker = harness.bot_client().await.broker();
    let key = ClientOrderId::from_uuid(CLIENT_UUID.parse().unwrap());
    let dollars = serde_json::from_value(json!("100")).unwrap();

    // The bot resizes and submits again on exactly this variant.
    let error = broker
        .submit_conversion(ConversionOrder::BuyWithUsd(dollars), &key, None)
        .await
        .unwrap_err();
    let AlpacaBrokerApiError::UsdConversionInsufficientBalance { source } = &error else {
        panic!("expected an insufficient balance, got {error:?}");
    };
    assert!(
        matches!(**source, AlpacaBrokerApiError::ApiError { status, .. } if status == 403),
        "{source:?}"
    );

    // The bot reschedules after the hold on exactly this classification.
    let error = broker
        .get_conversion_order(ORDER_ID.parse().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(error, AlpacaBrokerApiError::ApiError { status, .. } if status == 429),
        "{error:?}"
    );
    assert_eq!(
        error.backpressure().map(|pressure| pressure.retry_after),
        Some(Some(Duration::from_secs(7)))
    );
}

#[tokio::test]
async fn an_asset_comes_back_as_the_library_asset_details() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "inactive",
            "tradable": true,
            "fractionable": false,
            "attributes": ["overnight_tradable"]
        }));
    });
    let broker = harness.bot_client().await.broker();

    let asset = broker
        .get_asset_details(&Symbol::new("AAPL").unwrap())
        .await
        .unwrap();

    assert_eq!(
        asset,
        AssetDetails {
            status: AssetStatus::Inactive,
            tradable: true,
            fractionable: Some(false),
            fractional_eh_enabled: Some(false),
            overnight_tradable: Some(true),
            overnight_halted: Some(false),
        }
    );
}

/// The direct path reads a refused market data request as `ApiError`, or
/// `Entitlement` for a feed the credentials lack, inside the same wrap; a
/// bot reschedules or gives up on exactly that.
#[tokio::test]
async fn a_market_data_refusal_comes_back_as_the_direct_path_error() {
    let harness = Harness::start().await;
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v2/stocks/GONE/quotes/latest");
        then.status(404)
            .json_body(json!({ "message": "symbol not found" }));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v2/stocks/AAPL/trades/latest");
        then.status(403)
            .json_body(json!({ "message": "subscription does not permit SIP feed" }));
    });
    let broker = harness.bot_client().await.broker();

    let error = broker
        .fetch_latest_quote(&Symbol::new("GONE").unwrap())
        .await
        .unwrap_err();
    let AlpacaBrokerApiError::LatestQuote(source) = &error else {
        panic!("expected a latest quote failure, got {error:?}");
    };
    assert!(
        matches!(**source, AlpacaMarketDataError::ApiError { status, .. } if status == 404),
        "{source:?}"
    );
    assert_eq!(error.permanence(), Permanence::Permanent);

    let error = broker
        .fetch_latest_trade_price(&Symbol::new("AAPL").unwrap())
        .await
        .unwrap_err();
    let AlpacaBrokerApiError::LatestTrade(source) = &error else {
        panic!("expected a latest trade failure, got {error:?}");
    };
    assert!(
        matches!(**source, AlpacaMarketDataError::Entitlement { status, .. } if status == 403),
        "{source:?}"
    );
    assert_eq!(error.permanence(), Permanence::Permanent);
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

const JOURNAL_ID: &str = "9c1f2e3d-4b5a-4c6d-8e7f-a0b1c2d3e4f5";

/// Accepts exactly the 3.5 share limit order under `cli_key` on an asset
/// that is not fractionable, where only the exact limit order sends 3.5.
fn accept_exact_limit_order<'a>(harness: &'a Harness, cli_key: &str) -> httpmock::Mock<'a> {
    harness.alpaca.mock(|when, then| {
        when.method(GET).path("/v1/assets/AAPL");
        then.status(200).json_body(json!({
            "status": "active",
            "tradable": true,
            "fractionable": false
        }));
    });
    harness.alpaca.mock(|when, then| {
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
        then.status(200).json_body(json!({
            "id": ORDER_ID,
            "client_order_id": cli_key,
            "symbol": "AAPL",
            "qty": "3.5",
            "side": "buy",
            "status": "new",
            "created_at": "2026-09-17T10:15:29Z",
            "filled_qty": "0",
            "filled_avg_price": null
        }));
    })
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
    let place = accept_exact_limit_order(&harness, &cli_key);
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
    let operator = Some("operator@t0trade.com".to_string());
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

/// The deposit poll finds the transfer through `wallet.find_transfer`,
/// whose scan skips a listed row on another chain where the full transfer
/// list fails on it.
#[tokio::test]
async fn a_deposit_is_polled_by_its_hash_past_a_transfer_on_another_chain() {
    let harness = Harness::start().await;
    let tx_hash = format!("0x{}", "cd".repeat(32));
    let mut deposit = transfer_json("COMPLETE", Some(&tx_hash));
    deposit["direction"] = json!("INCOMING");
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/wallets/transfers"));
        then.status(200).json_body(json!([
            {
                "id": "1d2e3f40-5a6b-4c7d-8e9f-a0b1c2d3e4f5",
                "tx_hash": "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW",
                "direction": "INCOMING",
                "amount": "1",
                "chain": "SOL",
                "asset": "USDC",
                "from_address": "7EcDhSYGxXyscszYEp35KHN8vvw3svAuLKTzXwCFLtV",
                "to_address": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                "status": "COMPLETE",
                "created_at": "2026-10-06T09:00:00Z"
            },
            deposit
        ]));
    });
    let wallet = harness.bot_client().await.wallet();
    let config = PollingConfig {
        interval: Duration::from_millis(20),
        timeout: Duration::from_secs(10),
        max_retries: 0,
        min_retry_delay: Duration::from_millis(10),
        max_retry_delay: Duration::from_millis(10),
    };

    let found = poll_deposit_by_tx_hash_with(&wallet, &tx_hash.parse().unwrap(), &config)
        .await
        .unwrap();

    assert_eq!(
        found.id,
        AlpacaTransferId::from(TRANSFER_ID.parse::<Uuid>().unwrap())
    );
    assert_eq!(found.status, TransferStatus::Complete);
}

/// A tokenization request the issuer reports on another chain is the
/// direct service's `WrongNetwork`, naming the request even where the call
/// only knew the transaction.
#[tokio::test]
async fn a_request_on_another_network_comes_back_as_wrong_network() {
    let harness = Harness::start().await;
    let tx_hash = format!("0x{}", "ef".repeat(32));
    let request = |id: &str, kind: &str| {
        json!({
            "tokenization_request_id": id,
            "type": kind,
            "status": "pending",
            "underlying_symbol": "AAPL",
            "token_symbol": "tAAPL",
            "qty": "2.5",
            "issuer": "st0x",
            "network": "ethereum",
            "wallet_address": BOT_WALLET,
            "tx_hash": tx_hash,
            "created_at": "2026-10-05T10:30:00Z"
        })
    };
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests"))
            .query_param("type", "redeem");
        then.status(200)
            .json_body(json!([request("tok_req_redeem", "redeem")]));
    });
    harness.alpaca.mock(|when, then| {
        when.method(GET)
            .path(format!("/v1/accounts/{ACCOUNT_ID}/tokenization/requests"))
            .query_param_missing("type");
        then.status(200)
            .json_body(json!([request("tok_req_mint", "mint")]));
    });
    let tokenization = harness.bot_client().await.tokenization(Network::Base);

    let asked: TokenizationRequestId = "tok_req_mint".parse().unwrap();
    let error = tokenization.get_request(&asked).await.unwrap_err();
    assert!(
        matches!(
            &error,
            AlpacaTokenizationError::WrongNetwork { id, expected: Network::Base, actual }
                if *id == asked && *actual == WalletNetwork::new("ethereum")
        ),
        "{error:?}"
    );

    let error = tokenization
        .find_redemption_by_tx(&tx_hash.parse().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            AlpacaTokenizationError::WrongNetwork { id, expected: Network::Base, actual }
                if id.as_ref() == "tok_req_redeem" && *actual == WalletNetwork::new("ethereum")
        ),
        "{error:?}"
    );
}
