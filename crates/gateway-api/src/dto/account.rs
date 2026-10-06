//! `account.*` and `activities.list`.

use chrono::{DateTime, NaiveDate, Utc};
use rain_math_float::Float;
use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::broker::{AccountActivity, AccountFunds, EquityPosition, Inventory};
use st0x_alpaca::st0x_finance::{FractionalShares, Positive, Symbol, Usd, Usdc};
use uuid::Uuid;

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
    /// Comma separated Alpaca activity types, for example `FEE,DIV`. Must
    /// name at least one type: without one Alpaca answers every activity of
    /// the account.
    #[serde(deserialize_with = "activity_types")]
    pub types: String,
    pub after: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

fn activity_types<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let types = String::deserialize(deserializer)?;
    if types.split(',').all(|kind| kind.trim().is_empty()) {
        return Err(serde::de::Error::custom(
            "types must name at least one activity type",
        ));
    }
    Ok(types)
}

/// One account activity row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityResponse {
    pub id: String,
    pub activity_type: String,
    pub activity_sub_type: Option<String>,
    pub date: Option<NaiveDate>,
    pub created_at: Option<DateTime<Utc>>,
    pub net_amount: Option<String>,
    pub symbol: Option<String>,
    pub qty: Option<String>,
    pub per_share_amount: Option<String>,
    pub price: Option<String>,
    pub side: Option<String>,
    pub order_id: Option<Uuid>,
    pub transaction_time: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub group_id: Option<String>,
    pub currency: Option<String>,
}

impl From<AccountActivity> for ActivityResponse {
    fn from(activity: AccountActivity) -> Self {
        Self {
            id: activity.id,
            activity_type: activity.activity_type,
            activity_sub_type: activity.activity_sub_type,
            date: activity.date,
            created_at: activity.created_at,
            net_amount: activity.net_amount,
            symbol: activity.symbol,
            qty: activity.qty,
            per_share_amount: activity.per_share_amount,
            price: activity.price,
            side: activity.side,
            order_id: activity.order_id,
            transaction_time: activity.transaction_time,
            description: activity.description,
            status: activity.status,
            group_id: activity.group_id,
            currency: activity.currency,
        }
    }
}

impl From<ActivityResponse> for AccountActivity {
    fn from(activity: ActivityResponse) -> Self {
        Self {
            id: activity.id,
            activity_type: activity.activity_type,
            activity_sub_type: activity.activity_sub_type,
            date: activity.date,
            created_at: activity.created_at,
            net_amount: activity.net_amount,
            symbol: activity.symbol,
            qty: activity.qty,
            per_share_amount: activity.per_share_amount,
            price: activity.price,
            side: activity.side,
            order_id: activity.order_id,
            transaction_time: activity.transaction_time,
            description: activity.description,
            status: activity.status,
            group_id: activity.group_id,
            currency: activity.currency,
        }
    }
}

/// `activities.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitiesResponse {
    pub activities: Vec<ActivityResponse>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::through_wire;
    use super::*;

    #[test]
    fn an_activities_query_must_name_a_type() {
        for blank in ["", " ", ",", " , ,"] {
            assert!(
                serde_json::from_value::<ActivitiesQuery>(json!({ "types": blank })).is_err(),
                "{blank:?}"
            );
        }
        let query: ActivitiesQuery = serde_json::from_value(json!({ "types": "FEE,DIV" })).unwrap();
        assert_eq!(query.types, "FEE,DIV");
    }

    #[test]
    fn an_activity_comes_back_from_the_wire_as_alpaca_reported_it() {
        let alpaca: AccountActivity = serde_json::from_value(json!({
            "id": "20261006000000000::abc",
            "activity_type": "DIV",
            "activity_sub_type": "QDIV",
            "date": "2026-10-06",
            "created_at": "2026-10-06T10:00:00Z",
            "net_amount": "12.34",
            "symbol": "AAPL",
            "qty": "10",
            "per_share_amount": "1.234",
            "price": null,
            "side": null,
            "order_id": "7b3f5c1e-2d4a-4b6c-8e9f-0a1b2c3d4e5f",
            "transaction_time": "2026-10-06T10:00:01Z",
            "description": "dividend",
            "status": "executed",
            "group_id": "g1",
            "currency": "USD"
        }))
        .unwrap();
        let wire = ActivityResponse::from(alpaca.clone());

        let relayed = AccountActivity::from(through_wire(&wire));

        assert_eq!(
            serde_json::to_value(relayed).unwrap(),
            serde_json::to_value(alpaca).unwrap()
        );
        assert_eq!(serde_json::to_value(&wire).unwrap()["netAmount"], "12.34");
    }

    #[test]
    fn funds_and_inventory_come_back_from_the_wire_unchanged() {
        let funds: FundsResponse = serde_json::from_value(json!({
            "balanceCents": 150_025,
            "buyingPowerCents": 140_000,
            "withdrawableCents": null
        }))
        .unwrap();
        assert_eq!(
            FundsResponse::from(AccountFunds::from(through_wire(&funds))),
            funds
        );

        let inventory: InventoryResponse = serde_json::from_value(json!({
            "positions": [
                { "symbol": "AAPL", "quantity": "2.5", "marketValue": "500.5" },
                { "symbol": "BRK.B", "quantity": "-1", "marketValue": null }
            ],
            "alpacaUsdc": "12.345678",
            "usdBalanceCents": 150_025,
            "cashBuyingPowerCents": 140_000,
            "cashWithdrawableCents": null
        }))
        .unwrap();
        let relayed = InventoryResponse::from(Inventory::from(through_wire(&inventory)));
        assert_eq!(
            serde_json::to_value(relayed).unwrap(),
            serde_json::to_value(inventory).unwrap()
        );
    }
}
