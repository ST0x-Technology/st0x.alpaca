//! Adapters that make a [`GatewayClient`] look like the `st0x-alpaca`
//! services a bot already calls: the same method names, the library's
//! argument and result types, and the library's error enums. Each adapter
//! implements the library trait its poll loops run over
//! ([`ConversionOrders`], [`WalletTransfers`], [`TokenizationLookups`]), so
//! the library's `*_with` loops run over the gateway unchanged.
//!
//! A human tier refuses a mutation without a non blank reason, so every
//! mutation a human tier serves takes one: required where only the write
//! tier serves it, optional where the bot tier, which sends none, serves it
//! too.
//!
//! A failed call becomes:
//!
//! - the library variant, for the rejections a caller branches on:
//!   `insufficient_balance` is `UsdConversionInsufficientBalance`, a
//!   definitive mint rejection its tokenization variant, and
//!   `request_not_found` on `tokenization.request` is `RequestNotFound`;
//! - the family's `ApiError` with Alpaca's status (429 for the human
//!   budget), message and `Retry-After`, for `backpressure` and for an
//!   `upstream_transient` that carries an `alpacaStatus`; a market data read
//!   wraps it in `LatestTrade` or `LatestQuote`;
//! - the family's `Gateway` variant for everything else, carrying whether a
//!   later call can succeed (never after another `rejected`), the hold,
//!   whether a mutation's outcome is unknown and whether resending it with
//!   the same key is safe. A mutation without an answer reads as
//!   `outcome_unknown` unless it provably never left: no token, no
//!   connection, or a request that never built.

use std::time::Duration;

use alloy_primitives::{Address, TxHash};
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use st0x_alpaca::GatewayHopError;
use st0x_alpaca::broker::{
    AccountActivitiesQuery, AccountActivity, AccountFunds, AlpacaBrokerApiError, AlpacaLimitOrder,
    AlpacaMarketDataError, AssetDetails, CONVERSION_POLL_INTERVAL, CancellationOutcome,
    ClientOrderId, ConversionOrder, ConversionOrders, CryptoOrderResponse, IndicativeQuote,
    Inventory, LatestQuote, LimitOrder, MarketOrder, MarketSession, MarketSessionStatus,
    OrderPlacement, OrderState, PreparedShares, RecoveredOrderPlacement, convert_usdc_usd_with,
    poll_conversion_to_terminal_with,
};
use st0x_alpaca::core::Network as Chain;
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usd, Usdc};
use st0x_alpaca::tokenization::{
    AlpacaApiErrorMessage, AlpacaTokenizationError, InvalidTokenizationParameters, IssuerRequestId,
    TokenizationLookups, TokenizationRequest, TokenizationRequestId,
};
use st0x_alpaca::wallet::{
    AlpacaTransferId, AlpacaWalletError, Network, TokenSymbol, Transfer, WalletTransfers,
};
use uuid::Uuid;

use super::{ClientError, GatewayClient, NONE, TokenSource};
use crate::dto::account::{
    ActivitiesQuery, ActivitiesResponse, PositionMarkResponse, WithdrawableCashResponse,
};
use crate::dto::market::{
    CounterTradeSharesRequest, IsOpenResponse, LatestTradeResponse, OvernightQuoteResponse,
    QuoteResponse, SessionResponse,
};
use crate::dto::orders::{
    CancelOrderRequest, CancelOrderResponse, ConversionRequest, FindConversionResponse,
    FindOrderResponse, LimitOrderRequest, MarketOrderRequest, RecoverOrderRequest,
    RecoverOrderResponse,
};
use crate::dto::tokenization::{
    LookupResponse, MintRequest, NetworkQuery, RequestsQuery, RequestsResponse,
};
use crate::dto::wallet::{
    DepositAddressQuery, DepositAddressResponse, DepositResponse, JournalCreateRequest,
    JournalCreateResponse, TransferResponse, TransfersResponse, WithdrawRequest,
};
use crate::failure::{ErrorBody, ErrorCode, Outcome, RejectionReason};
use crate::ops::Operation;

impl<Token: TokenSource + Clone> GatewayClient<Token> {
    /// The broker and market data calls, shaped like `AlpacaBrokerApi`.
    #[must_use]
    pub fn broker(&self) -> GatewayBroker<Token> {
        GatewayBroker {
            client: self.clone(),
        }
    }

    /// The crypto wallet calls, shaped like `AlpacaWalletService`.
    #[must_use]
    pub fn wallet(&self) -> GatewayWallet<Token> {
        GatewayWallet {
            client: self.clone(),
        }
    }

    /// The tokenization calls bound to `network`, shaped like an
    /// `AlpacaTokenizationService` built for that network.
    #[must_use]
    pub fn tokenization(&self, network: Chain) -> GatewayTokenization<Token> {
        GatewayTokenization {
            client: self.clone(),
            network,
        }
    }
}

/// How a failed gateway call reads on the library side.
#[derive(Debug)]
enum Answer<E> {
    /// A typed rejection, rebuilt as its library variant.
    Typed(E),
    /// Alpaca's own answer, relayed with its status.
    Alpaca {
        status: StatusCode,
        message: String,
        retry_after: Option<Duration>,
    },
    /// No Alpaca answer to relay.
    Hop(GatewayHopError),
}

/// Reads a failed call to `operation`. `typed` rebuilds the rejections the
/// call can rebuild.
fn answer<E>(
    operation: Operation,
    error: ClientError,
    typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<E>,
) -> Answer<E> {
    let error = match error {
        ClientError::Gateway { status, body } => return answered(status, body, typed),
        error => error,
    };

    let (may_have_left, retryable) = reach_and_retry(&error);
    let hop = GatewayHopError::transport(error.to_string());
    Answer::Hop(if may_have_left && operation.mutates() {
        GatewayHopError {
            retryable: false,
            outcome_unknown: true,
            retryable_with_same_key: operation.resendable_with_same_key(),
            ..hop
        }
    } else {
        GatewayHopError { retryable, ..hop }
    })
}

/// Whether a call that got no gateway answer may have reached the gateway,
/// and so Alpaca, and whether a later call can succeed.
fn reach_and_retry(error: &ClientError) -> (bool, bool) {
    match error {
        ClientError::Gateway { body, .. } => (true, body.retryable),
        // A request that never built fails the same way again; a connection
        // that never opened carried nothing and can open later.
        ClientError::Transport(source) if source.is_builder() => (false, false),
        ClientError::Transport(source) if source.is_connect() => (false, true),
        ClientError::Transport(_) | ClientError::Timeout(_) => (true, true),
        ClientError::Unexpected { status, .. } => (true, clears_on_its_own(*status)),
        ClientError::Token(_) => (false, true),
        ClientError::TokenStatus(status) => (false, clears_on_its_own(*status)),
        ClientError::Origin(_)
        | ClientError::HttpClient(_)
        | ClientError::MissingParameter(_)
        | ClientError::InvalidParameter { .. } => (false, false),
    }
}

/// Whether an HTTP status in front of the gateway can clear on its own.
fn clears_on_its_own(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

/// Reads the gateway's error body.
fn answered<E>(
    status: u16,
    body: ErrorBody,
    typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<E>,
) -> Answer<E> {
    if body.code == ErrorCode::Rejected
        && let Some(reason) = body.reason
        && let Some(rebuilt) = typed(reason, &body)
    {
        return Answer::Typed(rebuilt);
    }

    let alpaca = body
        .alpaca_status
        .and_then(|alpaca| StatusCode::from_u16(alpaca).ok());
    let relayed = match body.code {
        // Rate limited, by Alpaca or by the human budget: the hold reads as
        // Alpaca's own.
        ErrorCode::Backpressure => Some(alpaca.unwrap_or(StatusCode::TOO_MANY_REQUESTS)),
        // Alpaca's own answer, a rejection included: the library reads its
        // status (a 422 on a cancel is a decline) as the direct path does.
        ErrorCode::UpstreamTransient | ErrorCode::Rejected => alpaca,
        ErrorCode::InvalidRequest
        | ErrorCode::Unauthenticated
        | ErrorCode::Forbidden
        | ErrorCode::CapabilityDisabled
        | ErrorCode::UnknownOperation
        | ErrorCode::Unavailable
        | ErrorCode::NotReady
        | ErrorCode::OutcomeUnknown => None,
    };

    if let Some(alpaca) = relayed {
        return Answer::Alpaca {
            status: alpaca,
            message: body.message,
            retry_after: body.retry_after_secs.map(Duration::from_secs),
        };
    }
    let retryable = body.retryable && body.code != ErrorCode::Rejected;
    let outcome_unknown =
        body.code == ErrorCode::OutcomeUnknown || body.outcome == Some(Outcome::Unknown);
    let retryable_with_same_key = body.retryable_with_same_key;
    let retry_after = body.retry_after_secs.map(Duration::from_secs);
    Answer::Hop(GatewayHopError {
        message: ClientError::Gateway { status, body }.to_string(),
        retryable,
        outcome_unknown,
        retryable_with_same_key,
        retry_after,
    })
}

/// For the calls without a typed rejection.
fn untyped<E>(_: RejectionReason, _: &ErrorBody) -> Option<E> {
    None
}

fn broker_error(operation: Operation, error: ClientError) -> AlpacaBrokerApiError {
    let typed = |reason, body: &ErrorBody| {
        (reason == RejectionReason::InsufficientBalance).then(|| {
            AlpacaBrokerApiError::UsdConversionInsufficientBalance {
                source: Box::new(AlpacaBrokerApiError::ApiError {
                    status: body
                        .alpaca_status
                        .and_then(|status| StatusCode::from_u16(status).ok())
                        .unwrap_or(StatusCode::FORBIDDEN),
                    alpaca_code: None,
                    message: body.message.clone(),
                    retry_after: None,
                }),
            }
        })
    };
    match answer(operation, error, typed) {
        Answer::Typed(error) => error,
        Answer::Alpaca {
            status,
            message,
            retry_after,
        } => AlpacaBrokerApiError::ApiError {
            status,
            alpaca_code: None,
            message,
            retry_after,
        },
        Answer::Hop(hop) => AlpacaBrokerApiError::Gateway(hop),
    }
}

/// A market data read's error, wrapped as the direct path wraps it
/// (`LatestTrade` or `LatestQuote`), so its `backpressure()` and
/// `permanence()` read the same.
fn market_data_error(
    operation: Operation,
    error: ClientError,
    wrap: fn(Box<AlpacaMarketDataError>) -> AlpacaBrokerApiError,
) -> AlpacaBrokerApiError {
    match answer(operation, error, untyped) {
        Answer::Typed(error) => error,
        Answer::Alpaca {
            status,
            message,
            retry_after,
        } => wrap(Box::new(AlpacaMarketDataError::ApiError {
            status,
            body: message,
            retry_after,
        })),
        Answer::Hop(hop) => AlpacaBrokerApiError::Gateway(hop),
    }
}

fn wallet_error(
    operation: Operation,
    error: ClientError,
    typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaWalletError>,
) -> AlpacaWalletError {
    match answer(operation, error, typed) {
        Answer::Typed(error) => error,
        Answer::Alpaca {
            status,
            message,
            retry_after,
        } => AlpacaWalletError::ApiError {
            status,
            message,
            retry_after,
        },
        Answer::Hop(hop) => AlpacaWalletError::Gateway(hop),
    }
}

fn tokenization_error(
    operation: Operation,
    error: ClientError,
    typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaTokenizationError>,
) -> AlpacaTokenizationError {
    match answer(operation, error, typed) {
        Answer::Typed(error) => error,
        Answer::Alpaca {
            status,
            message,
            retry_after,
        } => AlpacaTokenizationError::ApiError {
            status,
            message: AlpacaApiErrorMessage::from_response(message),
            retry_after,
        },
        Answer::Hop(hop) => AlpacaTokenizationError::Gateway(hop),
    }
}

/// The definitive rejections of a mint of `symbol`.
fn mint_rejection(
    symbol: &Symbol,
) -> impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaTokenizationError> {
    move |reason, body| match reason {
        RejectionReason::InsufficientPosition => {
            Some(AlpacaTokenizationError::InsufficientPosition {
                symbol: symbol.clone(),
            })
        }
        RejectionReason::UnsupportedAccount => Some(AlpacaTokenizationError::UnsupportedAccount),
        RejectionReason::InvalidParameters => Some(AlpacaTokenizationError::InvalidParameters {
            details: InvalidTokenizationParameters::from_response(body.message.clone()),
        }),
        _ => None,
    }
}

/// A read of request `id` the issuer does not hold.
fn request_not_found(
    id: &TokenizationRequestId,
) -> impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaTokenizationError> {
    move |reason, _| {
        (reason == RejectionReason::RequestNotFound)
            .then(|| AlpacaTokenizationError::RequestNotFound { id: id.clone() })
    }
}

/// An Alpaca order id the library takes as text, read as the UUID the
/// gateway path carries; the direct path refuses the same text the same way.
fn order_uuid(order_id: &str) -> Result<Uuid, AlpacaBrokerApiError> {
    Uuid::parse_str(order_id).map_err(AlpacaBrokerApiError::InvalidOrderId)
}

/// `AlpacaBrokerApi` over the gateway.
#[derive(Clone)]
pub struct GatewayBroker<Token> {
    client: GatewayClient<Token>,
}

impl<Token: TokenSource> GatewayBroker<Token> {
    async fn call<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, AlpacaBrokerApiError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.client
            .send(operation, params, query, body)
            .await
            .map_err(|error| broker_error(operation, error))
    }

    /// `account.funds`, as `AlpacaBrokerApi::account_funds`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn account_funds(&self) -> Result<AccountFunds, AlpacaBrokerApiError> {
        self.call(Operation::AccountFunds, &[], NONE, NONE).await
    }

    /// `account.inventory`, as `AlpacaBrokerApi::fetch_inventory`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_inventory(&self) -> Result<Inventory, AlpacaBrokerApiError> {
        self.call(Operation::AccountInventory, &[], NONE, NONE)
            .await
    }

    /// `account.withdrawable_cash`, as
    /// `AlpacaBrokerApi::withdrawable_cash_cents`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn withdrawable_cash_cents(&self) -> Result<Option<i64>, AlpacaBrokerApiError> {
        let cash: WithdrawableCashResponse = self
            .call(Operation::AccountWithdrawableCash, &[], NONE, NONE)
            .await?;
        Ok(cash.withdrawable_cents)
    }

    /// `account.position_mark`, as `AlpacaBrokerApi::fetch_position_mark`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_position_mark(
        &self,
        symbol: &Symbol,
    ) -> Result<Option<Positive<Usd>>, AlpacaBrokerApiError> {
        let mark: PositionMarkResponse = self
            .call(
                Operation::AccountPositionMark,
                &[("symbol", symbol.to_string())],
                NONE,
                NONE,
            )
            .await?;
        Ok(mark.mark)
    }

    /// `activities.list`, as `AlpacaBrokerApi::fetch_account_activities`;
    /// the gateway sets the page cap and refuses a query naming no type.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_account_activities(
        &self,
        query: &AccountActivitiesQuery,
    ) -> Result<Vec<AccountActivity>, AlpacaBrokerApiError> {
        let query = ActivitiesQuery {
            types: query.activity_types.join(","),
            after: query.after,
            until: query.until,
        };
        let answer: ActivitiesResponse = self
            .call(Operation::ActivitiesList, &[], Some(&query), NONE)
            .await?;
        Ok(answer.activities)
    }

    /// `market.is_open`, as `AlpacaBrokerApi::is_market_open`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn is_market_open(&self) -> Result<bool, AlpacaBrokerApiError> {
        let answer: IsOpenResponse = self.call(Operation::MarketIsOpen, &[], NONE, NONE).await?;
        Ok(answer.open)
    }

    /// `market.session`, as `AlpacaBrokerApi::market_session`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn market_session(&self) -> Result<MarketSession, AlpacaBrokerApiError> {
        let answer: SessionResponse = self.call(Operation::MarketSession, &[], NONE, NONE).await?;
        Ok(answer.session)
    }

    /// `market.session_status`, as `AlpacaBrokerApi::market_session_status`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn market_session_status(&self) -> Result<MarketSessionStatus, AlpacaBrokerApiError> {
        self.call(Operation::MarketSessionStatus, &[], NONE, NONE)
            .await
    }

    async fn market_data<Response: DeserializeOwned>(
        &self,
        operation: Operation,
        symbol: &Symbol,
        wrap: fn(Box<AlpacaMarketDataError>) -> AlpacaBrokerApiError,
    ) -> Result<Response, AlpacaBrokerApiError> {
        self.client
            .send(operation, &[("symbol", symbol.to_string())], NONE, NONE)
            .await
            .map_err(|error| market_data_error(operation, error, wrap))
    }

    /// `market.latest_trade`, as `AlpacaBrokerApi::fetch_latest_trade_price`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_latest_trade_price(
        &self,
        symbol: &Symbol,
    ) -> Result<Positive<Usd>, AlpacaBrokerApiError> {
        let trade: LatestTradeResponse = self
            .market_data(
                Operation::MarketLatestTrade,
                symbol,
                AlpacaBrokerApiError::LatestTrade,
            )
            .await?;
        Ok(trade.price)
    }

    /// `market.latest_quote`, as `AlpacaBrokerApi::fetch_latest_quote`. A
    /// crossed quote is refused as the direct path refuses it.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_latest_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<LatestQuote, AlpacaBrokerApiError> {
        let quote: QuoteResponse = self
            .market_data(
                Operation::MarketLatestQuote,
                symbol,
                AlpacaBrokerApiError::LatestQuote,
            )
            .await?;
        LatestQuote::try_from(quote).map_err(|source| invalid_quote(symbol, source))
    }

    /// `market.latest_overnight_quote`, as
    /// `AlpacaBrokerApi::fetch_latest_overnight_quote`. A crossed quote is
    /// refused as the direct path refuses it.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn fetch_latest_overnight_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<IndicativeQuote, AlpacaBrokerApiError> {
        let quote: OvernightQuoteResponse = self
            .market_data(
                Operation::MarketLatestOvernightQuote,
                symbol,
                AlpacaBrokerApiError::LatestQuote,
            )
            .await?;
        IndicativeQuote::try_from(quote).map_err(|source| invalid_quote(symbol, source))
    }

    /// `assets.get`, as `AlpacaBrokerApi::get_asset_details`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn get_asset_details(
        &self,
        symbol: &Symbol,
    ) -> Result<AssetDetails, AlpacaBrokerApiError> {
        self.call(
            Operation::AssetsGet,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `assets.counter_trade_shares`, as
    /// `AlpacaBrokerApi::prepare_counter_trade_shares`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn prepare_counter_trade_shares(
        &self,
        symbol: &Symbol,
        shares: Positive<FractionalShares>,
        extended_hours: bool,
    ) -> Result<PreparedShares, AlpacaBrokerApiError> {
        self.call(
            Operation::AssetsCounterTradeShares,
            &[("symbol", symbol.to_string())],
            NONE,
            Some(&CounterTradeSharesRequest {
                shares,
                extended_hours,
            }),
        )
        .await
    }

    /// `orders.place_market`, as `AlpacaBrokerApi::place_market_order`. Safe
    /// to send again with the same key after an unknown outcome. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn place_market_order(
        &self,
        order: MarketOrder,
        reason: Option<&str>,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let request = MarketOrderRequest::new(order, reason.map(str::to_string));
        self.call(Operation::OrdersPlaceMarket, &[], NONE, Some(&request))
            .await
    }

    /// `orders.place_limit`, as `AlpacaBrokerApi::place_limit_order`. Safe
    /// to send again with the same key after an unknown outcome. Served on
    /// the bot tier only, so it carries no reason.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn place_limit_order(
        &self,
        order: LimitOrder,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let request = LimitOrderRequest::new(order, None);
        self.call(Operation::OrdersPlaceLimit, &[], NONE, Some(&request))
            .await
    }

    /// `orders.place_exact_limit`, as
    /// `AlpacaBrokerApi::place_alpaca_limit_order`: an operator limit order
    /// with exactly the quantity given, served on the write tier only, which
    /// records `reason`. Safe to send again with the same key after an
    /// unknown outcome.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn place_alpaca_limit_order(
        &self,
        order: AlpacaLimitOrder,
        reason: impl Into<String>,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let request = LimitOrderRequest::exact(order, reason);
        self.call(Operation::OrdersPlaceExactLimit, &[], NONE, Some(&request))
            .await
    }

    /// `orders.get`, as `AlpacaBrokerApi::get_order_status`.
    ///
    /// # Errors
    ///
    /// `InvalidOrderId` for an id that is not a UUID; otherwise see the module docs.
    pub async fn get_order_status(
        &self,
        order_id: &str,
    ) -> Result<OrderState, AlpacaBrokerApiError> {
        let order_id = order_uuid(order_id)?;
        self.call(
            Operation::OrdersGet,
            &[("order_id", order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `orders.find`, as `AlpacaBrokerApi::get_order_by_client_order_id`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn get_order_by_client_order_id(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<RecoveredOrderPlacement<String>>, AlpacaBrokerApiError> {
        let found: FindOrderResponse = self
            .call(
                Operation::OrdersFind,
                &[("client_order_id", client_order_id.to_string())],
                NONE,
                NONE,
            )
            .await?;
        Ok(found.order)
    }

    /// `orders.recover`, as `AlpacaBrokerApi::recover_order_by_client_id`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn recover_order_by_client_id(
        &self,
        order: &MarketOrder,
    ) -> Result<Option<OrderPlacement<String>>, AlpacaBrokerApiError> {
        let request = RecoverOrderRequest::from(order.clone());
        let recovered: RecoverOrderResponse = self
            .call(Operation::OrdersRecover, &[], NONE, Some(&request))
            .await?;
        Ok(recovered.order)
    }

    /// `orders.cancel`, as `AlpacaBrokerApi::cancel_order`. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// `InvalidOrderId` for an id that is not a UUID; otherwise see the module docs.
    pub async fn cancel_order(
        &self,
        order_id: &str,
        reason: Option<&str>,
    ) -> Result<CancellationOutcome, AlpacaBrokerApiError> {
        let order_id = order_uuid(order_id)?;
        let request = CancelOrderRequest {
            reason: reason.map(str::to_string),
        };
        let cancelled: CancelOrderResponse = self
            .call(
                Operation::OrdersCancel,
                &[("order_id", order_id.to_string())],
                NONE,
                Some(&request),
            )
            .await?;
        Ok(cancelled.outcome)
    }

    /// `conversions.submit`. Never send again with the same key without
    /// `find_conversion_order` first. `reason` is required on the write
    /// tier.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn submit_conversion(
        &self,
        conversion: ConversionOrder,
        client_order_id: &ClientOrderId,
        reason: Option<&str>,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        let request = ConversionRequest {
            client_order_id: client_order_id.clone(),
            conversion: conversion.into(),
            reason: reason.map(str::to_string),
        };
        self.call(Operation::ConversionsSubmit, &[], NONE, Some(&request))
            .await
    }

    /// `conversions.submit` polled to a settled order, as
    /// `AlpacaBrokerApi::convert_usdc_usd`: the library's own loop over the
    /// gateway, at the interval the direct path polls at. `reason` goes with
    /// the submit and with the deadline cancel, and is required on the write
    /// tier.
    ///
    /// # Errors
    ///
    /// The errors of `convert_usdc_usd_with`; see the module docs.
    pub async fn convert_usdc_usd(
        &self,
        conversion: ConversionOrder,
        client_order_id: &ClientOrderId,
        reason: Option<&str>,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        let orders = Conversions {
            broker: self,
            reason,
        };
        convert_usdc_usd_with(
            &orders,
            conversion,
            client_order_id,
            CONVERSION_POLL_INTERVAL,
        )
        .await
    }

    /// `conversions.get` polled to a settled order, as
    /// `AlpacaBrokerApi::poll_conversion_to_terminal`. `reason` goes with the
    /// deadline cancel, and is required on the write tier.
    ///
    /// # Errors
    ///
    /// The errors of `poll_conversion_to_terminal_with`; see the module docs.
    pub async fn poll_conversion_to_terminal(
        &self,
        order_id: Uuid,
        reason: Option<&str>,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        let orders = Conversions {
            broker: self,
            reason,
        };
        poll_conversion_to_terminal_with(&orders, order_id, CONVERSION_POLL_INTERVAL).await
    }

    /// `journals.create`, as `AlpacaBrokerApi::create_journal`, except that
    /// the destination is a `counterparty` name from the deployment's config
    /// and the answer carries no account id: no account id crosses the wire.
    /// Served on the write tier only, which records `reason`. `operation_id`
    /// names the call in the audit. Never send again after an unknown
    /// outcome.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn create_journal(
        &self,
        counterparty: impl Into<String>,
        symbol: &Symbol,
        quantity: Positive<FractionalShares>,
        operation_id: Uuid,
        reason: impl Into<String>,
    ) -> Result<JournalCreateResponse, AlpacaBrokerApiError> {
        let request = JournalCreateRequest {
            counterparty: counterparty.into(),
            symbol: symbol.clone(),
            qty: quantity,
            operation_id,
            reason: Some(reason.into()),
        };
        self.call(Operation::JournalsCreate, &[], NONE, Some(&request))
            .await
    }
}

fn invalid_quote(
    symbol: &Symbol,
    source: st0x_alpaca::broker::LatestQuoteError,
) -> AlpacaBrokerApiError {
    AlpacaBrokerApiError::LatestQuote(Box::new(AlpacaMarketDataError::InvalidQuote {
        symbol: symbol.clone(),
        source,
    }))
}

/// The calls of the library's conversion loops, as a bot makes them: no
/// reason on the submit or on the deadline cancel.
impl<Token: TokenSource> ConversionOrders for GatewayBroker<Token> {
    async fn submit_conversion(
        &self,
        conversion: ConversionOrder,
        client_order_id: &ClientOrderId,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        Self::submit_conversion(self, conversion, client_order_id, None).await
    }

    async fn get_conversion_order(
        &self,
        order_id: Uuid,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        self.call(
            Operation::ConversionsGet,
            &[("order_id", order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    async fn find_conversion_order(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<CryptoOrderResponse>, AlpacaBrokerApiError> {
        let found: FindConversionResponse = self
            .call(
                Operation::ConversionsFind,
                &[("client_order_id", client_order_id.to_string())],
                NONE,
                NONE,
            )
            .await?;
        Ok(found.order)
    }

    async fn cancel_order(
        &self,
        order_id: &str,
    ) -> Result<CancellationOutcome, AlpacaBrokerApiError> {
        Self::cancel_order(self, order_id, None).await
    }
}

/// The calls of the library's conversion loops with `reason` on the submit
/// and on the deadline cancel, for a caller on the write tier.
struct Conversions<'a, Token> {
    broker: &'a GatewayBroker<Token>,
    reason: Option<&'a str>,
}

impl<Token: TokenSource> ConversionOrders for Conversions<'_, Token> {
    async fn submit_conversion(
        &self,
        conversion: ConversionOrder,
        client_order_id: &ClientOrderId,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        self.broker
            .submit_conversion(conversion, client_order_id, self.reason)
            .await
    }

    async fn get_conversion_order(
        &self,
        order_id: Uuid,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        ConversionOrders::get_conversion_order(self.broker, order_id).await
    }

    async fn find_conversion_order(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<CryptoOrderResponse>, AlpacaBrokerApiError> {
        ConversionOrders::find_conversion_order(self.broker, client_order_id).await
    }

    async fn cancel_order(
        &self,
        order_id: &str,
    ) -> Result<CancellationOutcome, AlpacaBrokerApiError> {
        self.broker.cancel_order(order_id, self.reason).await
    }
}

/// `AlpacaWalletService` over the gateway.
#[derive(Clone)]
pub struct GatewayWallet<Token> {
    client: GatewayClient<Token>,
}

impl<Token: TokenSource> GatewayWallet<Token> {
    async fn call<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, AlpacaWalletError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.client
            .send(operation, params, query, body)
            .await
            .map_err(|error| wallet_error(operation, error, untyped))
    }

    /// `wallet.withdraw`, as `AlpacaWalletService::initiate_withdrawal`: the
    /// gateway checks the whitelist, then sends the withdrawal once.
    /// `operation_id` names the call in the audit; `reason` is required on
    /// the write tier. After an unknown outcome, wait for the record carrying
    /// the work result: `answered` with Alpaca traffic, or `settled`. Then
    /// reconcile `list_all_transfers` repeatedly over a window longer than
    /// the local timeout and expected Alpaca processing delay. Match amount,
    /// destination, and `created_at` no earlier than the gateway answer.
    /// Neither a local timeout nor one empty read proves rejection; never
    /// retry the withdrawal from either condition.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn initiate_withdrawal(
        &self,
        amount: Positive<Usdc>,
        asset: &TokenSymbol,
        to_address: &Address,
        operation_id: Uuid,
        reason: Option<&str>,
    ) -> Result<Transfer, AlpacaWalletError> {
        let request = WithdrawRequest {
            amount,
            asset: asset.clone(),
            address: *to_address,
            operation_id,
            reason: reason.map(str::to_string),
        };
        let operation = Operation::WalletWithdraw;
        // The direct path's typed refusal, on the network it checks.
        let not_whitelisted = |reason, _: &ErrorBody| {
            (reason == RejectionReason::AddressNotWhitelisted).then(|| {
                AlpacaWalletError::AddressNotWhitelisted {
                    address: *to_address,
                    asset: asset.clone(),
                    network: Network::new("ethereum"),
                }
            })
        };
        self.client
            .send(operation, &[], NONE, Some(&request))
            .await
            .map_err(|error| wallet_error(operation, error, not_whitelisted))
    }

    /// `wallet.transfer`, as `AlpacaWalletService::get_transfer` followed by
    /// `TransferWithFees::reported_fees`: the transfer and the network plus
    /// Alpaca fee in USDC, `None` when Alpaca omits either one or the
    /// transfer is not in USDC.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn get_transfer_with_fees(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<(Transfer, Option<Usdc>), AlpacaWalletError> {
        let answer: TransferResponse = self
            .call(
                Operation::WalletTransfer,
                &[("transfer_id", transfer_id.0.to_string())],
                NONE,
                NONE,
            )
            .await?;
        Ok((answer.transfer, answer.reported_fees))
    }

    /// `wallet.deposit_address`, as `AlpacaWalletService::get_wallet_address`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn get_wallet_address(
        &self,
        asset: &TokenSymbol,
        network: &Network,
    ) -> Result<Address, AlpacaWalletError> {
        let query = DepositAddressQuery {
            asset: asset.clone(),
            network: network.clone(),
        };
        let answer: DepositAddressResponse = self
            .call(Operation::WalletDepositAddress, &[], Some(&query), NONE)
            .await?;
        Ok(answer.address)
    }

    /// `wallet.transfers`, as `AlpacaWalletService::list_all_transfers`:
    /// fails on a listed row that is not an EVM transfer.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn list_all_transfers(&self) -> Result<Vec<Transfer>, AlpacaWalletError> {
        let answer: TransfersResponse = self
            .call(Operation::WalletTransfers, &[], NONE, NONE)
            .await?;
        Ok(answer.transfers)
    }
}

impl<Token: TokenSource> WalletTransfers for GatewayWallet<Token> {
    async fn get_transfer(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<Transfer, AlpacaWalletError> {
        self.get_transfer_with_fees(transfer_id)
            .await
            .map(|(transfer, _)| transfer)
    }

    /// `wallet.find_deposit`, as `AlpacaWalletService::find_deposit_by_tx_hash`:
    /// the service's own incoming only scan, so an outgoing transfer with
    /// the hash is never the deposit.
    async fn find_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<Transfer>, AlpacaWalletError> {
        let found: DepositResponse = self
            .call(
                Operation::WalletFindDeposit,
                &[("tx_hash", tx_hash.to_string())],
                NONE,
                NONE,
            )
            .await?;
        Ok(found.deposit)
    }
}

/// `AlpacaTokenizationService` for one network over the gateway. The
/// gateway refuses a lookup bound to the network when the issuer reports
/// the request on another chain or without one.
#[derive(Clone)]
pub struct GatewayTokenization<Token> {
    client: GatewayClient<Token>,
    network: Chain,
}

impl<Token: TokenSource> GatewayTokenization<Token> {
    async fn call<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
        typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaTokenizationError>,
    ) -> Result<Response, AlpacaTokenizationError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.client
            .send(operation, params, query, body)
            .await
            .map_err(|error| tokenization_error(operation, error, typed))
    }

    /// `tokenization.mint` on this adapter's network, as
    /// `AlpacaTokenizationService::request_mint`. Safe to send again with the
    /// same `issuer_request_id` after an unknown outcome. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn request_mint(
        &self,
        underlying_symbol: Symbol,
        quantity: Positive<FractionalShares>,
        wallet: Address,
        issuer_request_id: IssuerRequestId,
        reason: Option<&str>,
    ) -> Result<TokenizationRequest, AlpacaTokenizationError> {
        let request = MintRequest {
            issuer_request_id,
            symbol: underlying_symbol,
            quantity,
            wallet_address: wallet,
            network: self.network,
            reason: reason.map(str::to_string),
        };
        self.call(
            Operation::TokenizationMint,
            &[],
            NONE,
            Some(&request),
            mint_rejection(&request.symbol),
        )
        .await
    }

    /// `tokenization.find_mint`, as
    /// `AlpacaTokenizationService::find_mint_by_issuer_request_id`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn find_mint_by_issuer_request_id(
        &self,
        issuer_request_id: &IssuerRequestId,
    ) -> Result<Option<TokenizationRequest>, AlpacaTokenizationError> {
        let found: LookupResponse = self
            .call(
                Operation::TokenizationFindMint,
                &[("issuer_request_id", issuer_request_id.to_string())],
                NONE,
                NONE,
                untyped,
            )
            .await?;
        Ok(found.request)
    }

    /// `tokenization.requests`, as `AlpacaTokenizationService::list_requests`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn list_requests(&self) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        self.requests(None).await
    }

    /// `tokenization.requests` of pending requests only, as
    /// `AlpacaTokenizationService::list_pending_requests`.
    ///
    /// # Errors
    ///
    /// See the module docs.
    pub async fn list_pending_requests(
        &self,
    ) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        self.requests(Some(true)).await
    }

    async fn requests(
        &self,
        pending_only: Option<bool>,
    ) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        let answer: RequestsResponse = self
            .call(
                Operation::TokenizationRequests,
                &[],
                Some(&RequestsQuery { pending_only }),
                NONE,
                untyped,
            )
            .await?;
        Ok(answer.requests)
    }
}

impl<Token: TokenSource> TokenizationLookups for GatewayTokenization<Token> {
    async fn get_request(
        &self,
        id: &TokenizationRequestId,
    ) -> Result<TokenizationRequest, AlpacaTokenizationError> {
        self.call(
            Operation::TokenizationRequest,
            &[("tokenization_request_id", id.to_string())],
            Some(&NetworkQuery {
                network: self.network,
            }),
            NONE,
            request_not_found(id),
        )
        .await
    }

    async fn find_redemption_by_tx(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<TokenizationRequest>, AlpacaTokenizationError> {
        let found: LookupResponse = self
            .call(
                Operation::TokenizationFindRedemption,
                &[("tx_hash", tx_hash.to_string())],
                Some(&NetworkQuery {
                    network: self.network,
                }),
                NONE,
                untyped,
            )
            .await?;
        Ok(found.request)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use st0x_alpaca::Permanence;

    use super::*;
    use crate::Tier;
    use crate::client::StaticToken;

    /// A read, a mutation Alpaca dedupes by key, and a keyless mutation.
    const READ: Operation = Operation::OrdersGet;
    const KEYED: Operation = Operation::OrdersPlaceMarket;
    const KEYLESS: Operation = Operation::WalletWithdraw;

    const CODES: [ErrorCode; 11] = [
        ErrorCode::InvalidRequest,
        ErrorCode::Unauthenticated,
        ErrorCode::Forbidden,
        ErrorCode::CapabilityDisabled,
        ErrorCode::UnknownOperation,
        ErrorCode::Rejected,
        ErrorCode::Backpressure,
        ErrorCode::Unavailable,
        ErrorCode::NotReady,
        ErrorCode::UpstreamTransient,
        ErrorCode::OutcomeUnknown,
    ];

    fn body(code: ErrorCode) -> ErrorBody {
        ErrorBody {
            code,
            outcome: None,
            retryable: false,
            retryable_with_same_key: false,
            retry_after_secs: None,
            reason: None,
            alpaca_status: None,
            request_id: Uuid::nil(),
            message: "refused".to_string(),
        }
    }

    fn rejected(reason: RejectionReason) -> ErrorBody {
        ErrorBody {
            reason: Some(reason),
            ..body(ErrorCode::Rejected)
        }
    }

    fn gateway(body: ErrorBody) -> ClientError {
        ClientError::Gateway {
            status: body.code.status(),
            body,
        }
    }

    /// The answers the gateway gives with each code, with the operation
    /// they answer. Exhaustive so a new code gets its answers here.
    fn answers(code: ErrorCode) -> Vec<(Operation, ErrorBody)> {
        let not_applied = |body: ErrorBody| ErrorBody {
            outcome: Some(Outcome::NotApplied),
            ..body
        };
        let retryable = |body: ErrorBody| ErrorBody {
            retryable: true,
            ..body
        };
        match code {
            ErrorCode::InvalidRequest
            | ErrorCode::Unauthenticated
            | ErrorCode::Forbidden
            | ErrorCode::CapabilityDisabled
            | ErrorCode::UnknownOperation => {
                vec![(READ, body(code)), (KEYLESS, not_applied(body(code)))]
            }
            ErrorCode::Rejected => vec![
                (
                    READ,
                    ErrorBody {
                        alpaca_status: Some(404),
                        ..rejected(RejectionReason::AlpacaApi)
                    },
                ),
                (
                    KEYED,
                    not_applied(ErrorBody {
                        alpaca_status: Some(422),
                        ..rejected(RejectionReason::AlpacaApi)
                    }),
                ),
                // Decided by the gateway, with no Alpaca status.
                (READ, rejected(RejectionReason::UnsupportedNetwork)),
            ],
            ErrorCode::Backpressure => vec![
                (
                    READ,
                    ErrorBody {
                        retryable: true,
                        retry_after_secs: Some(7),
                        alpaca_status: Some(429),
                        ..body(code)
                    },
                ),
                // The human budget, which carries no Alpaca status.
                (
                    KEYED,
                    not_applied(ErrorBody {
                        retryable: true,
                        retry_after_secs: Some(3),
                        ..body(code)
                    }),
                ),
            ],
            ErrorCode::Unavailable | ErrorCode::NotReady => vec![
                (READ, retryable(body(code))),
                // A throttled credential mint: the hold the gateway relays.
                (
                    READ,
                    retryable(ErrorBody {
                        retry_after_secs: Some(5),
                        ..body(code)
                    }),
                ),
                (KEYLESS, not_applied(retryable(body(code)))),
            ],
            ErrorCode::UpstreamTransient => vec![
                (
                    READ,
                    retryable(ErrorBody {
                        alpaca_status: Some(503),
                        ..body(code)
                    }),
                ),
                (READ, retryable(body(code))),
                // An Alpaca answer the library cannot read: asking again
                // returns the same answer.
                (READ, body(code)),
            ],
            ErrorCode::OutcomeUnknown => vec![
                (
                    KEYED,
                    ErrorBody {
                        outcome: Some(Outcome::Unknown),
                        retryable_with_same_key: true,
                        ..body(code)
                    },
                ),
                (
                    KEYLESS,
                    ErrorBody {
                        outcome: Some(Outcome::Unknown),
                        ..body(code)
                    },
                ),
            ],
        }
    }

    /// What the gateway says about trying again: a fresh call, or the same
    /// mutation under the same key.
    fn gateway_permanence(body: &ErrorBody) -> Permanence {
        if body.retryable || body.retryable_with_same_key {
            Permanence::Transient
        } else {
            Permanence::Permanent
        }
    }

    /// Every family, market data included, reads every gateway answer with
    /// the gateway's retry classification, outcome and hold.
    #[test]
    fn every_family_keeps_the_gateway_classification() {
        for (operation, body) in CODES.into_iter().flat_map(answers) {
            let broker = broker_error(operation, gateway(body.clone()));
            let market = market_data_error(
                operation,
                gateway(body.clone()),
                AlpacaBrokerApiError::LatestTrade,
            );
            let wallet = wallet_error(operation, gateway(body.clone()), untyped);
            let tokenization = tokenization_error(operation, gateway(body.clone()), untyped);
            let unknown = |hop: &GatewayHopError| hop.outcome_unknown;

            for (family, permanence, outcome_unknown, backpressure) in [
                (
                    "broker",
                    broker.permanence(),
                    matches!(&broker, AlpacaBrokerApiError::Gateway(hop) if unknown(hop)),
                    broker.backpressure(),
                ),
                (
                    "market data",
                    market.permanence(),
                    matches!(&market, AlpacaBrokerApiError::Gateway(hop) if unknown(hop)),
                    market.backpressure(),
                ),
                (
                    "wallet",
                    wallet.permanence(),
                    matches!(&wallet, AlpacaWalletError::Gateway(hop) if unknown(hop)),
                    wallet.backpressure(),
                ),
                (
                    "tokenization",
                    tokenization.permanence(),
                    matches!(&tokenization, AlpacaTokenizationError::Gateway(hop) if unknown(hop)),
                    tokenization.backpressure(),
                ),
            ] {
                let case = format!("{family} {operation} {body:?}");
                assert_eq!(permanence, gateway_permanence(&body), "{case}");
                assert_eq!(
                    outcome_unknown,
                    body.outcome == Some(Outcome::Unknown),
                    "{case}"
                );
                if let Some(secs) = body.retry_after_secs {
                    assert_eq!(
                        backpressure.map(|pressure| pressure.retry_after),
                        Some(Some(Duration::from_secs(secs))),
                        "{case}"
                    );
                }
            }
        }
    }

    /// A request that never built: a header value with a newline, as a
    /// malformed token or `acting_for` value gives.
    async fn builder_error() -> reqwest::Error {
        let error = reqwest::Client::new()
            .post("http://127.0.0.1:1")
            .header("x-on-behalf-of", "ops\nsplit")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_builder(), "{error}");
        error
    }

    /// A reqwest error from a port nothing listens on: port 1 is reserved and
    /// never bound, so a parallel test cannot take it the way it can reuse an
    /// ephemeral port a dropped listener gave back.
    async fn connect_error() -> reqwest::Error {
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error}");
        error
    }

    /// The error `Response::bytes`, which the client reads every answer
    /// with, gives for a 200 whose body is cut short: the server declares a
    /// longer `Content-Length` than it sends, then closes, as a connection
    /// reset mid answer does. The request reached the server.
    async fn body_cut_short_error() -> reqwest::Error {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "the client closed before sending its request");
                request.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"")
                .await
                .unwrap();
        });

        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .send()
            .await
            .unwrap();
        server.await.unwrap();
        let error = response.bytes().await.unwrap_err();
        assert!(error.is_decode(), "{error:?}");
        error
    }

    /// A call that got no answer: a mutation that may have left reads as
    /// the gateway's `outcome_unknown` for it; nothing else is ambiguous.
    #[tokio::test]
    async fn a_call_without_an_answer_reads_as_the_gateway_would_answer_it() {
        let timeout = || ClientError::Timeout(Duration::from_secs(60));
        let transport = ClientError::Transport;
        for (operation, error, permanence, outcome_unknown) in [
            (READ, timeout(), Permanence::Transient, false),
            (KEYED, timeout(), Permanence::Transient, true),
            (KEYLESS, timeout(), Permanence::Permanent, true),
            // Never built: fails the same way again, and nothing left.
            (
                KEYLESS,
                transport(builder_error().await),
                Permanence::Permanent,
                false,
            ),
            (
                READ,
                transport(builder_error().await),
                Permanence::Permanent,
                false,
            ),
            // Never connected: nothing left, and a later call can connect.
            (
                KEYLESS,
                transport(connect_error().await),
                Permanence::Transient,
                false,
            ),
            // The answer was cut short, so the request reached the gateway.
            (
                KEYLESS,
                transport(body_cut_short_error().await),
                Permanence::Permanent,
                true,
            ),
            (
                KEYED,
                transport(body_cut_short_error().await),
                Permanence::Transient,
                true,
            ),
            (
                READ,
                transport(body_cut_short_error().await),
                Permanence::Transient,
                false,
            ),
            (
                KEYLESS,
                ClientError::Unexpected {
                    status: 502,
                    detail: "bad gateway".to_string(),
                },
                Permanence::Permanent,
                true,
            ),
            (
                READ,
                ClientError::Unexpected {
                    status: 502,
                    detail: "bad gateway".to_string(),
                },
                Permanence::Transient,
                false,
            ),
            // A 2xx body that does not decode decodes no better next time.
            (
                READ,
                ClientError::Unexpected {
                    status: 200,
                    detail: "missing field".to_string(),
                },
                Permanence::Permanent,
                false,
            ),
            // No token: the request never left.
            (
                KEYLESS,
                ClientError::TokenStatus(503),
                Permanence::Transient,
                false,
            ),
            (
                READ,
                ClientError::MissingParameter("order_id"),
                Permanence::Permanent,
                false,
            ),
            // Refused before anything is sent, so not ambiguous even for a
            // mutation.
            (
                KEYED,
                ClientError::InvalidParameter {
                    name: "tokenization_request_id",
                    value: "..".to_string(),
                },
                Permanence::Permanent,
                false,
            ),
        ] {
            assert_reads_as(operation, error, permanence, outcome_unknown);
        }
    }

    /// Asserts how the adapter reads `error` from a call to `operation`.
    fn assert_reads_as(
        operation: Operation,
        error: ClientError,
        permanence: Permanence,
        outcome_unknown: bool,
    ) {
        let message = error.to_string();
        let error = broker_error(operation, error);
        let AlpacaBrokerApiError::Gateway(hop) = &error else {
            panic!("{operation} {message}: {error:?}");
        };
        assert_eq!(hop.permanence(), permanence, "{operation} {message}");
        assert_eq!(
            hop.outcome_unknown, outcome_unknown,
            "{operation} {message}"
        );
        assert_eq!(
            hop.retryable_with_same_key,
            outcome_unknown && operation.resendable_with_same_key(),
            "{operation} {message}"
        );
    }

    /// A token or `acting_for` value that is no header value fails before
    /// anything is sent, the same way every time: even a keyless withdrawal
    /// is permanent with a known outcome, so a caller stops at once rather
    /// than retrying, and the gateway never sees a request.
    #[tokio::test]
    async fn a_request_that_never_builds_is_permanent_and_never_reaches_the_gateway() {
        let server = httpmock::MockServer::start_async().await;
        let any = server
            .mock_async(|_, then| {
                then.status(200);
            })
            .await;
        let malformed_token = GatewayClient::new(
            &server.base_url(),
            Tier::Bot,
            StaticToken("token\nsplit".into()),
        )
        .unwrap();
        let malformed_human =
            GatewayClient::new(&server.base_url(), Tier::Bot, StaticToken("token".into()))
                .unwrap()
                .acting_for("ops\nsplit");

        for client in [malformed_token, malformed_human] {
            let error = client
                .wallet()
                .initiate_withdrawal(
                    serde_json::from_value(json!("10")).unwrap(),
                    &TokenSymbol::new("USDC"),
                    &Address::repeat_byte(0x33),
                    Uuid::new_v4(),
                    None,
                )
                .await
                .unwrap_err();

            let AlpacaWalletError::Gateway(hop) = &error else {
                panic!("{error:?}");
            };
            assert_eq!(hop.permanence(), Permanence::Permanent, "{hop:?}");
            assert!(!hop.outcome_unknown, "{hop:?}");
            assert!(!hop.retryable_with_same_key, "{hop:?}");
        }
        any.assert_calls_async(0).await;
    }

    /// The rejections a caller branches on come back as their library
    /// variant; a rejection Alpaca answered comes back as the family's
    /// `ApiError` with that status, so a 422 on a cancel reads as the
    /// decline it is on the direct path; any other rejection is a permanent
    /// hop with a known outcome.
    #[test]
    fn only_the_rejections_a_caller_branches_on_are_rebuilt() {
        let symbol = Symbol::new("AAPL").unwrap();
        let id: TokenizationRequestId = "tok_req_1".parse().unwrap();
        let refused = |reason| {
            gateway(ErrorBody {
                outcome: Some(Outcome::NotApplied),
                alpaca_status: Some(403),
                ..rejected(reason)
            })
        };

        let error = broker_error(
            Operation::ConversionsSubmit,
            refused(RejectionReason::InsufficientBalance),
        );
        let AlpacaBrokerApiError::UsdConversionInsufficientBalance { source } = &error else {
            panic!("{error:?}");
        };
        assert!(
            matches!(**source, AlpacaBrokerApiError::ApiError { status, .. } if status == 403),
            "{source:?}"
        );

        let mint = |reason| {
            tokenization_error(
                Operation::TokenizationMint,
                refused(reason),
                mint_rejection(&symbol),
            )
        };
        let error = mint(RejectionReason::InsufficientPosition);
        assert!(
            matches!(&error, AlpacaTokenizationError::InsufficientPosition { symbol: named } if *named == symbol),
            "{error:?}"
        );
        for reason in [
            RejectionReason::UnsupportedAccount,
            RejectionReason::InvalidParameters,
        ] {
            assert!(mint(reason).is_definitive_mint_rejection(), "{reason:?}");
        }

        let error = tokenization_error(
            Operation::TokenizationRequest,
            refused(RejectionReason::RequestNotFound),
            request_not_found(&id),
        );
        assert!(
            matches!(&error, AlpacaTokenizationError::RequestNotFound { id: named } if *named == id),
            "{error:?}"
        );
        let relayed = broker_error(
            Operation::OrdersCancel,
            gateway(ErrorBody {
                outcome: Some(Outcome::NotApplied),
                alpaca_status: Some(422),
                ..rejected(RejectionReason::AlpacaApi)
            }),
        );
        assert!(
            matches!(relayed, AlpacaBrokerApiError::ApiError { status, .. } if status == 422),
            "{relayed:?}"
        );

        // The gateway's own rejections carry no Alpaca status.
        let ours = |reason| {
            gateway(ErrorBody {
                outcome: Some(Outcome::NotApplied),
                ..rejected(reason)
            })
        };
        let hops = [
            broker_error(KEYED, ours(RejectionReason::AssetNotActive)).permanence(),
            wallet_error(
                KEYLESS,
                ours(RejectionReason::AddressNotWhitelisted),
                untyped,
            )
            .permanence(),
            tokenization_error(
                Operation::TokenizationRequest,
                ours(RejectionReason::WrongNetwork),
                request_not_found(&id),
            )
            .permanence(),
            tokenization_error(
                Operation::TokenizationMint,
                ours(RejectionReason::RequestNotFound),
                mint_rejection(&symbol),
            )
            .permanence(),
        ];
        assert_eq!(hops, [Permanence::Permanent; 4]);
        assert!(matches!(
            broker_error(KEYED, ours(RejectionReason::AssetNotActive)),
            AlpacaBrokerApiError::Gateway(GatewayHopError {
                retryable: false,
                outcome_unknown: false,
                ..
            })
        ));
    }
}
