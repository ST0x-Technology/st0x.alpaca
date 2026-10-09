//! `account.*` and `activities.list`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::broker::AccountActivity;
use st0x_alpaca::st0x_finance::{Positive, Usd};

/// `account.withdrawable_cash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawableCashResponse {
    pub withdrawable_cents: Option<i64>,
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

/// `activities.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitiesResponse {
    pub activities: Vec<AccountActivity>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
