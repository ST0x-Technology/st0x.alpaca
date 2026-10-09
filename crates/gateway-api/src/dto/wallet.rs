//! `wallet.*` and `journals.create`.
//!
//! Every pinned value (withdrawal destinations the bot may use, the Travel
//! Rule beneficiary, journal counterparty accounts) comes from the
//! deployment's config. No request here carries an account id or Travel Rule
//! info; unknown fields are refused.

use alloy_primitives::Address;
use chrono::NaiveDate;
use rain_math_float::Float;
use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::broker::{JournalResponse, JournalStatus};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usdc};
use st0x_alpaca::wallet::{Network, TokenSymbol, Transfer, WhitelistEntry};
use uuid::Uuid;

/// `wallet.withdraw` request. The address must hold an approved whitelist
/// entry; the bot tier may only use the deployment's pinned destinations and
/// the write tier may use any other approved address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WithdrawRequest {
    pub amount: Positive<Usdc>,
    #[serde(deserialize_with = "asset")]
    pub asset: TokenSymbol,
    pub address: Address,
    /// Caller chosen id for the audit record. Alpaca does not dedupe
    /// withdrawals, so sending the same id twice withdraws twice.
    pub operation_id: Uuid,
    /// Required on the write tier.
    pub reason: Option<String>,
}

/// `wallet.transfer`: one transfer with the fees Alpaca reports for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferResponse {
    #[serde(flatten)]
    pub transfer: Transfer,
    /// Network fee plus Alpaca's fee, in USDC. `None` when Alpaca omits
    /// either one or the transfer is not in USDC.
    pub reported_fees: Option<Usdc>,
}

/// `wallet.transfers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransfersResponse {
    pub transfers: Vec<Transfer>,
}

/// `wallet.find_deposit`: `None` when the transfer list holds no incoming
/// transfer with the hash. The list may be capped, so `None` is not proof
/// the deposit does not exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositResponse {
    pub deposit: Option<Transfer>,
}

/// `wallet.deposit_address` query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepositAddressQuery {
    #[serde(deserialize_with = "asset")]
    pub asset: TokenSymbol,
    #[serde(deserialize_with = "network")]
    pub network: Network,
}

/// Reads a requested asset ticker of at most [`super::SYMBOL_MAX`]
/// characters.
fn asset<'de, D>(deserializer: D) -> Result<TokenSymbol, D::Error>
where
    D: Deserializer<'de>,
{
    super::bounded("asset", super::SYMBOL_MAX, deserializer).map(TokenSymbol)
}

/// Reads a requested network name of at most [`super::NETWORK_MAX`]
/// characters.
fn network<'de, D>(deserializer: D) -> Result<Network, D::Error>
where
    D: Deserializer<'de>,
{
    super::bounded("network", super::NETWORK_MAX, deserializer).map(Network::from)
}

/// `wallet.deposit_address`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositAddressResponse {
    pub address: Address,
}

/// `wallet.whitelist`, and the entries `wallet.whitelist_remove` and
/// `wallet.whitelist_patch_travel_rule` touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhitelistResponse {
    pub entries: Vec<WhitelistEntry>,
}

/// `wallet.whitelist_create` request. The Travel Rule beneficiary comes from
/// the deployment's config, never from the request. There is no network:
/// Alpaca takes the chain from the asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WhitelistCreateRequest {
    pub address: Address,
    #[serde(deserialize_with = "asset")]
    pub asset: TokenSymbol,
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// `wallet.whitelist_remove` request. Removes every entry for the address in
/// the path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WhitelistRemoveRequest {
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// `wallet.whitelist_patch_travel_rule` request. Attaches the configured
/// Travel Rule beneficiary to every whitelist entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TravelRulePatchRequest {
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// `journals.create` request. `counterparty` is a name from the deployment's
/// config, read by [`super::counterparty`]; the account id behind it never
/// crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalCreateRequest {
    #[serde(deserialize_with = "super::counterparty")]
    pub counterparty: String,
    #[serde(deserialize_with = "super::symbol")]
    pub symbol: Symbol,
    pub qty: Positive<FractionalShares>,
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// `journals.create`. Carries no account id, neither ours nor the
/// counterparty's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalCreateResponse {
    pub id: Uuid,
    pub status: JournalStatus,
    pub symbol: Symbol,
    pub quantity: Positive<FractionalShares>,
    #[serde(
        default,
        serialize_with = "st0x_float_serde::serialize_option_float",
        deserialize_with = "st0x_float_serde::deserialize_option_float_from_number_or_string"
    )]
    pub price: Option<Float>,
    pub settle_date: Option<NaiveDate>,
}

impl From<JournalResponse> for JournalCreateResponse {
    fn from(journal: JournalResponse) -> Self {
        Self {
            id: journal.id,
            status: journal.status,
            symbol: journal.symbol,
            quantity: journal.quantity,
            price: journal.price,
            settle_date: journal.settle_date,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn journal(counterparty: &str) -> serde_json::Result<JournalCreateRequest> {
        serde_json::from_value(json!({
            "counterparty": counterparty,
            "symbol": "AAPL",
            "qty": "5",
            "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"
        }))
    }

    #[test]
    fn a_journal_names_its_counterparty_in_config_key_form() {
        let longest = "c".repeat(super::super::COUNTERPARTY_MAX);
        for accepted in ["issuer", "Issuer_2", "market-maker", longest.as_str()] {
            assert_eq!(journal(accepted).unwrap().counterparty, accepted);
            assert!(super::super::is_counterparty_name(accepted), "{accepted:?}");
        }

        let overlong = "c".repeat(super::super::COUNTERPARTY_MAX + 1);
        for refused in [
            "",
            " ",
            "issuer ",
            "a.b",
            "a/b",
            "ïssuer",
            overlong.as_str(),
        ] {
            assert!(journal(refused).is_err(), "{refused:?} was accepted");
            assert!(!super::super::is_counterparty_name(refused), "{refused:?}");
        }
        let error = journal(&overlong).unwrap_err();
        assert!(!error.to_string().contains(&overlong), "{error}");
    }

    #[test]
    fn requested_assets_and_networks_are_bounded_in_length() {
        let asset = |asset: &str| {
            serde_json::from_value::<WithdrawRequest>(json!({
                "amount": "1",
                "asset": asset,
                "address": "0x1111111111111111111111111111111111111111",
                "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"
            }))
        };
        let whitelisted = |asset: &str| {
            serde_json::from_value::<WhitelistCreateRequest>(json!({
                "address": "0x1111111111111111111111111111111111111111",
                "asset": asset,
                "operationId": "6a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"
            }))
        };
        let deposit = |asset: &str, network: &str| {
            serde_json::from_value::<DepositAddressQuery>(
                json!({ "asset": asset, "network": network }),
            )
        };
        let longest = "U".repeat(super::super::SYMBOL_MAX);
        let overlong = "U".repeat(super::super::SYMBOL_MAX + 1);
        let longest_network = "E".repeat(super::super::NETWORK_MAX);
        let overlong_network = "E".repeat(super::super::NETWORK_MAX + 1);

        assert_eq!(asset(&longest).unwrap().asset.as_ref(), longest);
        assert_eq!(whitelisted(&longest).unwrap().asset.as_ref(), longest);
        let query = deposit(&longest, &longest_network).unwrap();
        assert_eq!(query.asset.as_ref(), longest);
        // The network is still read case insensitively.
        assert_eq!(query.network.as_ref(), longest_network.to_lowercase());

        assert!(asset(&overlong).is_err());
        assert!(whitelisted(&overlong).is_err());
        assert!(deposit(&overlong, "ethereum").is_err());
        let error = deposit("USDC", &overlong_network).unwrap_err();
        assert!(!error.to_string().contains(&overlong_network), "{error}");
    }
}
