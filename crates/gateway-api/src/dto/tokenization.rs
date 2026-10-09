//! `tokenization.*`.

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use st0x_alpaca::core::Network;
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol};
use st0x_alpaca::tokenization::{IssuerRequestId, TokenizationRequest};

/// `tokenization.mint` request. The recipient must be one of the
/// deployment's pinned mint recipients, on every tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MintRequest {
    /// Idempotency key: Alpaca dedupes a mint sent again with the same id.
    pub issuer_request_id: IssuerRequestId,
    #[serde(deserialize_with = "super::symbol")]
    pub symbol: Symbol,
    pub quantity: Positive<FractionalShares>,
    pub wallet_address: Address,
    /// Chain the tokens are minted on; picks the deployment's client for it.
    pub network: Network,
    /// Required on the write tier.
    pub reason: Option<String>,
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
    pub requests: Vec<TokenizationRequest>,
}

/// Query of the lookups bound to one network: the answer is refused when
/// Alpaca reports the request on another chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkQuery {
    pub network: Network,
}

/// `tokenization.find_mint` and `tokenization.find_redemption`: `None` when
/// Alpaca holds no matching request yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupResponse {
    pub request: Option<TokenizationRequest>,
}
