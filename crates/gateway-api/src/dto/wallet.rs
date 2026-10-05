//! `wallet.*` and `journals.create`.
//!
//! Every pinned value (withdrawal destinations the bot may use, the Travel
//! Rule beneficiary, journal counterparty accounts) comes from the
//! deployment's config. No request here carries an account id or Travel Rule
//! info; unknown fields are refused.

use alloy_primitives::{Address, TxHash};
use chrono::{DateTime, NaiveDate, Utc};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{AlpacaAmount, JournalResponse, JournalStatus};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usdc, UsdcConversionError};
use st0x_alpaca::wallet::{
    AlpacaTransferId, Network, TokenSymbol, TransferDirection, TransferStatus, WhitelistEntry,
};
use uuid::Uuid;

/// One crypto transfer, in or out of the account's wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: AlpacaTransferId,
    /// Onchain transaction hash, once Alpaca reports one.
    pub tx_hash: Option<TxHash>,
    pub direction: TransferDirection,
    /// Floored to USDC's six decimal onchain grid.
    pub amount: Usdc,
    pub chain: String,
    pub asset: TokenSymbol,
    pub from_address: Address,
    pub to_address: Address,
    pub status: TransferStatus,
    pub created_at: DateTime<Utc>,
}

impl From<st0x_alpaca::wallet::Transfer> for Transfer {
    fn from(transfer: st0x_alpaca::wallet::Transfer) -> Self {
        Self {
            id: transfer.id,
            tx_hash: transfer.tx,
            direction: transfer.direction,
            amount: Usdc::new(transfer.amount.into_normalized()),
            chain: transfer.chain,
            asset: transfer.asset,
            from_address: transfer.from,
            to_address: transfer.to,
            status: transfer.status,
            created_at: transfer.created_at,
        }
    }
}

impl TryFrom<Transfer> for st0x_alpaca::wallet::Transfer {
    type Error = UsdcConversionError;

    fn try_from(transfer: Transfer) -> Result<Self, Self::Error> {
        Ok(Self {
            id: transfer.id,
            tx: transfer.tx_hash,
            direction: transfer.direction,
            amount: AlpacaAmount::try_from(transfer.amount.inner())?,
            chain: transfer.chain,
            asset: transfer.asset,
            from: transfer.from_address,
            to: transfer.to_address,
            status: transfer.status,
            created_at: transfer.created_at,
        })
    }
}

/// `wallet.withdraw` request. The address must hold an approved whitelist
/// entry; the bot tier may only use the deployment's pinned destinations and
/// the write tier may use any other approved address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WithdrawRequest {
    pub amount: Positive<Usdc>,
    pub asset: TokenSymbol,
    pub address: Address,
    /// Caller chosen id for the audit record. Alpaca does not dedupe
    /// withdrawals, so sending the same id twice withdraws twice.
    pub operation_id: Uuid,
    /// Required on the write tier.
    pub reason: Option<String>,
}

/// `wallet.transfer` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferIdPath {
    pub transfer_id: Uuid,
}

/// `wallet.transfer`: one transfer with the fees Alpaca reports for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferResponse {
    #[serde(flatten)]
    pub transfer: Transfer,
    /// Network fee plus Alpaca's fee, in USDC. `None` when Alpaca omits
    /// either one or the transfer is not in USDC.
    pub reported_fees: Option<Usdc>,
}

/// `wallet.transfers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransfersResponse {
    pub transfers: Vec<Transfer>,
}

/// `wallet.find_deposit` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxHashPath {
    pub tx_hash: TxHash,
}

/// `wallet.find_deposit`: `None` when the transfer list holds no incoming
/// transfer with the hash. The list may be capped, so `None` is not proof
/// the deposit does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositResponse {
    pub deposit: Option<Transfer>,
}

/// `wallet.deposit_address` query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepositAddressQuery {
    pub asset: TokenSymbol,
    pub network: Network,
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
/// the deployment's config, never from the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WhitelistCreateRequest {
    pub address: Address,
    pub asset: TokenSymbol,
    pub network: Network,
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// `wallet.whitelist_remove` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressPath {
    pub address: Address,
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
/// config; the account id behind it never crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalCreateRequest {
    pub counterparty: String,
    pub symbol: Symbol,
    pub qty: Positive<FractionalShares>,
    pub operation_id: Uuid,
    pub reason: Option<String>,
}

/// Alpaca journal status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    Queued,
    SentToClearing,
    Pending,
    Executed,
    Rejected,
    Canceled,
    Refused,
    Deleted,
    Correct,
}

impl From<JournalStatus> for JournalState {
    fn from(status: JournalStatus) -> Self {
        match status {
            JournalStatus::Queued => Self::Queued,
            JournalStatus::SentToClearing => Self::SentToClearing,
            JournalStatus::Pending => Self::Pending,
            JournalStatus::Executed => Self::Executed,
            JournalStatus::Rejected => Self::Rejected,
            JournalStatus::Canceled => Self::Canceled,
            JournalStatus::Refused => Self::Refused,
            JournalStatus::Deleted => Self::Deleted,
            JournalStatus::Correct => Self::Correct,
        }
    }
}

impl From<JournalState> for JournalStatus {
    fn from(state: JournalState) -> Self {
        match state {
            JournalState::Queued => Self::Queued,
            JournalState::SentToClearing => Self::SentToClearing,
            JournalState::Pending => Self::Pending,
            JournalState::Executed => Self::Executed,
            JournalState::Rejected => Self::Rejected,
            JournalState::Canceled => Self::Canceled,
            JournalState::Refused => Self::Refused,
            JournalState::Deleted => Self::Deleted,
            JournalState::Correct => Self::Correct,
        }
    }
}

/// `journals.create`. Carries no account id, neither ours nor the
/// counterparty's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalCreateResponse {
    pub id: Uuid,
    pub status: JournalState,
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
            status: journal.status.into(),
            symbol: journal.symbol,
            quantity: journal.quantity,
            price: journal.price,
            settle_date: journal.settle_date,
        }
    }
}
