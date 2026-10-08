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
use st0x_alpaca::st0x_finance::Positive;

/// `issuer.mint_callback` request: Alpaca is told the mint went out onchain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MintCallbackRequest {
    #[serde(deserialize_with = "tokenization_request_id")]
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
    #[serde(deserialize_with = "issuer_request_id")]
    pub issuer_request_id: IssuerRequestId,
    #[serde(deserialize_with = "underlying_symbol")]
    pub underlying_symbol: UnderlyingSymbol,
    #[serde(deserialize_with = "token_symbol")]
    pub token_symbol: TokenSymbol,
    pub client_id: ClientId,
    /// Strictly positive, sent to Alpaca exactly as spelled here.
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

/// `issuer.request` path: the tokenization request to read, under the same
/// rule as the body field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestPath {
    #[serde(deserialize_with = "tokenization_request_id")]
    pub tokenization_request_id: TokenizationRequestId,
}

/// Reads a request id: not blank, without control characters, and at most
/// [`super::REQUEST_ID_MAX`] characters.
fn request_id<'de, D: Deserializer<'de>>(field: &str, deserializer: D) -> Result<String, D::Error> {
    let raw = super::bounded(field, super::REQUEST_ID_MAX, deserializer)?;
    if raw.trim().is_empty() {
        return Err(serde::de::Error::custom(format_args!(
            "{field} must not be blank"
        )));
    }
    if raw.chars().any(char::is_control) {
        return Err(serde::de::Error::custom(format_args!(
            "{field} must not contain control characters"
        )));
    }
    Ok(raw)
}

fn tokenization_request_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<TokenizationRequestId, D::Error> {
    request_id("tokenizationRequestId", deserializer).map(TokenizationRequestId)
}

fn issuer_request_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<IssuerRequestId, D::Error> {
    request_id("issuerRequestId", deserializer).map(IssuerRequestId)
}

fn underlying_symbol<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<UnderlyingSymbol, D::Error> {
    super::symbol(deserializer).map(UnderlyingSymbol)
}

fn token_symbol<'de, D: Deserializer<'de>>(deserializer: D) -> Result<TokenSymbol, D::Error> {
    super::safe_symbol(deserializer).map(TokenSymbol)
}

const REDEEM_QTY_MAX_SCALE: usize = 9;

fn redeem_qty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RedeemQty, D::Error> {
    let wire = String::deserialize(deserializer)?;
    if !plain_decimal_with_max_scale(&wire, REDEEM_QTY_MAX_SCALE) {
        return Err(serde::de::Error::custom(format_args!(
            "quantity must be a plain decimal with at most {REDEEM_QTY_MAX_SCALE} fractional digits"
        )));
    }
    let quantity = RedeemQty::new(wire).map_err(serde::de::Error::custom)?;
    Positive::new(quantity.shares().0).map_err(serde::de::Error::custom)?;
    Ok(quantity)
}

fn plain_decimal_with_max_scale(raw: &str, max_scale: usize) -> bool {
    let (whole, fraction) = raw
        .split_once('.')
        .map_or((raw, None), |(whole, fraction)| (whole, Some(fraction)));
    !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty()
                && fraction.len() <= max_scale
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
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

    fn redeem_request(issuer_request_id: &str, quantity: &str) -> serde_json::Value {
        json!({
            "issuerRequestId": issuer_request_id,
            "underlyingSymbol": "AAPL",
            "tokenSymbol": "tAAPL",
            "clientId": "904837e3-3b76-47ec-b432-046db621571b",
            "quantity": quantity,
            "network": "base",
            "walletAddress": Address::repeat_byte(1),
            "txHash": B256::repeat_byte(2),
        })
    }

    #[test]
    fn redeem_reaches_alpaca_with_the_callers_quantity_spelling() {
        let request: RedeemRequest =
            serde_json::from_value(redeem_request("0xabc", "1.50")).unwrap();

        let alpaca = serde_json::to_value(issuer::RedeemRequest::from(request)).unwrap();

        assert_eq!(alpaca["qty"], "1.50");
        assert_eq!(alpaca["underlying_symbol"], "AAPL");
        assert_eq!(alpaca["token_symbol"], "tAAPL");
        assert_eq!(alpaca["wallet_address"], json!(Address::repeat_byte(1)));
    }

    #[test]
    fn redeem_quantity_is_a_plain_decimal_with_at_most_nine_fractional_digits() {
        for accepted in ["1", "1.0", "0.000000001", "0001.500000000"] {
            serde_json::from_value::<RedeemRequest>(redeem_request("0xabc", accepted)).unwrap();
        }

        for refused in ["1e2", "1e-37", "0.0000000000001", ".1", "1."] {
            let error = serde_json::from_value::<RedeemRequest>(redeem_request("0xabc", refused))
                .unwrap_err();
            assert!(
                error.to_string().contains("plain decimal"),
                "{refused}: {error}"
            );
        }
    }

    #[test]
    fn request_ids_refuse_control_characters() {
        for refused in ["0xabc\n", "0xabc\0", "0xabc\u{7f}"] {
            let redeem_error =
                serde_json::from_value::<RedeemRequest>(redeem_request(refused, "1")).unwrap_err();
            assert!(
                redeem_error.to_string().contains("control characters"),
                "{redeem_error}"
            );

            let path_error = serde_json::from_value::<RequestPath>(json!({
                "tokenization_request_id": refused,
            }))
            .unwrap_err();
            assert!(
                path_error.to_string().contains("control characters"),
                "{path_error}"
            );
        }
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
