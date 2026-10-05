//! `tokenization.*`.

use alloy_primitives::{Address, TxHash};
use chrono::{DateTime, Utc};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};
use st0x_alpaca::core::Network;
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol};
use st0x_alpaca::tokenization::{
    ClientRequestId, IssuerRequestId, TokenizationRequest, TokenizationRequestId,
    TokenizationRequestStatus, TokenizationRequestType,
};
use st0x_alpaca::wallet::Network as IssuerNetwork;

/// `tokenization.mint` request. The recipient must be one of the
/// deployment's pinned mint recipients, on every tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MintRequest {
    /// Idempotency key: Alpaca dedupes a mint sent again with the same id.
    pub issuer_request_id: IssuerRequestId,
    pub symbol: Symbol,
    pub quantity: Positive<FractionalShares>,
    pub wallet_address: Address,
    /// Chain the tokens are minted on; picks the deployment's client for it.
    pub network: Network,
    /// Required on the write tier.
    pub reason: Option<String>,
}

/// One tokenization request as Alpaca reports it. `tokenization.mint` and
/// `tokenization.request` answer with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenizationRequestResponse {
    pub id: TokenizationRequestId,
    pub r#type: Option<TokenizationRequestType>,
    pub status: TokenizationRequestStatus,
    pub underlying_symbol: Symbol,
    pub token_symbol: Option<String>,
    pub quantity: FractionalShares,
    pub wallet: Option<Address>,
    pub client_request_id: Option<ClientRequestId>,
    /// The chain as the issuer names it; `None` when the issuer omitted it.
    pub network: Option<IssuerNetwork>,
    pub issuer_request_id: Option<IssuerRequestId>,
    pub tx_hash: Option<TxHash>,
    #[serde(
        default,
        serialize_with = "st0x_float_serde::serialize_option_float",
        deserialize_with = "st0x_float_serde::deserialize_option_float_from_number_or_string"
    )]
    pub fees: Option<Float>,
    pub created_at: DateTime<Utc>,
}

impl From<TokenizationRequest> for TokenizationRequestResponse {
    fn from(request: TokenizationRequest) -> Self {
        Self {
            id: request.id,
            r#type: request.r#type,
            status: request.status,
            underlying_symbol: request.underlying_symbol,
            token_symbol: request.token_symbol,
            quantity: request.quantity,
            wallet: request.wallet,
            client_request_id: request.client_request_id,
            network: request.network,
            issuer_request_id: request.issuer_request_id,
            tx_hash: request.tx_hash,
            fees: request.fees,
            created_at: request.created_at,
        }
    }
}

impl From<TokenizationRequestResponse> for TokenizationRequest {
    fn from(request: TokenizationRequestResponse) -> Self {
        Self {
            id: request.id,
            r#type: request.r#type,
            status: request.status,
            underlying_symbol: request.underlying_symbol,
            token_symbol: request.token_symbol,
            quantity: request.quantity,
            wallet: request.wallet,
            client_request_id: request.client_request_id,
            network: request.network,
            issuer_request_id: request.issuer_request_id,
            tx_hash: request.tx_hash,
            fees: request.fees,
            created_at: request.created_at,
        }
    }
}

/// `tokenization.requests` query. The account comes from the deployment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestsQuery {
    /// Only requests still pending at Alpaca.
    pub pending_only: Option<bool>,
}

/// `tokenization.requests`: the whole account's requests, on every network.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestsResponse {
    pub requests: Vec<TokenizationRequestResponse>,
}

/// Query of the lookups bound to one network: the answer is refused when
/// Alpaca reports the request on another chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkQuery {
    pub network: Network,
}

/// Path of `tokenization.request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizationRequestPath {
    pub tokenization_request_id: TokenizationRequestId,
}

/// Path of `tokenization.find_mint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerRequestIdPath {
    pub issuer_request_id: IssuerRequestId,
}

/// Path of `tokenization.find_redemption`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedemptionTxPath {
    pub tx_hash: TxHash,
}

/// `tokenization.find_mint` and `tokenization.find_redemption`: `None` when
/// Alpaca holds no matching request yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupResponse {
    pub request: Option<TokenizationRequestResponse>,
}
