//! Typed HTTP client for the gateway.
//!
//! One client talks to one deployment on one tier. Bots use [`Tier::Bot`]
//! with [`MetadataIdToken`]; operator tools use a human tier with the ID
//! token their OAuth flow produced ([`StaticToken`]). Every operation has
//! one method, with its request and response types fixed by the method. The
//! client never retries a request: retry decisions belong to the caller,
//! guided by [`ErrorBody::retryable`] and
//! [`ErrorBody::retryable_with_same_key`].
//!
//! Calling an operation the deployment does not serve on the client's tier
//! answers [`ClientError::Gateway`] with `unknown_operation`.
//!
//! Every call, token included, is bounded by the operation's deadline plus a
//! margin, so the gateway's own deadline answer arrives first. For a
//! mutation, [`ClientError::Transport`], [`ClientError::Timeout`] and
//! [`ClientError::Unexpected`] are as ambiguous as `outcome_unknown`: the
//! request may have reached Alpaca.
//!
//! [`GatewayClient::broker`], [`GatewayClient::wallet`] and
//! [`GatewayClient::tokenization`] wrap the client in adapters that speak the
//! library's own types and errors, so the library's poll loops run over the
//! gateway unchanged.

use std::time::Duration;

use alloy_primitives::{Address, TxHash};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use st0x_alpaca::broker::ClientOrderId;
use st0x_alpaca::core::Network;
use st0x_alpaca::endpoint::{EndpointError, validate_credential_origin};
use st0x_alpaca::st0x_finance::Symbol;
use st0x_alpaca::tokenization::{IssuerRequestId, TokenizationRequestId};
use url::Url;
use uuid::Uuid;

use crate::ON_BEHALF_OF_HEADER;
use crate::access::Tier;
use crate::dto::account::{
    ActivitiesQuery, ActivitiesResponse, FundsResponse, InventoryResponse, PositionMarkResponse,
    WithdrawableCashResponse,
};
use crate::dto::market::{
    AssetResponse, CounterTradeSharesRequest, CounterTradeSharesResponse, IsOpenResponse,
    LatestTradeResponse, OvernightQuoteResponse, QuoteResponse, SessionResponse,
    SessionStatusResponse,
};
use crate::dto::orders::{
    CancelOrderRequest, CancelOrderResponse, ConversionOrderResponse, ConversionRequest,
    ExactLimitOrderRequest, FindConversionResponse, FindOrderResponse, LimitOrderRequest,
    MarketOrderRequest, OrderStateResponse, PlacementResponse, RecoverOrderRequest,
    RecoverOrderResponse,
};
use crate::dto::tokenization::{
    LookupResponse, MintRequest, NetworkQuery, RequestsQuery, RequestsResponse,
    TokenizationRequestResponse,
};
use crate::dto::wallet::{
    DepositAddressQuery, DepositAddressResponse, DepositResponse, JournalCreateRequest,
    JournalCreateResponse, Transfer, TransferLookupResponse, TransferResponse, TransfersResponse,
    TravelRulePatchRequest, WhitelistCreateRequest, WhitelistEntryResponse, WhitelistRemoveRequest,
    WhitelistResponse, WithdrawRequest,
};
use crate::failure::ErrorBody;
use crate::ops::{Method, Operation};

mod adapter;

pub use adapter::{GatewayBroker, GatewayTokenization, GatewayWallet};

/// Margin above an operation's deadline before the client gives up, so the
/// gateway's own deadline answer arrives first.
const DEADLINE_MARGIN: Duration = Duration::from_secs(15);

const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

/// Bounds of one metadata server token request.
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// Produces the credential sent with every request.
pub trait TokenSource: Send + Sync {
    /// Returns a currently valid token.
    fn token(&self) -> impl Future<Output = Result<String, ClientError>> + Send;

    /// The header the token travels in. `authorization` carries it as a
    /// bearer token; any other header carries the bare token, as a proxy in
    /// front of the gateway would set it.
    fn header_name(&self) -> HeaderName {
        AUTHORIZATION
    }
}

/// A token the caller already holds, such as an operator's OAuth ID token.
#[derive(Clone)]
pub struct StaticToken(pub String);

impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String, ClientError> {
        Ok(self.0.clone())
    }
}

/// A Google ID token for the runtime service account, from the metadata
/// server, with the email claim so the gateway can audit it.
///
/// Uses its own HTTP client: no proxy (the token must not leave the host),
/// no redirects (a redirected answer is an error, never a hop), and its own
/// connect and total timeouts.
#[derive(Clone)]
pub struct MetadataIdToken {
    audience: String,
    endpoint: String,
    http: reqwest::Client,
}

impl MetadataIdToken {
    /// # Errors
    ///
    /// [`ClientError::Token`] when the HTTP client cannot be built.
    pub fn new(audience: impl Into<String>) -> Result<Self, ClientError> {
        Self::with_endpoint(audience, METADATA_IDENTITY_URL)
    }

    fn with_endpoint(
        audience: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(METADATA_CONNECT_TIMEOUT)
            .timeout(METADATA_TIMEOUT)
            .build()
            .map_err(ClientError::Token)?;
        Ok(Self {
            audience: audience.into(),
            endpoint: endpoint.into(),
            http,
        })
    }
}

impl TokenSource for MetadataIdToken {
    async fn token(&self) -> Result<String, ClientError> {
        let response = self
            .http
            .get(&self.endpoint)
            .header("Metadata-Flavor", "Google")
            .query(&[("audience", self.audience.as_str()), ("format", "full")])
            .send()
            .await
            .map_err(ClientError::Token)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::TokenStatus(status.as_u16()));
        }
        response.text().await.map_err(ClientError::Token)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not get a token: {0}")]
    Token(#[source] reqwest::Error),
    /// The metadata server answered without a token, including a redirect,
    /// which is never followed.
    #[error("the metadata server answered {0} instead of a token")]
    TokenStatus(u16),
    /// The base URL failed the rule `st0x-alpaca` applies to every
    /// credential bearing origin, so no token was attached to it.
    #[error("refusing to send a gateway token to this URL: {0}")]
    Origin(#[from] EndpointError),
    /// A method did not supply a parameter its path names. A bug in the
    /// client, caught before anything is sent.
    #[error("missing path parameter {0}")]
    MissingParameter(&'static str),
    /// The request did not get an answer: connect failure or transport
    /// error. For a mutation this is as ambiguous as `outcome_unknown`: the
    /// request may have reached the gateway and Alpaca.
    #[error("no answer from the gateway: {0}")]
    Transport(#[source] reqwest::Error),
    /// No answer within the operation's deadline plus the client's margin,
    /// token fetch included. For a mutation this is as ambiguous as
    /// `outcome_unknown`.
    #[error("no answer from the gateway within {0:?}")]
    Timeout(Duration),
    /// The gateway answered with an error body.
    #[error("{} {:?}: {}", .status, .body.code, .body.message)]
    Gateway { status: u16, body: ErrorBody },
    /// The gateway, or something in front of it, answered something that is
    /// not the contract: a 2xx body that does not decode, or a non 2xx body
    /// that is not an [`ErrorBody`] (a load balancer page). For a mutation
    /// this is as ambiguous as `outcome_unknown`: a 2xx means Alpaca applied
    /// it, and an intermediary's error page says nothing about whether the
    /// request reached the gateway.
    #[error("unexpected answer {status}: {detail}")]
    Unexpected { status: u16, detail: String },
}

/// Client of one gateway deployment on one tier.
#[derive(Clone)]
pub struct GatewayClient<Token> {
    base: Url,
    tier: Tier,
    http: reqwest::Client,
    token: Token,
    on_behalf_of: Option<String>,
}

/// No path parameter, no query, no body.
const NONE: Option<&()> = None;

impl<Token: TokenSource> GatewayClient<Token> {
    /// `base` is the deployment's origin: the Cloud Run URL for the bot
    /// tier, the load balancer URL for the human tiers. A path on it is kept
    /// as the prefix of every request path.
    ///
    /// # Errors
    ///
    /// [`ClientError::Origin`] for a base URL that does not parse, is not
    /// HTTPS (plain HTTP only on a loopback host), has no host, or carries
    /// credentials, a query or a fragment.
    pub fn new(
        base: &str,
        tier: Tier,
        http: reqwest::Client,
        token: Token,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            base: validate_credential_origin(base)?,
            tier,
            http,
            token,
            on_behalf_of: None,
        })
    }

    /// A copy of this client that names `human` in `X-On-Behalf-Of` on every
    /// request, for a bot route acting for an IAP verified operator. Audit
    /// only; the gateway never authorizes on it.
    #[must_use]
    pub fn acting_for(&self, human: impl Into<String>) -> Self
    where
        Token: Clone,
    {
        Self {
            on_behalf_of: Some(human.into()),
            ..self.clone()
        }
    }

    // account.* and activities.list

    /// `account.funds`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn account_funds(&self) -> Result<FundsResponse, ClientError> {
        self.send(Operation::AccountFunds, &[], NONE, NONE).await
    }

    /// `account.withdrawable_cash`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn withdrawable_cash(&self) -> Result<WithdrawableCashResponse, ClientError> {
        self.send(Operation::AccountWithdrawableCash, &[], NONE, NONE)
            .await
    }

    /// `account.inventory`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn inventory(&self) -> Result<InventoryResponse, ClientError> {
        self.send(Operation::AccountInventory, &[], NONE, NONE)
            .await
    }

    /// `account.position_mark`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn position_mark(
        &self,
        symbol: &Symbol,
    ) -> Result<PositionMarkResponse, ClientError> {
        self.send(
            Operation::AccountPositionMark,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `activities.list`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn activities(
        &self,
        query: &ActivitiesQuery,
    ) -> Result<ActivitiesResponse, ClientError> {
        self.send(Operation::ActivitiesList, &[], Some(query), NONE)
            .await
    }

    // market.* and assets.*

    /// `market.is_open`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn market_is_open(&self) -> Result<IsOpenResponse, ClientError> {
        self.send(Operation::MarketIsOpen, &[], NONE, NONE).await
    }

    /// `market.session`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn market_session(&self) -> Result<SessionResponse, ClientError> {
        self.send(Operation::MarketSession, &[], NONE, NONE).await
    }

    /// `market.session_status`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn market_session_status(&self) -> Result<SessionStatusResponse, ClientError> {
        self.send(Operation::MarketSessionStatus, &[], NONE, NONE)
            .await
    }

    /// `market.latest_trade`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn latest_trade(&self, symbol: &Symbol) -> Result<LatestTradeResponse, ClientError> {
        self.send(
            Operation::MarketLatestTrade,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `market.latest_quote`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn latest_quote(&self, symbol: &Symbol) -> Result<QuoteResponse, ClientError> {
        self.send(
            Operation::MarketLatestQuote,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `market.latest_overnight_quote`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn latest_overnight_quote(
        &self,
        symbol: &Symbol,
    ) -> Result<OvernightQuoteResponse, ClientError> {
        self.send(
            Operation::MarketLatestOvernightQuote,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `assets.get`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn asset(&self, symbol: &Symbol) -> Result<AssetResponse, ClientError> {
        self.send(
            Operation::AssetsGet,
            &[("symbol", symbol.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `assets.counter_trade_shares`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn counter_trade_shares(
        &self,
        symbol: &Symbol,
        request: &CounterTradeSharesRequest,
    ) -> Result<CounterTradeSharesResponse, ClientError> {
        self.send(
            Operation::AssetsCounterTradeShares,
            &[("symbol", symbol.to_string())],
            NONE,
            Some(request),
        )
        .await
    }

    // orders.* and conversions.*

    /// `orders.place_market`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn place_market_order(
        &self,
        request: &MarketOrderRequest,
    ) -> Result<PlacementResponse, ClientError> {
        self.send(Operation::OrdersPlaceMarket, &[], NONE, Some(request))
            .await
    }

    /// `orders.place_limit`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn place_limit_order(
        &self,
        request: &LimitOrderRequest,
    ) -> Result<PlacementResponse, ClientError> {
        self.send(Operation::OrdersPlaceLimit, &[], NONE, Some(request))
            .await
    }

    /// `orders.place_exact_limit`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn place_exact_limit_order(
        &self,
        request: &ExactLimitOrderRequest,
    ) -> Result<PlacementResponse, ClientError> {
        self.send(Operation::OrdersPlaceExactLimit, &[], NONE, Some(request))
            .await
    }

    /// `orders.get`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn order(&self, order_id: Uuid) -> Result<OrderStateResponse, ClientError> {
        self.send(
            Operation::OrdersGet,
            &[("order_id", order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `orders.find`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_order(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<FindOrderResponse, ClientError> {
        self.send(
            Operation::OrdersFind,
            &[("client_order_id", client_order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `orders.recover`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn recover_order(
        &self,
        request: &RecoverOrderRequest,
    ) -> Result<RecoverOrderResponse, ClientError> {
        self.send(Operation::OrdersRecover, &[], NONE, Some(request))
            .await
    }

    /// `orders.cancel`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn cancel_order(
        &self,
        order_id: Uuid,
        request: &CancelOrderRequest,
    ) -> Result<CancelOrderResponse, ClientError> {
        self.send(
            Operation::OrdersCancel,
            &[("order_id", order_id.to_string())],
            NONE,
            Some(request),
        )
        .await
    }

    /// `conversions.submit`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn submit_conversion(
        &self,
        request: &ConversionRequest,
    ) -> Result<ConversionOrderResponse, ClientError> {
        self.send(Operation::ConversionsSubmit, &[], NONE, Some(request))
            .await
    }

    /// `conversions.get`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn conversion(&self, order_id: Uuid) -> Result<ConversionOrderResponse, ClientError> {
        self.send(
            Operation::ConversionsGet,
            &[("order_id", order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `conversions.find`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_conversion(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<FindConversionResponse, ClientError> {
        self.send(
            Operation::ConversionsFind,
            &[("client_order_id", client_order_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    // journals.create

    /// `journals.create`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn create_journal(
        &self,
        request: &JournalCreateRequest,
    ) -> Result<JournalCreateResponse, ClientError> {
        self.send(Operation::JournalsCreate, &[], NONE, Some(request))
            .await
    }

    // wallet.*

    /// `wallet.withdraw`. Never resend after an unknown outcome; reconcile
    /// from [`Self::transfers`].
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn withdraw(&self, request: &WithdrawRequest) -> Result<Transfer, ClientError> {
        self.send(Operation::WalletWithdraw, &[], NONE, Some(request))
            .await
    }

    /// `wallet.transfer`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn transfer(&self, transfer_id: Uuid) -> Result<TransferResponse, ClientError> {
        self.send(
            Operation::WalletTransfer,
            &[("transfer_id", transfer_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `wallet.transfers`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn transfers(&self) -> Result<TransfersResponse, ClientError> {
        self.send(Operation::WalletTransfers, &[], NONE, NONE).await
    }

    /// `wallet.find_deposit`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_deposit(&self, tx_hash: &TxHash) -> Result<DepositResponse, ClientError> {
        self.send(
            Operation::WalletFindDeposit,
            &[("tx_hash", tx_hash.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `wallet.find_transfer`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_transfer(
        &self,
        tx_hash: &TxHash,
    ) -> Result<TransferLookupResponse, ClientError> {
        self.send(
            Operation::WalletFindTransfer,
            &[("tx_hash", tx_hash.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `wallet.deposit_address`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn deposit_address(
        &self,
        query: &DepositAddressQuery,
    ) -> Result<DepositAddressResponse, ClientError> {
        self.send(Operation::WalletDepositAddress, &[], Some(query), NONE)
            .await
    }

    /// `wallet.whitelist`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn whitelist(&self) -> Result<WhitelistResponse, ClientError> {
        self.send(Operation::WalletWhitelist, &[], NONE, NONE).await
    }

    /// `wallet.whitelist_create`. Never resend after an unknown outcome;
    /// reconcile from [`Self::whitelist`].
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn create_whitelist_entry(
        &self,
        request: &WhitelistCreateRequest,
    ) -> Result<WhitelistEntryResponse, ClientError> {
        self.send(Operation::WalletWhitelistCreate, &[], NONE, Some(request))
            .await
    }

    /// `wallet.whitelist_remove`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn remove_whitelist_entries(
        &self,
        address: &Address,
        request: &WhitelistRemoveRequest,
    ) -> Result<WhitelistResponse, ClientError> {
        self.send(
            Operation::WalletWhitelistRemove,
            &[("address", address.to_string())],
            NONE,
            Some(request),
        )
        .await
    }

    /// `wallet.whitelist_patch_travel_rule`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn patch_travel_rule(
        &self,
        request: &TravelRulePatchRequest,
    ) -> Result<WhitelistResponse, ClientError> {
        self.send(
            Operation::WalletWhitelistPatchTravelRule,
            &[],
            NONE,
            Some(request),
        )
        .await
    }

    // tokenization.*

    /// `tokenization.mint`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn request_mint(
        &self,
        request: &MintRequest,
    ) -> Result<TokenizationRequestResponse, ClientError> {
        self.send(Operation::TokenizationMint, &[], NONE, Some(request))
            .await
    }

    /// `tokenization.requests`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn tokenization_requests(
        &self,
        query: &RequestsQuery,
    ) -> Result<RequestsResponse, ClientError> {
        self.send(Operation::TokenizationRequests, &[], Some(query), NONE)
            .await
    }

    /// `tokenization.request`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn tokenization_request(
        &self,
        tokenization_request_id: &TokenizationRequestId,
        network: Network,
    ) -> Result<TokenizationRequestResponse, ClientError> {
        self.send(
            Operation::TokenizationRequest,
            &[(
                "tokenization_request_id",
                tokenization_request_id.to_string(),
            )],
            Some(&NetworkQuery { network }),
            NONE,
        )
        .await
    }

    /// `tokenization.find_mint`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_mint(
        &self,
        issuer_request_id: &IssuerRequestId,
    ) -> Result<LookupResponse, ClientError> {
        self.send(
            Operation::TokenizationFindMint,
            &[("issuer_request_id", issuer_request_id.to_string())],
            NONE,
            NONE,
        )
        .await
    }

    /// `tokenization.find_redemption`.
    ///
    /// # Errors
    ///
    /// [`ClientError`]; see the type for each case.
    pub async fn find_redemption(
        &self,
        tx_hash: &TxHash,
        network: Network,
    ) -> Result<LookupResponse, ClientError> {
        self.send(
            Operation::TokenizationFindRedemption,
            &[("tx_hash", tx_hash.to_string())],
            Some(&NetworkQuery { network }),
            NONE,
        )
        .await
    }

    /// Sends one operation, bounded as a whole (token included) by the
    /// operation's deadline plus [`DEADLINE_MARGIN`].
    async fn send<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, ClientError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let bound = operation.deadline() + DEADLINE_MARGIN;
        tokio::time::timeout(bound, self.exchange(operation, params, query, body))
            .await
            .map_err(|_| ClientError::Timeout(bound))?
    }

    async fn exchange<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, ClientError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let url = self.url(operation, params)?;
        let token = self.token.token().await?;

        let request = match operation.method() {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url),
        };
        let mut request = match self.token.header_name() {
            header if header == AUTHORIZATION => request.bearer_auth(token),
            header => match HeaderValue::from_str(&token) {
                Ok(mut value) => {
                    value.set_sensitive(true);
                    request.header(header, value)
                }
                // Refused as a builder error before anything is sent, as
                // `bearer_auth` refuses a token that is no header value.
                Err(_) => request.header(header, token),
            },
        };

        if let Some(query) = query {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(human) = &self.on_behalf_of {
            request = request.header(ON_BEHALF_OF_HEADER, human);
        }

        let response = request.send().await.map_err(ClientError::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(ClientError::Transport)?;

        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|error| ClientError::Unexpected {
                status: status.as_u16(),
                detail: error.to_string(),
            });
        }

        match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(body) => Err(ClientError::Gateway {
                status: status.as_u16(),
                body,
            }),
            Err(_) => Err(ClientError::Unexpected {
                status: status.as_u16(),
                detail: String::from_utf8_lossy(&bytes).chars().take(512).collect(),
            }),
        }
    }

    /// Builds the URL from the base URL's path, the tier prefix and the
    /// operation's path, each parameter encoded as one path segment.
    fn url(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
    ) -> Result<Url, ClientError> {
        let mut segments: Vec<&str> = self
            .tier
            .prefix()
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        for segment in operation
            .path()
            .split('/')
            .filter(|segment| !segment.is_empty())
        {
            let value = match segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
            {
                Some(name) => params
                    .iter()
                    .find(|(param, _)| *param == name)
                    .map(|(_, value)| value.as_str())
                    .ok_or(ClientError::MissingParameter(name))?,
                None => segment,
            };
            segments.push(value);
        }

        let mut url = self.base.clone();
        // `validate_credential_origin` guarantees a host, so the URL can
        // always be a base and `path_segments_mut` cannot fail.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(tier: Tier) -> GatewayClient<StaticToken> {
        GatewayClient::new(
            "https://t0-alpaca.example.com",
            tier,
            reqwest::Client::new(),
            StaticToken("token".into()),
        )
        .unwrap()
    }

    #[test]
    fn only_https_or_loopback_http_origins_get_a_token() {
        for accepted in [
            "https://t0-alpaca.example.com",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            GatewayClient::new(
                accepted,
                Tier::Bot,
                reqwest::Client::new(),
                StaticToken("token".into()),
            )
            .unwrap();
        }

        for refused in [
            "http://t0-alpaca.example.com",
            "http://10.0.0.5:8080",
            "https://user:pass@t0-alpaca.example.com",
            "https://t0-alpaca.example.com?audience=x",
            "unix:/run/gateway.sock",
            "not a url",
        ] {
            let Err(error) = GatewayClient::new(
                refused,
                Tier::Bot,
                reqwest::Client::new(),
                StaticToken("token".into()),
            ) else {
                panic!("{refused} was accepted");
            };
            assert!(
                matches!(error, ClientError::Origin(_)),
                "{refused}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_redirect_from_the_metadata_server_is_an_error_not_a_hop() {
        let server = httpmock::MockServer::start_async().await;
        let redirect = server
            .mock_async(|when, then| {
                when.path("/identity")
                    .header("Metadata-Flavor", "Google")
                    .query_param("audience", "https://t0-alpaca.example.com");
                then.status(302)
                    .header("Location", format!("{}/other", server.base_url()));
            })
            .await;
        let target = server
            .mock_async(|when, then| {
                when.path("/other");
                then.status(200).body("must-not-be-read");
            })
            .await;
        let source = MetadataIdToken::with_endpoint(
            "https://t0-alpaca.example.com",
            format!("{}/identity", server.base_url()),
        )
        .unwrap();

        let error = source.token().await.unwrap_err();

        assert!(matches!(error, ClientError::TokenStatus(302)), "{error}");
        redirect.assert_async().await;
        target.assert_calls_async(0).await;
    }

    #[test]
    fn path_parameters_are_encoded_into_the_tier_prefix() {
        let url = client(Tier::Bot)
            .url(
                Operation::AccountPositionMark,
                &[("symbol", "BRK/B".to_string())],
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://t0-alpaca.example.com/bot/v1/account/positions/BRK%2FB/mark"
        );
    }

    /// A load balancer routing by path prefix serves the gateway below it.
    #[test]
    fn a_path_on_the_base_url_prefixes_every_request() {
        for base in [
            "https://lb.example.com/t0-alpaca",
            "https://lb.example.com/t0-alpaca/",
        ] {
            let url = GatewayClient::new(
                base,
                Tier::Read,
                reqwest::Client::new(),
                StaticToken("token".into()),
            )
            .unwrap()
            .url(Operation::AccountFunds, &[])
            .unwrap();
            assert_eq!(
                url.as_str(),
                "https://lb.example.com/t0-alpaca/alpaca-read/v1/account/funds",
                "{base}"
            );
        }
    }
}
