//! `account.*` and `activities.list`.

use chrono::{DateTime, Utc};
use rain_math_float::Float;
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{AccountActivity, AccountFunds, EquityPosition, Inventory};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usd, Usdc};

/// `account.funds`: account level USD figures, in cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundsResponse {
    pub balance_cents: i64,
    pub buying_power_cents: i64,
    pub withdrawable_cents: Option<i64>,
}

impl From<AccountFunds> for FundsResponse {
    fn from(funds: AccountFunds) -> Self {
        Self {
            balance_cents: funds.balance,
            buying_power_cents: funds.buying_power,
            withdrawable_cents: funds.withdrawable,
        }
    }
}

impl From<FundsResponse> for AccountFunds {
    fn from(funds: FundsResponse) -> Self {
        Self {
            balance: funds.balance_cents,
            buying_power: funds.buying_power_cents,
            withdrawable: funds.withdrawable_cents,
        }
    }
}

/// `account.withdrawable_cash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawableCashResponse {
    pub withdrawable_cents: Option<i64>,
}

/// One equity position.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub symbol: Symbol,
    pub quantity: FractionalShares,
    #[serde(
        default,
        serialize_with = "st0x_float_serde::serialize_option_float",
        deserialize_with = "st0x_float_serde::deserialize_option_float_from_number_or_string"
    )]
    pub market_value: Option<Float>,
}

/// `account.inventory`: positions and cash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryResponse {
    pub positions: Vec<Position>,
    pub alpaca_usdc: Option<Usdc>,
    pub usd_balance_cents: i64,
    pub cash_buying_power_cents: Option<i64>,
    pub cash_withdrawable_cents: Option<i64>,
}

impl From<Inventory> for InventoryResponse {
    fn from(inventory: Inventory) -> Self {
        Self {
            positions: inventory
                .positions
                .into_iter()
                .map(|position| Position {
                    symbol: position.symbol,
                    quantity: position.quantity,
                    market_value: position.market_value,
                })
                .collect(),
            alpaca_usdc: inventory.alpaca_usdc,
            usd_balance_cents: inventory.usd_balance_cents,
            cash_buying_power_cents: inventory.cash_buying_power_cents,
            cash_withdrawable_cents: inventory.cash_withdrawable_cents,
        }
    }
}

impl From<InventoryResponse> for Inventory {
    fn from(inventory: InventoryResponse) -> Self {
        Self {
            positions: inventory
                .positions
                .into_iter()
                .map(|position| EquityPosition {
                    symbol: position.symbol,
                    quantity: position.quantity,
                    market_value: position.market_value,
                })
                .collect(),
            alpaca_usdc: inventory.alpaca_usdc,
            usd_balance_cents: inventory.usd_balance_cents,
            cash_buying_power_cents: inventory.cash_buying_power_cents,
            cash_withdrawable_cents: inventory.cash_withdrawable_cents,
        }
    }
}

/// `account.position_mark`: `None` when the account holds no position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionMarkResponse {
    pub mark: Option<Positive<Usd>>,
}

/// `activities.list` query. The account filter comes from the deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivitiesQuery {
    /// Comma separated Alpaca activity types, for example `FEE,DIV`.
    pub types: String,
    pub after: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// `activities.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitiesResponse {
    pub activities: Vec<AccountActivity>,
}
