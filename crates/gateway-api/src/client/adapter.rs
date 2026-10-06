//! Adapters that make a [`GatewayClient`] look like the `st0x-alpaca`
//! services a bot already calls: the same method names, the library's
//! argument and result types, and the library's error enums.
//!
//! Each adapter implements the library trait its poll loops run over
//! ([`ConversionOrders`], [`WalletTransfers`], [`TokenizationLookups`]), so
//! `convert_usdc_usd_with`, `poll_transfer_until_complete_with`,
//! `poll_mint_until_complete_with` and their siblings run over the gateway
//! with exactly the loop they run over Alpaca.
//!
//! A human tier refuses a mutation that carries no non blank reason, so
//! every mutation a human tier serves takes one: required where only the
//! write tier serves the operation (`place_alpaca_limit_order`,
//! `create_journal`), optional where the bot tier, which sends none, serves
//! it too. The [`ConversionOrders`] implementation sends none, as a bot
//! does; [`GatewayBroker::convert_usdc_usd`] and
//! [`GatewayBroker::poll_conversion_to_terminal`] run the library loop with
//! the caller's reason on the submit and on the deadline cancel.
//!
//! A failed call becomes the library variant the direct call would have
//! returned:
//!
//! - a typed `reason` becomes its variant (`insufficient_balance` becomes
//!   `UsdConversionInsufficientBalance`, a definitive mint rejection its
//!   tokenization variant, `request_not_found` `RequestNotFound` or
//!   `TransferNotFound`, `address_not_whitelisted` `AddressNotWhitelisted`,
//!   `wrong_network` and `network_missing` `WrongNetwork` and
//!   `NetworkMissing`);
//! - `rejected` and `upstream_transient` that carry an `alpacaStatus`, and
//!   every `backpressure`, become the family's `ApiError` with that status
//!   (429 for the human budget), the message and the `Retry-After` hint, so
//!   `backpressure()` and `permanence()` read them as they read Alpaca's own
//!   answer; a market data read wraps it in `LatestTrade` or `LatestQuote`
//!   as the direct path does;
//! - everything else (a refusal by the gateway itself, `unavailable`,
//!   `not_ready`, `outcome_unknown`, no answer, the client's timeout, an
//!   answer outside the contract) becomes the family's `Gateway` variant
//!   carrying the gateway's own classification: whether a later call can
//!   succeed, how long to hold first (`retryAfterSecs`, such as a throttled
//!   credential mint), whether a mutation's outcome is unknown, and whether
//!   resending it with the same key is safe. A mutation that got no answer
//!   reads as the gateway's `outcome_unknown` for it would, unless the
//!   request provably never left (no token, no connection).

use std::time::Duration;

use alloy_primitives::{Address, TxHash};
use reqwest::StatusCode;
use st0x_alpaca::GatewayHopError;
use st0x_alpaca::broker::{
    AccountActivitiesQuery, AccountActivity, AccountFunds, AlpacaBrokerApiError, AlpacaLimitOrder,
    AlpacaMarketDataError, AssetDetails, AssetStatus, CONVERSION_POLL_INTERVAL,
    CancellationOutcome, ClientOrderId, ConversionOrder, ConversionOrders, CryptoOrderResponse,
    IndicativeQuote, Inventory, LatestQuote, LimitOrder, MarketOrder, MarketSession,
    MarketSessionStatus, OrderPlacement, OrderState, PreparedShares, RecoveredOrderPlacement,
    convert_usdc_usd_with, poll_conversion_to_terminal_with,
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

use super::{ClientError, GatewayClient, TokenSource};
use crate::dto::account::ActivitiesQuery;
use crate::dto::market::CounterTradeSharesRequest;
use crate::dto::orders::{
    CancelOrderRequest, ConversionRequest, ExactLimitOrderRequest, LimitOrderRequest,
    MarketOrderRequest, RecoverOrderRequest,
};
use crate::dto::tokenization::{MintRequest, RequestsQuery};
use crate::dto::wallet::{
    DepositAddressQuery, JournalCreateRequest, JournalCreateResponse, WithdrawRequest,
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

/// Reads a failed call to `operation`. `typed` rebuilds the typed
/// rejections the call can rebuild.
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

/// Whether a failed call may have reached the gateway, and so Alpaca, and
/// whether a later call can succeed.
fn reach_and_retry(error: &ClientError) -> (bool, bool) {
    match error {
        ClientError::Gateway { body, .. } => (true, body.retryable),
        // A connection that never opened carried nothing.
        ClientError::Transport(source) => (!(source.is_builder() || source.is_connect()), true),
        ClientError::Timeout(_) => (true, true),
        ClientError::Unexpected { status, .. } => (true, clears_on_its_own(*status)),
        ClientError::Token(_) => (false, true),
        ClientError::TokenStatus(status) => (false, clears_on_its_own(*status)),
        ClientError::Origin(_) | ClientError::MissingParameter(_) => (false, false),
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
        // Rate limited, by Alpaca or by the human budget: nothing was
        // applied, and the hold reads as Alpaca's own.
        ErrorCode::Backpressure => Some(alpaca.unwrap_or(StatusCode::TOO_MANY_REQUESTS)),
        // Alpaca's own answer to a read, or to a mutation it refused.
        ErrorCode::Rejected | ErrorCode::UpstreamTransient => alpaca,
        // Decided by the gateway, or a mutation whose result is not known:
        // whatever status it carries, there is no Alpaca answer to relay.
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
    let retryable = body.retryable;
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

fn broker_error(
    operation: Operation,
    error: ClientError,
    typed: impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaBrokerApiError>,
) -> AlpacaBrokerApiError {
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

/// The broker rejections a call can rebuild. `symbol` is the asset an order,
/// asset or market data read named, which the asset rejections carry.
fn broker_rejection(
    symbol: Option<&Symbol>,
) -> impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaBrokerApiError> {
    use RejectionReason as R;

    move |reason, body| match reason {
        R::InsufficientBalance => Some(AlpacaBrokerApiError::UsdConversionInsufficientBalance {
            source: Box::new(AlpacaBrokerApiError::ApiError {
                status: body
                    .alpaca_status
                    .and_then(|status| StatusCode::from_u16(status).ok())
                    .unwrap_or(StatusCode::FORBIDDEN),
                alpaca_code: None,
                message: body.message.clone(),
                retry_after: None,
            }),
        }),
        // An asset is either active or inactive; this rejection means the
        // latter.
        R::AssetNotActive => symbol.map(|symbol| AlpacaBrokerApiError::AssetNotActive {
            symbol: symbol.clone(),
            status: AssetStatus::Inactive,
        }),
        R::AssetNotTradable => symbol.map(|symbol| AlpacaBrokerApiError::AssetNotTradable {
            symbol: symbol.clone(),
        }),
        // Carries the account id and status, which the answer does not.
        R::AccountNotActive
        // Relayed by its Alpaca status.
        | R::AlpacaApi
        // Not broker answers.
        | R::InsufficientPosition
        | R::UnsupportedAccount
        | R::InvalidParameters
        | R::AddressNotWhitelisted
        | R::DestinationNotAllowed
        | R::WrongNetwork
        | R::NetworkMissing
        | R::UnsupportedNetwork
        | R::RequestNotFound => None,
    }
}

/// A market data read's error, wrapped as the direct path wraps it
/// (`LatestTrade` or `LatestQuote`), so its `backpressure()` and
/// `permanence()` read the same. Alpaca's 401 and 403 there are the
/// direct path's `Entitlement`.
fn market_data_error(
    operation: Operation,
    error: ClientError,
    symbol: &Symbol,
    wrap: fn(Box<AlpacaMarketDataError>) -> AlpacaBrokerApiError,
) -> AlpacaBrokerApiError {
    match answer(operation, error, broker_rejection(Some(symbol))) {
        Answer::Typed(error) => error,
        Answer::Alpaca {
            status,
            message,
            retry_after,
        } => wrap(Box::new(
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                AlpacaMarketDataError::Entitlement {
                    status,
                    body: message,
                }
            } else {
                AlpacaMarketDataError::ApiError {
                    status,
                    body: message,
                    retry_after,
                }
            },
        )),
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

/// What a wallet call names, which its typed rejections carry.
#[derive(Debug, Clone, Copy)]
enum WalletSubject<'a> {
    /// A read of the account's wallet as a whole.
    Account,
    Transfer(&'a AlpacaTransferId),
    Withdrawal {
        address: &'a Address,
        asset: &'a TokenSymbol,
    },
}

/// The wallet rejections a call can rebuild.
fn wallet_rejection(
    subject: WalletSubject<'_>,
) -> impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaWalletError> {
    use RejectionReason as R;

    move |reason, _| match (reason, subject) {
        (R::RequestNotFound, WalletSubject::Transfer(transfer_id)) => {
            Some(AlpacaWalletError::TransferNotFound {
                transfer_id: *transfer_id,
            })
        }
        (R::AddressNotWhitelisted, WalletSubject::Withdrawal { address, asset }) => {
            Some(AlpacaWalletError::AddressNotWhitelisted {
                address: *address,
                asset: asset.clone(),
                // The chain the library checks withdrawals on.
                network: Network::new("ethereum"),
            })
        }
        // A pinned destination refusal is decided by the gateway with
        // nothing sent: a permanent hop whose outcome is known.
        (
            R::DestinationNotAllowed
            // Relayed by its Alpaca status.
            | R::AlpacaApi
            // Need a subject the call does not name, or are not wallet
            // answers.
            | R::RequestNotFound
            | R::AddressNotWhitelisted
            | R::InsufficientBalance
            | R::AccountNotActive
            | R::AssetNotActive
            | R::AssetNotTradable
            | R::InsufficientPosition
            | R::UnsupportedAccount
            | R::InvalidParameters
            | R::WrongNetwork
            | R::NetworkMissing
            | R::UnsupportedNetwork,
            _,
        ) => None,
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

/// What a tokenization call names, which its typed rejections carry.
#[derive(Debug, Clone, Copy)]
enum TokenizationSubject<'a> {
    /// A read of the account's requests as a whole.
    Account,
    /// A mint of this symbol.
    Mint(&'a Symbol),
    /// A read of this request.
    Request(&'a TokenizationRequestId),
    /// A redemption lookup, which does not know the request it finds.
    Redemption,
}

/// The tokenization rejections a call can rebuild. `network` is the network
/// the adapter is bound to.
fn tokenization_rejection(
    network: Chain,
    subject: TokenizationSubject<'_>,
) -> impl FnOnce(RejectionReason, &ErrorBody) -> Option<AlpacaTokenizationError> {
    use RejectionReason as R;
    use TokenizationSubject as S;

    move |reason, body| match (reason, subject) {
        (R::InsufficientPosition, S::Mint(symbol)) => {
            Some(AlpacaTokenizationError::InsufficientPosition {
                symbol: symbol.clone(),
            })
        }
        (R::UnsupportedAccount, _) => Some(AlpacaTokenizationError::UnsupportedAccount),
        (R::InvalidParameters, _) => Some(AlpacaTokenizationError::InvalidParameters {
            details: InvalidTokenizationParameters::from_response(body.message.clone()),
        }),
        (R::RequestNotFound, S::Request(id)) => {
            Some(AlpacaTokenizationError::RequestNotFound { id: id.clone() })
        }
        (R::WrongNetwork, S::Request(_) | S::Redemption) => {
            Some(AlpacaTokenizationError::WrongNetwork {
                id: reported_request(body, subject)?,
                expected: network,
                actual: Network::from(body.network.clone()?),
            })
        }
        (R::NetworkMissing, S::Request(_) | S::Redemption) => {
            Some(AlpacaTokenizationError::NetworkMissing {
                id: reported_request(body, subject)?,
            })
        }
        (
            // Need a subject the call does not name.
            R::InsufficientPosition
            | R::RequestNotFound
            | R::WrongNetwork
            | R::NetworkMissing
            // Relayed by its Alpaca status.
            | R::AlpacaApi
            // Decided by the gateway: a permanent hop.
            | R::UnsupportedNetwork
            // Not tokenization answers.
            | R::InsufficientBalance
            | R::AccountNotActive
            | R::AssetNotActive
            | R::AssetNotTradable
            | R::AddressNotWhitelisted
            | R::DestinationNotAllowed,
            _,
        ) => None,
    }
}

/// The request a network refusal is about: the one the answer names, or
/// the one the call asked for.
fn reported_request(
    body: &ErrorBody,
    subject: TokenizationSubject<'_>,
) -> Option<TokenizationRequestId> {
    match (body.alpaca_object_ids.first(), subject) {
        (Some(named), _) => named.parse().ok(),
        (None, TokenizationSubject::Request(id)) => Some(id.clone()),
        (
            None,
            TokenizationSubject::Account
            | TokenizationSubject::Mint(_)
            | TokenizationSubject::Redemption,
        ) => None,
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
    /// `account.funds`, as `AlpacaBrokerApi::account_funds`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn account_funds(&self) -> Result<AccountFunds, AlpacaBrokerApiError> {
        self.client
            .account_funds()
            .await
            .map(Into::into)
            .map_err(|error| broker_error(Operation::AccountFunds, error, broker_rejection(None)))
    }

    /// `account.inventory`, as `AlpacaBrokerApi::fetch_inventory`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn fetch_inventory(&self) -> Result<Inventory, AlpacaBrokerApiError> {
        self.client
            .inventory()
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(Operation::AccountInventory, error, broker_rejection(None))
            })
    }

    /// `account.withdrawable_cash`, as
    /// `AlpacaBrokerApi::withdrawable_cash_cents`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn withdrawable_cash_cents(&self) -> Result<Option<i64>, AlpacaBrokerApiError> {
        self.client
            .withdrawable_cash()
            .await
            .map(|cash| cash.withdrawable_cents)
            .map_err(|error| {
                broker_error(
                    Operation::AccountWithdrawableCash,
                    error,
                    broker_rejection(None),
                )
            })
    }

    /// `account.position_mark`, as `AlpacaBrokerApi::fetch_position_mark`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn fetch_position_mark(
        &self,
        symbol: &Symbol,
    ) -> Result<Option<Positive<Usd>>, AlpacaBrokerApiError> {
        self.client
            .position_mark(symbol)
            .await
            .map(|mark| mark.mark)
            .map_err(|error| {
                broker_error(
                    Operation::AccountPositionMark,
                    error,
                    broker_rejection(Some(symbol)),
                )
            })
    }

    /// `activities.list`, as `AlpacaBrokerApi::fetch_account_activities`;
    /// the gateway sets the page cap.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs. A query
    /// naming no activity type is refused by the gateway.
    pub async fn fetch_account_activities(
        &self,
        query: &AccountActivitiesQuery,
    ) -> Result<Vec<AccountActivity>, AlpacaBrokerApiError> {
        let query = ActivitiesQuery {
            types: query.activity_types.join(","),
            after: query.after,
            until: query.until,
        };
        self.client
            .activities(&query)
            .await
            .map(|answer| answer.activities.into_iter().map(Into::into).collect())
            .map_err(|error| broker_error(Operation::ActivitiesList, error, broker_rejection(None)))
    }

    /// `market.is_open`, as `AlpacaBrokerApi::is_market_open`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn is_market_open(&self) -> Result<bool, AlpacaBrokerApiError> {
        self.client
            .market_is_open()
            .await
            .map(|answer| answer.open)
            .map_err(|error| broker_error(Operation::MarketIsOpen, error, broker_rejection(None)))
    }

    /// `market.session`, as `AlpacaBrokerApi::market_session`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn market_session(&self) -> Result<MarketSession, AlpacaBrokerApiError> {
        self.client
            .market_session()
            .await
            .map(|answer| answer.session)
            .map_err(|error| broker_error(Operation::MarketSession, error, broker_rejection(None)))
    }

    /// `market.session_status`, as `AlpacaBrokerApi::market_session_status`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn market_session_status(&self) -> Result<MarketSessionStatus, AlpacaBrokerApiError> {
        self.client
            .market_session_status()
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(
                    Operation::MarketSessionStatus,
                    error,
                    broker_rejection(None),
                )
            })
    }

    /// `market.latest_trade`, as `AlpacaBrokerApi::fetch_latest_trade_price`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn fetch_latest_trade_price(
        &self,
        symbol: &Symbol,
    ) -> Result<Positive<Usd>, AlpacaBrokerApiError> {
        self.client
            .latest_trade(symbol)
            .await
            .map(|trade| trade.price)
            .map_err(|error| {
                market_data_error(
                    Operation::MarketLatestTrade,
                    error,
                    symbol,
                    AlpacaBrokerApiError::LatestTrade,
                )
            })
    }

    /// `market.latest_quote`, as `AlpacaBrokerApi::fetch_latest_quote`. A
    /// crossed quote is refused as the direct path refuses it.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn fetch_latest_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<LatestQuote, AlpacaBrokerApiError> {
        let quote = self.client.latest_quote(symbol).await.map_err(|error| {
            market_data_error(
                Operation::MarketLatestQuote,
                error,
                symbol,
                AlpacaBrokerApiError::LatestQuote,
            )
        })?;
        LatestQuote::try_from(quote).map_err(|source| invalid_quote(symbol, source))
    }

    /// `market.latest_overnight_quote`, as
    /// `AlpacaBrokerApi::fetch_latest_overnight_quote`. A crossed quote is
    /// refused as the direct path refuses it.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn fetch_latest_overnight_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<IndicativeQuote, AlpacaBrokerApiError> {
        let quote = self
            .client
            .latest_overnight_quote(symbol)
            .await
            .map_err(|error| {
                market_data_error(
                    Operation::MarketLatestOvernightQuote,
                    error,
                    symbol,
                    AlpacaBrokerApiError::LatestQuote,
                )
            })?;
        IndicativeQuote::try_from(quote).map_err(|source| invalid_quote(symbol, source))
    }

    /// `assets.get`, as `AlpacaBrokerApi::get_asset_details`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn get_asset_details(
        &self,
        symbol: &Symbol,
    ) -> Result<AssetDetails, AlpacaBrokerApiError> {
        self.client
            .asset(symbol)
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(Operation::AssetsGet, error, broker_rejection(Some(symbol)))
            })
    }

    /// `assets.counter_trade_shares`, as
    /// `AlpacaBrokerApi::prepare_counter_trade_shares`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn prepare_counter_trade_shares(
        &self,
        symbol: &Symbol,
        shares: Positive<FractionalShares>,
        extended_hours: bool,
    ) -> Result<PreparedShares, AlpacaBrokerApiError> {
        self.client
            .counter_trade_shares(
                symbol,
                &CounterTradeSharesRequest {
                    shares,
                    extended_hours,
                },
            )
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(
                    Operation::AssetsCounterTradeShares,
                    error,
                    broker_rejection(Some(symbol)),
                )
            })
    }

    /// `orders.place_market`, as `AlpacaBrokerApi::place_market_order`. Safe
    /// to send again with the same key after an unknown outcome. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn place_market_order(
        &self,
        order: MarketOrder,
        reason: Option<&str>,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let symbol = order.symbol.clone();
        self.client
            .place_market_order(&MarketOrderRequest::new(order, reason.map(str::to_string)))
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(
                    Operation::OrdersPlaceMarket,
                    error,
                    broker_rejection(Some(&symbol)),
                )
            })
    }

    /// `orders.place_limit`, as `AlpacaBrokerApi::place_limit_order`. Safe
    /// to send again with the same key after an unknown outcome. Served on
    /// the bot tier only, so it carries no reason.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn place_limit_order(
        &self,
        order: LimitOrder,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let symbol = order.symbol.clone();
        self.client
            .place_limit_order(&LimitOrderRequest::new(order, None))
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(
                    Operation::OrdersPlaceLimit,
                    error,
                    broker_rejection(Some(&symbol)),
                )
            })
    }

    /// `orders.place_exact_limit`, as
    /// `AlpacaBrokerApi::place_alpaca_limit_order`: an operator limit order
    /// with exactly the quantity given, served on the write tier only, which
    /// records `reason`. Safe to send again with the same key after an
    /// unknown outcome.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn place_alpaca_limit_order(
        &self,
        order: AlpacaLimitOrder,
        reason: impl Into<String>,
    ) -> Result<OrderPlacement<String>, AlpacaBrokerApiError> {
        let symbol = order.symbol.clone();
        self.client
            .place_exact_limit_order(&ExactLimitOrderRequest::new(order, reason))
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(
                    Operation::OrdersPlaceExactLimit,
                    error,
                    broker_rejection(Some(&symbol)),
                )
            })
    }

    /// `orders.get`, as `AlpacaBrokerApi::get_order_status`.
    ///
    /// # Errors
    ///
    /// `InvalidOrderId` for an id that is not a UUID, or the library error
    /// the answer maps to; see the module docs.
    pub async fn get_order_status(
        &self,
        order_id: &str,
    ) -> Result<OrderState, AlpacaBrokerApiError> {
        self.client
            .order(order_uuid(order_id)?)
            .await
            .map(Into::into)
            .map_err(|error| broker_error(Operation::OrdersGet, error, broker_rejection(None)))
    }

    /// `orders.find`, as `AlpacaBrokerApi::get_order_by_client_order_id`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn get_order_by_client_order_id(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<RecoveredOrderPlacement<String>>, AlpacaBrokerApiError> {
        self.client
            .find_order(client_order_id)
            .await
            .map(|found| found.order.map(Into::into))
            .map_err(|error| broker_error(Operation::OrdersFind, error, broker_rejection(None)))
    }

    /// `orders.recover`, as `AlpacaBrokerApi::recover_order_by_client_id`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn recover_order_by_client_id(
        &self,
        order: &MarketOrder,
    ) -> Result<Option<OrderPlacement<String>>, AlpacaBrokerApiError> {
        self.client
            .recover_order(&RecoverOrderRequest::from(order.clone()))
            .await
            .map(|recovered| recovered.order.map(Into::into))
            .map_err(|error| {
                broker_error(
                    Operation::OrdersRecover,
                    error,
                    broker_rejection(Some(&order.symbol)),
                )
            })
    }

    /// `orders.cancel`, as `AlpacaBrokerApi::cancel_order`. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// `InvalidOrderId` for an id that is not a UUID, or the library error
    /// the answer maps to; see the module docs.
    pub async fn cancel_order(
        &self,
        order_id: &str,
        reason: Option<&str>,
    ) -> Result<CancellationOutcome, AlpacaBrokerApiError> {
        self.client
            .cancel_order(
                order_uuid(order_id)?,
                &CancelOrderRequest {
                    reason: reason.map(str::to_string),
                },
            )
            .await
            .map(|cancelled| cancelled.outcome.into())
            .map_err(|error| broker_error(Operation::OrdersCancel, error, broker_rejection(None)))
    }

    /// `conversions.submit`. Never send again with the same key without
    /// `find_conversion_order` first. `reason` is required on the write
    /// tier.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn submit_conversion(
        &self,
        conversion: ConversionOrder,
        client_order_id: &ClientOrderId,
        reason: Option<&str>,
    ) -> Result<CryptoOrderResponse, AlpacaBrokerApiError> {
        self.client
            .submit_conversion(&ConversionRequest {
                client_order_id: client_order_id.clone(),
                conversion: conversion.into(),
                reason: reason.map(str::to_string),
            })
            .await
            .map(Into::into)
            .map_err(|error| {
                broker_error(Operation::ConversionsSubmit, error, broker_rejection(None))
            })
    }

    /// `conversions.submit` polled to a settled order, as
    /// `AlpacaBrokerApi::convert_usdc_usd`: the library's own loop over the
    /// gateway, at the interval the direct path polls at. `reason` goes with
    /// the submit and with the deadline cancel, and is required on the write
    /// tier.
    ///
    /// # Errors
    ///
    /// The errors of `convert_usdc_usd_with`, with each call's failure mapped
    /// as the module docs say.
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
    /// The errors of `poll_conversion_to_terminal_with`, with each call's
    /// failure mapped as the module docs say.
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
    /// The library error the answer maps to; see the module docs.
    pub async fn create_journal(
        &self,
        counterparty: impl Into<String>,
        symbol: &Symbol,
        quantity: Positive<FractionalShares>,
        operation_id: Uuid,
        reason: impl Into<String>,
    ) -> Result<JournalCreateResponse, AlpacaBrokerApiError> {
        self.client
            .create_journal(&JournalCreateRequest {
                counterparty: counterparty.into(),
                symbol: symbol.clone(),
                qty: quantity,
                operation_id,
                reason: Some(reason.into()),
            })
            .await
            .map_err(|error| {
                broker_error(
                    Operation::JournalsCreate,
                    error,
                    broker_rejection(Some(symbol)),
                )
            })
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
        self.client
            .conversion(order_id)
            .await
            .map(Into::into)
            .map_err(|error| broker_error(Operation::ConversionsGet, error, broker_rejection(None)))
    }

    async fn find_conversion_order(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<CryptoOrderResponse>, AlpacaBrokerApiError> {
        self.client
            .find_conversion(client_order_id)
            .await
            .map(|found| found.order.map(Into::into))
            .map_err(|error| {
                broker_error(Operation::ConversionsFind, error, broker_rejection(None))
            })
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
    /// `wallet.withdraw`, as `AlpacaWalletService::initiate_withdrawal`: the
    /// gateway checks the whitelist, then sends the withdrawal once.
    /// `operation_id` names the call in the audit; `reason` is required on
    /// the write tier. Never send again after an unknown outcome; reconcile
    /// from `list_all_transfers`.
    ///
    /// # Errors
    ///
    /// `AddressNotWhitelisted` when the address holds no approved entry for
    /// the asset, or the library error the answer maps to; see the module
    /// docs.
    pub async fn initiate_withdrawal(
        &self,
        amount: Positive<Usdc>,
        asset: &TokenSymbol,
        to_address: &Address,
        operation_id: Uuid,
        reason: Option<&str>,
    ) -> Result<Transfer, AlpacaWalletError> {
        self.client
            .withdraw(&WithdrawRequest {
                amount,
                asset: asset.clone(),
                address: *to_address,
                operation_id,
                reason: reason.map(str::to_string),
            })
            .await
            .map(Into::into)
            .map_err(|error| {
                wallet_error(
                    Operation::WalletWithdraw,
                    error,
                    wallet_rejection(WalletSubject::Withdrawal {
                        address: to_address,
                        asset,
                    }),
                )
            })
    }

    /// `wallet.transfer`, as `AlpacaWalletService::get_transfer` followed by
    /// `TransferWithFees::reported_fees`: the transfer and the network plus
    /// Alpaca fee in USDC, `None` when Alpaca omits either one or the
    /// transfer is not in USDC.
    ///
    /// # Errors
    ///
    /// `TransferNotFound`, or the library error the answer maps to; see the
    /// module docs.
    pub async fn get_transfer_with_fees(
        &self,
        transfer_id: &AlpacaTransferId,
    ) -> Result<(Transfer, Option<Usdc>), AlpacaWalletError> {
        self.client
            .transfer(transfer_id.0)
            .await
            .map(|answer| (answer.transfer.into(), answer.reported_fees))
            .map_err(|error| {
                wallet_error(
                    Operation::WalletTransfer,
                    error,
                    wallet_rejection(WalletSubject::Transfer(transfer_id)),
                )
            })
    }

    /// `wallet.find_deposit`, as `AlpacaWalletService::find_deposit_by_tx_hash`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn find_deposit_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<Transfer>, AlpacaWalletError> {
        self.client
            .find_deposit(tx_hash)
            .await
            .map(|found| found.deposit.map(Into::into))
            .map_err(|error| {
                wallet_error(
                    Operation::WalletFindDeposit,
                    error,
                    wallet_rejection(WalletSubject::Account),
                )
            })
    }

    /// `wallet.deposit_address`, as `AlpacaWalletService::get_wallet_address`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn get_wallet_address(
        &self,
        asset: &TokenSymbol,
        network: &Network,
    ) -> Result<Address, AlpacaWalletError> {
        self.client
            .deposit_address(&DepositAddressQuery {
                asset: asset.clone(),
                network: network.clone(),
            })
            .await
            .map(|answer| answer.address)
            .map_err(|error| {
                wallet_error(
                    Operation::WalletDepositAddress,
                    error,
                    wallet_rejection(WalletSubject::Account),
                )
            })
    }

    /// `wallet.transfers`, as `AlpacaWalletService::list_all_transfers`:
    /// fails on a listed row that is not an EVM transfer.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn list_all_transfers(&self) -> Result<Vec<Transfer>, AlpacaWalletError> {
        self.client
            .transfers()
            .await
            .map(|answer| answer.transfers.into_iter().map(Into::into).collect())
            .map_err(|error| {
                wallet_error(
                    Operation::WalletTransfers,
                    error,
                    wallet_rejection(WalletSubject::Account),
                )
            })
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

    /// `wallet.find_transfer`: the service's own scan, which skips a listed
    /// row on another chain instead of failing the lookup, and costs no
    /// human budget so a poll loop is never cut short.
    async fn find_transfer_by_tx_hash(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<Transfer>, AlpacaWalletError> {
        self.client
            .find_transfer(tx_hash)
            .await
            .map(|found| found.transfer.map(Into::into))
            .map_err(|error| {
                wallet_error(
                    Operation::WalletFindTransfer,
                    error,
                    wallet_rejection(WalletSubject::Account),
                )
            })
    }
}

/// `AlpacaTokenizationService` for one network over the gateway. The
/// lookups bound to the network refuse a request the issuer reports on
/// another chain or without one, as the direct service does.
#[derive(Clone)]
pub struct GatewayTokenization<Token> {
    client: GatewayClient<Token>,
    network: Chain,
}

impl<Token: TokenSource> GatewayTokenization<Token> {
    /// `tokenization.mint` on this adapter's network, as
    /// `AlpacaTokenizationService::request_mint`. Safe to send again with the
    /// same `issuer_request_id` after an unknown outcome. `reason` is
    /// required on the write tier.
    ///
    /// # Errors
    ///
    /// A definitive mint rejection (`InsufficientPosition`,
    /// `UnsupportedAccount`, `InvalidParameters`), or the library error the
    /// answer maps to; see the module docs.
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
        self.client
            .request_mint(&request)
            .await
            .map(Into::into)
            .map_err(|error| {
                tokenization_error(
                    Operation::TokenizationMint,
                    error,
                    tokenization_rejection(
                        self.network,
                        TokenizationSubject::Mint(&request.symbol),
                    ),
                )
            })
    }

    /// `tokenization.find_mint`, as
    /// `AlpacaTokenizationService::find_mint_by_issuer_request_id`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn find_mint_by_issuer_request_id(
        &self,
        issuer_request_id: &IssuerRequestId,
    ) -> Result<Option<TokenizationRequest>, AlpacaTokenizationError> {
        self.client
            .find_mint(issuer_request_id)
            .await
            .map(|found| found.request.map(Into::into))
            .map_err(|error| {
                tokenization_error(
                    Operation::TokenizationFindMint,
                    error,
                    tokenization_rejection(self.network, TokenizationSubject::Account),
                )
            })
    }

    /// `tokenization.requests`, as `AlpacaTokenizationService::list_requests`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn list_requests(&self) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        self.requests(None).await
    }

    /// `tokenization.requests` of pending requests only, as
    /// `AlpacaTokenizationService::list_pending_requests`.
    ///
    /// # Errors
    ///
    /// The library error the answer maps to; see the module docs.
    pub async fn list_pending_requests(
        &self,
    ) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        self.requests(Some(true)).await
    }

    async fn requests(
        &self,
        pending_only: Option<bool>,
    ) -> Result<Vec<TokenizationRequest>, AlpacaTokenizationError> {
        self.client
            .tokenization_requests(&RequestsQuery { pending_only })
            .await
            .map(|answer| answer.requests.into_iter().map(Into::into).collect())
            .map_err(|error| {
                tokenization_error(
                    Operation::TokenizationRequests,
                    error,
                    tokenization_rejection(self.network, TokenizationSubject::Account),
                )
            })
    }
}

impl<Token: TokenSource> TokenizationLookups for GatewayTokenization<Token> {
    async fn get_request(
        &self,
        id: &TokenizationRequestId,
    ) -> Result<TokenizationRequest, AlpacaTokenizationError> {
        self.client
            .tokenization_request(id, self.network)
            .await
            .map(Into::into)
            .map_err(|error| {
                tokenization_error(
                    Operation::TokenizationRequest,
                    error,
                    tokenization_rejection(self.network, TokenizationSubject::Request(id)),
                )
            })
    }

    async fn find_redemption_by_tx(
        &self,
        tx_hash: &TxHash,
    ) -> Result<Option<TokenizationRequest>, AlpacaTokenizationError> {
        self.client
            .find_redemption(tx_hash, self.network)
            .await
            .map(|found| found.request.map(Into::into))
            .map_err(|error| {
                tokenization_error(
                    Operation::TokenizationFindRedemption,
                    error,
                    tokenization_rejection(self.network, TokenizationSubject::Redemption),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;
    use st0x_alpaca::Permanence;
    use st0x_alpaca::wallet::{PollingConfig, poll_transfer_tx_hash_with};

    use super::*;

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
            network: None,
            alpaca_object_ids: Vec::new(),
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

    fn every_answer() -> impl Iterator<Item = (Operation, ErrorBody)> {
        CODES.into_iter().flat_map(answers)
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

    #[test]
    fn broker_errors_keep_the_gateway_retry_and_outcome_classification() {
        for (operation, body) in every_answer() {
            let error = broker_error(operation, gateway(body.clone()), broker_rejection(None));

            assert_eq!(
                error.permanence(),
                gateway_permanence(&body),
                "{operation} {body:?}: {error:?}"
            );
            assert_eq!(
                matches!(&error, AlpacaBrokerApiError::Gateway(hop) if hop.outcome_unknown),
                body.outcome == Some(Outcome::Unknown),
                "{operation} {body:?}: {error:?}"
            );
            // Whichever variant the answer becomes, the hold it relays is
            // the one a caller waits.
            if let Some(secs) = body.retry_after_secs {
                assert_eq!(
                    error.backpressure().map(|pressure| pressure.retry_after),
                    Some(Some(Duration::from_secs(secs))),
                    "{body:?}: {error:?}"
                );
            }
        }
    }

    /// Answers the first read with the gateway's `body` to `operation`,
    /// mapped as the adapter maps it, then with the transfer complete.
    struct ScriptedTransfers {
        operation: Operation,
        body: ErrorBody,
        answered: AtomicBool,
    }

    fn complete_transfer() -> Transfer {
        serde_json::from_value(json!({
            "id": "0f8a4c62-1c3e-4a5b-9d7e-2b6f8c9d0e1a",
            "tx_hash": format!("0x{}", "ab".repeat(32)),
            "direction": "OUTGOING",
            "amount": "250",
            "chain": "ETH",
            "asset": "USDC",
            "from_address": "0x2222222222222222222222222222222222222222",
            "to_address": "0x1111111111111111111111111111111111111111",
            "status": "COMPLETE",
            "created_at": "2026-10-06T09:30:00Z"
        }))
        .unwrap()
    }

    impl WalletTransfers for ScriptedTransfers {
        async fn get_transfer(
            &self,
            transfer_id: &AlpacaTransferId,
        ) -> Result<Transfer, AlpacaWalletError> {
            if self.answered.swap(true, Ordering::SeqCst) {
                return Ok(complete_transfer());
            }
            Err(wallet_error(
                self.operation,
                gateway(self.body.clone()),
                wallet_rejection(WalletSubject::Transfer(transfer_id)),
            ))
        }

        async fn find_transfer_by_tx_hash(
            &self,
            _tx_hash: &TxHash,
        ) -> Result<Option<Transfer>, AlpacaWalletError> {
            Ok(None)
        }
    }

    /// The library's transfer poll retries exactly the answers the gateway
    /// calls retryable, holding at least as long as the gateway asked, and
    /// stops on the rest.
    #[tokio::test(start_paused = true)]
    async fn the_wallet_poll_retries_exactly_what_the_gateway_calls_retryable() {
        let config = PollingConfig {
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(3600),
            max_retries: 0,
            min_retry_delay: Duration::from_millis(10),
            max_retry_delay: Duration::from_millis(10),
        };
        let transfer_id = complete_transfer().id;

        for (operation, body) in every_answer() {
            let transfers = ScriptedTransfers {
                operation,
                body: body.clone(),
                answered: AtomicBool::new(false),
            };

            let started = tokio::time::Instant::now();
            let polled = poll_transfer_tx_hash_with(&transfers, &transfer_id, &config).await;
            let waited = started.elapsed();

            assert_eq!(
                polled.is_ok(),
                gateway_permanence(&body) == Permanence::Transient,
                "{operation} {body:?}: {polled:?}"
            );
            if polled.is_ok() {
                let hold = body
                    .retry_after_secs
                    .map_or(config.interval, Duration::from_secs);
                assert!(waited >= hold, "{operation} {body:?}: waited {waited:?}");
            }
        }
    }

    #[test]
    fn tokenization_errors_relay_alpaca_answers_and_keep_the_gateway_classification() {
        for (operation, body) in every_answer() {
            let error = tokenization_error(
                operation,
                gateway(body.clone()),
                tokenization_rejection(Chain::Base, TokenizationSubject::Account),
            );

            match &error {
                AlpacaTokenizationError::Gateway(hop) => {
                    assert_eq!(hop.permanence(), gateway_permanence(&body), "{body:?}");
                    assert_eq!(
                        hop.outcome_unknown,
                        body.outcome == Some(Outcome::Unknown),
                        "{body:?}"
                    );
                    assert_eq!(
                        hop.retry_after,
                        body.retry_after_secs.map(Duration::from_secs),
                        "{body:?}"
                    );
                }
                AlpacaTokenizationError::ApiError {
                    status,
                    retry_after,
                    ..
                } => {
                    let relayed = body.alpaca_status.unwrap_or(429);
                    assert_eq!(status.as_u16(), relayed, "{body:?}");
                    assert_eq!(
                        *retry_after,
                        body.retry_after_secs.map(Duration::from_secs),
                        "{body:?}"
                    );
                }
                other => panic!("{body:?} became {other:?}"),
            }
        }
    }

    /// A call that got no answer: a mutation that may have left reads as
    /// the gateway's `outcome_unknown` for it; nothing else is ambiguous.
    #[test]
    fn a_call_without_an_answer_reads_as_the_gateway_would_answer_it() {
        let timeout = || ClientError::Timeout(Duration::from_secs(60));
        for (operation, error, permanence, outcome_unknown) in [
            (READ, timeout(), Permanence::Transient, false),
            (KEYED, timeout(), Permanence::Transient, true),
            (KEYLESS, timeout(), Permanence::Permanent, true),
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
        ] {
            let message = error.to_string();
            let error = broker_error(operation, error, broker_rejection(None));

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
    }

    #[test]
    fn typed_broker_rejections_rebuild_their_variants() {
        let symbol = Symbol::new("AAPL").unwrap();
        let rebuild =
            |body: ErrorBody| broker_error(KEYED, gateway(body), broker_rejection(Some(&symbol)));

        let error = rebuild(ErrorBody {
            alpaca_status: Some(403),
            ..rejected(RejectionReason::InsufficientBalance)
        });
        let AlpacaBrokerApiError::UsdConversionInsufficientBalance { source } = &error else {
            panic!("{error:?}");
        };
        assert!(
            matches!(**source, AlpacaBrokerApiError::ApiError { status, .. } if status == 403),
            "{source:?}"
        );

        let error = rebuild(rejected(RejectionReason::AssetNotActive));
        assert!(
            matches!(
                &error,
                AlpacaBrokerApiError::AssetNotActive { symbol: named, status: AssetStatus::Inactive }
                    if *named == symbol
            ),
            "{error:?}"
        );

        let error = rebuild(rejected(RejectionReason::AssetNotTradable));
        assert!(
            matches!(&error, AlpacaBrokerApiError::AssetNotTradable { symbol: named } if *named == symbol),
            "{error:?}"
        );
    }

    /// The direct path wraps a market data failure in `LatestTrade` or
    /// `LatestQuote`, and its permanence and hold read through the wrap.
    #[test]
    fn a_market_data_failure_keeps_its_alpaca_status_inside_the_wrap() {
        let symbol = Symbol::new("AAPL").unwrap();
        let refused = |status| {
            gateway(ErrorBody {
                alpaca_status: Some(status),
                ..rejected(RejectionReason::AlpacaApi)
            })
        };

        let error = market_data_error(
            Operation::MarketLatestQuote,
            refused(404),
            &symbol,
            AlpacaBrokerApiError::LatestQuote,
        );
        let AlpacaBrokerApiError::LatestQuote(source) = &error else {
            panic!("{error:?}");
        };
        assert!(
            matches!(**source, AlpacaMarketDataError::ApiError { status, .. } if status == 404),
            "{source:?}"
        );
        assert_eq!(error.permanence(), Permanence::Permanent);

        // A feed the credentials are not entitled to.
        let error = market_data_error(
            Operation::MarketLatestQuote,
            refused(403),
            &symbol,
            AlpacaBrokerApiError::LatestQuote,
        );
        let AlpacaBrokerApiError::LatestQuote(source) = &error else {
            panic!("{error:?}");
        };
        assert!(
            matches!(**source, AlpacaMarketDataError::Entitlement { status, .. } if status == 403),
            "{source:?}"
        );
        assert_eq!(error.permanence(), Permanence::Permanent);

        let error = market_data_error(
            Operation::MarketLatestTrade,
            gateway(ErrorBody {
                retryable: true,
                alpaca_status: Some(503),
                ..body(ErrorCode::UpstreamTransient)
            }),
            &symbol,
            AlpacaBrokerApiError::LatestTrade,
        );
        let AlpacaBrokerApiError::LatestTrade(source) = &error else {
            panic!("{error:?}");
        };
        assert!(
            matches!(**source, AlpacaMarketDataError::ApiError { status, .. } if status == 503),
            "{source:?}"
        );
        assert_eq!(error.permanence(), Permanence::Transient);

        let error = market_data_error(
            Operation::MarketLatestTrade,
            gateway(ErrorBody {
                retryable: true,
                retry_after_secs: Some(7),
                alpaca_status: Some(429),
                ..body(ErrorCode::Backpressure)
            }),
            &symbol,
            AlpacaBrokerApiError::LatestTrade,
        );
        assert!(
            matches!(error, AlpacaBrokerApiError::LatestTrade(_)),
            "{error:?}"
        );
        assert_eq!(
            error.backpressure().map(|pressure| pressure.retry_after),
            Some(Some(Duration::from_secs(7)))
        );
    }

    #[test]
    fn typed_wallet_rejections_rebuild_their_variants() {
        let transfer_id = complete_transfer().id;
        let error = wallet_error(
            Operation::WalletTransfer,
            gateway(ErrorBody {
                alpaca_status: Some(404),
                ..rejected(RejectionReason::RequestNotFound)
            }),
            wallet_rejection(WalletSubject::Transfer(&transfer_id)),
        );
        assert!(
            matches!(error, AlpacaWalletError::TransferNotFound { transfer_id: id } if id == transfer_id),
            "{error:?}"
        );

        let address = Address::repeat_byte(0x33);
        let asset = TokenSymbol::new("USDC");
        let error = wallet_error(
            KEYLESS,
            gateway(ErrorBody {
                outcome: Some(Outcome::NotApplied),
                ..rejected(RejectionReason::AddressNotWhitelisted)
            }),
            wallet_rejection(WalletSubject::Withdrawal {
                address: &address,
                asset: &asset,
            }),
        );
        assert!(
            matches!(
                &error,
                AlpacaWalletError::AddressNotWhitelisted { address: named, asset: named_asset, .. }
                    if *named == address && *named_asset == asset
            ),
            "{error:?}"
        );
    }

    /// A destination outside the pinned list is refused before anything is
    /// sent: a definite refusal, not an outcome to reconcile.
    #[test]
    fn a_pinned_destination_refusal_is_permanent_with_a_known_outcome() {
        let address = Address::repeat_byte(0x33);
        let asset = TokenSymbol::new("USDC");
        let error = wallet_error(
            KEYLESS,
            gateway(ErrorBody {
                outcome: Some(Outcome::NotApplied),
                reason: Some(RejectionReason::DestinationNotAllowed),
                ..body(ErrorCode::Forbidden)
            }),
            wallet_rejection(WalletSubject::Withdrawal {
                address: &address,
                asset: &asset,
            }),
        );

        let AlpacaWalletError::Gateway(hop) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(hop.permanence(), Permanence::Permanent);
        assert!(!hop.outcome_unknown);
    }

    #[test]
    fn typed_mint_rejections_rebuild_their_variants() {
        let symbol = Symbol::new("AAPL").unwrap();
        let rebuild = |reason| {
            tokenization_error(
                Operation::TokenizationMint,
                gateway(ErrorBody {
                    outcome: Some(Outcome::NotApplied),
                    alpaca_status: Some(403),
                    ..rejected(reason)
                }),
                tokenization_rejection(Chain::Base, TokenizationSubject::Mint(&symbol)),
            )
        };

        let error = rebuild(RejectionReason::InsufficientPosition);
        assert!(
            matches!(&error, AlpacaTokenizationError::InsufficientPosition { symbol: named } if *named == symbol),
            "{error:?}"
        );
        let error = rebuild(RejectionReason::UnsupportedAccount);
        assert!(
            matches!(error, AlpacaTokenizationError::UnsupportedAccount),
            "{error:?}"
        );
        let error = rebuild(RejectionReason::InvalidParameters);
        assert!(
            matches!(error, AlpacaTokenizationError::InvalidParameters { .. }),
            "{error:?}"
        );
        assert!(error.is_definitive_mint_rejection());
    }

    #[test]
    fn a_request_the_issuer_does_not_hold_rebuilds_request_not_found() {
        let id: TokenizationRequestId = "tok_req_1".parse().unwrap();
        let error = tokenization_error(
            Operation::TokenizationRequest,
            gateway(rejected(RejectionReason::RequestNotFound)),
            tokenization_rejection(Chain::Base, TokenizationSubject::Request(&id)),
        );

        assert!(
            matches!(&error, AlpacaTokenizationError::RequestNotFound { id: named } if *named == id),
            "{error:?}"
        );
    }

    #[test]
    fn a_request_on_another_network_rebuilds_wrong_network() {
        let asked: TokenizationRequestId = "tok_req_1".parse().unwrap();
        let wrong_network = ErrorBody {
            network: Some("Ethereum".to_string()),
            alpaca_object_ids: vec!["tok_req_1".to_string()],
            ..rejected(RejectionReason::WrongNetwork)
        };

        for (operation, subject) in [
            (
                Operation::TokenizationRequest,
                TokenizationSubject::Request(&asked),
            ),
            // The redemption lookup learns the request from the answer.
            (
                Operation::TokenizationFindRedemption,
                TokenizationSubject::Redemption,
            ),
        ] {
            let error = tokenization_error(
                operation,
                gateway(wrong_network.clone()),
                tokenization_rejection(Chain::Base, subject),
            );

            assert!(
                matches!(
                    &error,
                    AlpacaTokenizationError::WrongNetwork { id, expected: Chain::Base, actual }
                        if *id == asked && *actual == Network::new("ethereum")
                ),
                "{operation}: {error:?}"
            );
        }
    }

    #[test]
    fn a_request_without_a_network_rebuilds_network_missing() {
        let asked: TokenizationRequestId = "tok_req_1".parse().unwrap();

        // The id the call asked for when the answer names none.
        let error = tokenization_error(
            Operation::TokenizationRequest,
            gateway(rejected(RejectionReason::NetworkMissing)),
            tokenization_rejection(Chain::Base, TokenizationSubject::Request(&asked)),
        );
        assert!(
            matches!(&error, AlpacaTokenizationError::NetworkMissing { id } if *id == asked),
            "{error:?}"
        );

        let error = tokenization_error(
            Operation::TokenizationFindRedemption,
            gateway(ErrorBody {
                alpaca_object_ids: vec!["redeem_7".to_string()],
                ..rejected(RejectionReason::NetworkMissing)
            }),
            tokenization_rejection(Chain::Base, TokenizationSubject::Redemption),
        );
        assert!(
            matches!(
                &error,
                AlpacaTokenizationError::NetworkMissing { id } if id.as_ref() == "redeem_7"
            ),
            "{error:?}"
        );
    }

    /// A network refusal missing what the typed variant carries stays a
    /// definite refusal, never a retryable hop.
    #[test]
    fn a_network_refusal_that_cannot_be_rebuilt_stays_permanent() {
        let error = tokenization_error(
            Operation::TokenizationFindRedemption,
            gateway(rejected(RejectionReason::WrongNetwork)),
            tokenization_rejection(Chain::Base, TokenizationSubject::Redemption),
        );

        let AlpacaTokenizationError::Gateway(hop) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(hop.permanence(), Permanence::Permanent);
    }
}
