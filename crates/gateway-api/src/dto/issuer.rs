//! `issuer.*` and `corporate_actions.stream`.

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::core::{Network, TokenizationRequestId};
use st0x_alpaca::corporate_actions::{
    CorporateActionBootstrapSince, CorporateActionBootstrapSinceError, CorporateActionEventId,
    CorporateActionReplay, CorporateActionReplayUntil,
};
use st0x_alpaca::issuer::{
    self, ClientId, IssuerRequestId, RedeemQty, TokenSymbol, UnderlyingSymbol,
};

/// `issuer.mint_callback` request: Alpaca is told the mint went out onchain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MintCallbackRequest {
    pub tokenization_request_id: TokenizationRequestId,
    pub client_id: ClientId,
    pub wallet_address: Address,
    pub tx_hash: B256,
    pub network: Network,
}

impl From<MintCallbackRequest> for issuer::MintCallbackRequest {
    fn from(request: MintCallbackRequest) -> Self {
        Self {
            tokenization_request_id: request.tokenization_request_id,
            client_id: request.client_id,
            wallet_address: request.wallet_address,
            tx_hash: request.tx_hash,
            network: request.network,
        }
    }
}

/// `issuer.redeem` request: Alpaca is asked to redeem tokens burned onchain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedeemRequest {
    pub issuer_request_id: IssuerRequestId,
    pub underlying_symbol: UnderlyingSymbol,
    pub token_symbol: TokenSymbol,
    pub client_id: ClientId,
    /// Sent to Alpaca exactly as spelled here.
    #[serde(deserialize_with = "redeem_qty")]
    pub quantity: RedeemQty,
    pub network: Network,
    pub wallet_address: Address,
    pub tx_hash: B256,
}

impl From<RedeemRequest> for issuer::RedeemRequest {
    fn from(request: RedeemRequest) -> Self {
        Self {
            issuer_request_id: request.issuer_request_id,
            underlying: request.underlying_symbol,
            token: request.token_symbol,
            client_id: request.client_id,
            quantity: request.quantity,
            network: request.network,
            wallet: request.wallet_address,
            tx_hash: request.tx_hash,
        }
    }
}

fn redeem_qty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RedeemQty, D::Error> {
    RedeemQty::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

/// `corporate_actions.stream` query: where the relayed stream starts.
/// `sinceId` alone resumes after a cursor, `since` with an optional `until`
/// replays from an instant, and nothing starts at live events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplayQuery {
    pub since_id: Option<CorporateActionEventId>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Why a [`ReplayQuery`] names no replay position.
#[derive(Debug, thiserror::Error)]
pub enum ReplayQueryError {
    #[error("sinceId must come alone, and until only with since")]
    Combination,
    #[error(transparent)]
    Since(#[from] CorporateActionBootstrapSinceError),
}

impl TryFrom<ReplayQuery> for CorporateActionReplay {
    type Error = ReplayQueryError;

    fn try_from(query: ReplayQuery) -> Result<Self, Self::Error> {
        Ok(match (query.since_id, query.since, query.until) {
            (None, None, None) => Self::Live,
            (Some(since_id), None, None) => Self::SinceId(since_id),
            (None, Some(since), None) => {
                Self::Since(CorporateActionBootstrapSince::try_from_instant(since)?)
            }
            (None, Some(since), Some(until)) => Self::Window {
                since: CorporateActionBootstrapSince::try_from_instant(since)?,
                until: CorporateActionReplayUntil::at(until),
            },
            _ => return Err(ReplayQueryError::Combination),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redeem_reaches_alpaca_with_the_callers_quantity_spelling() {
        let request: RedeemRequest = serde_json::from_value(json!({
            "issuerRequestId": "0xabc",
            "underlyingSymbol": "AAPL",
            "tokenSymbol": "tAAPL",
            "clientId": "904837e3-3b76-47ec-b432-046db621571b",
            "quantity": "1.50",
            "network": "base",
            "walletAddress": Address::repeat_byte(1),
            "txHash": B256::repeat_byte(2),
        }))
        .unwrap();

        let alpaca = serde_json::to_value(issuer::RedeemRequest::from(request)).unwrap();

        assert_eq!(alpaca["qty"], "1.50");
        assert_eq!(alpaca["underlying_symbol"], "AAPL");
        assert_eq!(alpaca["token_symbol"], "tAAPL");
        assert_eq!(alpaca["wallet_address"], json!(Address::repeat_byte(1)));
    }

    #[test]
    fn replay_is_a_cursor_alone_or_since_with_an_optional_until() {
        let cursor = CorporateActionEventId::new("01J0000000000000000000000A").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let until = Utc::now();
        let replay = |since_id: Option<&CorporateActionEventId>, since, until| {
            CorporateActionReplay::try_from(ReplayQuery {
                since_id: since_id.cloned(),
                since,
                until,
            })
        };

        assert_eq!(
            replay(None, None, None).unwrap(),
            CorporateActionReplay::Live
        );
        assert_eq!(
            replay(Some(&cursor), None, None).unwrap(),
            CorporateActionReplay::SinceId(cursor.clone())
        );
        assert!(matches!(
            replay(None, Some(since), None).unwrap(),
            CorporateActionReplay::Since(_)
        ));
        assert!(matches!(
            replay(None, Some(since), Some(until)).unwrap(),
            CorporateActionReplay::Window { .. }
        ));

        for (since_id, since, until) in [
            (Some(&cursor), Some(since), None),
            (Some(&cursor), None, Some(until)),
            (None, None, Some(until)),
        ] {
            assert!(matches!(
                replay(since_id, since, until),
                Err(ReplayQueryError::Combination)
            ));
        }
        assert!(matches!(
            replay(None, Some(Utc::now() + chrono::Duration::days(1)), None),
            Err(ReplayQueryError::Since(_))
        ));
    }
}
