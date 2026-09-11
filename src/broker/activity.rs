//! Alpaca Broker account activity retrieval.
//!
//! Fetches immutable broker activity rows for the configured account. It
//! does not place orders, mutate account state, or participate in
//! hot-path execution.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{BrokerApiError, get_json};
use crate::core::AlpacaClient;

const ACCOUNT_ACTIVITIES_PAGE_SIZE: usize = 100;

#[cfg(not(test))]
const MAX_ACCOUNT_ACTIVITIES_PAGES: usize = 1000;
#[cfg(test)]
const MAX_ACCOUNT_ACTIVITIES_PAGES: usize = 3;

/// Server-side filter for the account-activities endpoint.
///
/// The Broker API docs list `activity_types` as the server-side filter and
/// enumerate supported activity codes on the specific-type endpoint:
/// <https://docs.alpaca.markets/us/reference/getaccountactivities>
/// <https://docs.alpaca.markets/us/reference/getaccountactivitiesbytype>
#[derive(Debug, Clone)]
pub struct AccountActivitiesQuery {
    pub activity_types: Vec<String>,
    pub after: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// A single immutable account-activity row as Alpaca returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountActivity {
    pub id: String,
    pub activity_type: String,
    #[serde(default)]
    pub activity_sub_type: Option<String>,
    #[serde(default)]
    pub date: Option<NaiveDate>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub net_amount: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub qty: Option<String>,
    #[serde(default)]
    pub per_share_amount: Option<String>,
    #[serde(default)]
    pub price: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub order_id: Option<Uuid>,
    #[serde(default)]
    pub transaction_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
}

/// Fetches every account activity matching `query`, following Alpaca's
/// `page_token` pagination in ascending order.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport or API failures,
/// [`BrokerApiError::AccountActivitiesPaginationInvariantViolation`] if
/// Alpaca returns the same page token twice, and
/// [`BrokerApiError::AccountActivitiesPageLimitExceeded`] if pagination
/// does not terminate within the page budget.
pub async fn get_account_activities(
    client: &AlpacaClient,
    query: &AccountActivitiesQuery,
) -> Result<Vec<AccountActivity>, BrokerApiError> {
    let mut rows = Vec::new();
    let mut page_token: Option<String> = None;

    for _ in 0..MAX_ACCOUNT_ACTIVITIES_PAGES {
        let page = fetch_account_activities_page(client, query, page_token.as_deref()).await?;
        if page.is_empty() {
            return Ok(rows);
        }

        // Alpaca documents `page_token` as the ID of the last item from the
        // current page; with `direction=asc`, the next page begins
        // immediately after that activity.
        let last_id = page.last().map(|row| row.id.clone());
        let page_len = page.len();
        rows.extend(page);

        if page_len < ACCOUNT_ACTIVITIES_PAGE_SIZE {
            return Ok(rows);
        }

        if page_token == last_id {
            return Err(BrokerApiError::AccountActivitiesPaginationInvariantViolation);
        }

        page_token = last_id;
    }

    Err(BrokerApiError::AccountActivitiesPageLimitExceeded {
        pages: MAX_ACCOUNT_ACTIVITIES_PAGES,
    })
}

async fn fetch_account_activities_page(
    client: &AlpacaClient,
    query: &AccountActivitiesQuery,
    page_token: Option<&str>,
) -> Result<Vec<AccountActivity>, BrokerApiError> {
    let base = format!("{}/v1/accounts/activities", client.base_url());
    let mut url = reqwest::Url::parse(&base).map_err(|error| {
        BrokerApiError::InvalidAccountActivitiesUrl {
            url: base.clone(),
            source: error,
        }
    })?;

    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("account_id", client.account_id());
        pairs.append_pair("direction", "asc");
        pairs.append_pair("page_size", &ACCOUNT_ACTIVITIES_PAGE_SIZE.to_string());

        if !query.activity_types.is_empty() {
            pairs.append_pair("activity_types", &query.activity_types.join(","));
        }

        if let Some(after) = query.after {
            pairs.append_pair("after", &after.to_rfc3339());
        }

        if let Some(until) = query.until {
            pairs.append_pair("until", &until.to_rfc3339());
        }

        if let Some(page_token) = page_token {
            pairs.append_pair("page_token", page_token);
        }
    }

    get_json(client, url.as_str()).await
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use httpmock::prelude::*;

    use super::super::{TEST_ACCOUNT_ID, test_client};
    use super::*;

    #[tokio::test]
    async fn fetches_account_activities_with_pagination() {
        let server = MockServer::start();
        let second_page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/activities")
                .query_param("account_id", TEST_ACCOUNT_ID)
                .query_param("page_token", "20260600000000000099::fee");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!([
                    {
                        "activity_type": "DIV",
                        "id": "20260604000000000::div",
                        "date": "2026-06-04",
                        "net_amount": "1.25",
                        "symbol": "SGOV"
                    }
                ]));
        });
        let first_page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/activities")
                .query_param("account_id", TEST_ACCOUNT_ID)
                .query_param("activity_types", "FEE,DIV")
                .query_param("direction", "asc")
                .query_param("page_size", ACCOUNT_ACTIVITIES_PAGE_SIZE.to_string());
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!(
                    (0..ACCOUNT_ACTIVITIES_PAGE_SIZE)
                        .map(|idx| serde_json::json!({
                            "activity_type": "FEE",
                            "id": format!("202606{idx:014}::fee"),
                            "date": "2026-06-03",
                            "net_amount": "-0.01"
                        }))
                        .collect::<Vec<_>>()
                ));
        });

        let client = test_client(server.base_url());
        let rows = get_account_activities(
            &client,
            &AccountActivitiesQuery {
                activity_types: vec!["FEE".to_string(), "DIV".to_string()],
                after: None,
                until: None,
            },
        )
        .await
        .unwrap();

        first_page.assert();
        second_page.assert();
        assert_eq!(rows.len(), ACCOUNT_ACTIVITIES_PAGE_SIZE + 1);
        assert_eq!(rows.last().unwrap().activity_type, "DIV");
        assert_eq!(rows.last().unwrap().symbol.as_deref(), Some("SGOV"));
    }

    #[tokio::test]
    async fn sends_after_and_until_filters() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/activities")
                .query_param("after", "2026-06-03T00:00:00+00:00")
                .query_param("until", "2026-06-04T00:00:00+00:00");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!([]));
        });

        let client = test_client(server.base_url());
        get_account_activities(
            &client,
            &AccountActivitiesQuery {
                activity_types: Vec::new(),
                after: Utc.with_ymd_and_hms(2026, 6, 3, 0, 0, 0).single(),
                until: Utc.with_ymd_and_hms(2026, 6, 4, 0, 0, 0).single(),
            },
        )
        .await
        .unwrap();

        mock.assert();
    }

    #[tokio::test]
    async fn rejects_repeated_account_activities_page_token() {
        let server = MockServer::start();
        let second_page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/activities")
                .query_param("account_id", TEST_ACCOUNT_ID)
                .query_param("page_token", "repeated-token");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(full_page("repeated-token"));
        });
        let first_page = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/activities")
                .query_param("account_id", TEST_ACCOUNT_ID)
                .query_param("page_size", ACCOUNT_ACTIVITIES_PAGE_SIZE.to_string());
            then.status(200)
                .header("content-type", "application/json")
                .json_body(full_page("repeated-token"));
        });

        let client = test_client(server.base_url());
        let error = get_account_activities(
            &client,
            &AccountActivitiesQuery {
                activity_types: Vec::new(),
                after: None,
                until: None,
            },
        )
        .await
        .unwrap_err();

        first_page.assert();
        second_page.assert();
        assert!(matches!(
            error,
            BrokerApiError::AccountActivitiesPaginationInvariantViolation
        ));
    }

    #[tokio::test]
    async fn rejects_account_activities_after_page_limit() {
        let server = MockServer::start();
        let mut mocks = Vec::new();
        for page_index in 0..MAX_ACCOUNT_ACTIVITIES_PAGES {
            let page_token = page_index
                .checked_sub(1)
                .map(|previous| format!("page-{previous}-last"));
            let last_id = format!("page-{page_index}-last");
            let mock = server.mock(|when, then| {
                let when = when
                    .method(GET)
                    .path("/v1/accounts/activities")
                    .query_param("account_id", TEST_ACCOUNT_ID)
                    .query_param("page_size", ACCOUNT_ACTIVITIES_PAGE_SIZE.to_string());
                if let Some(page_token) = &page_token {
                    when.query_param("page_token", page_token);
                } else {
                    when.query_param_missing("page_token");
                }
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(full_page(&last_id));
            });
            mocks.push(mock);
        }

        let client = test_client(server.base_url());
        let error = get_account_activities(
            &client,
            &AccountActivitiesQuery {
                activity_types: Vec::new(),
                after: None,
                until: None,
            },
        )
        .await
        .unwrap_err();

        for mock in mocks {
            mock.assert();
        }
        assert!(matches!(
            error,
            BrokerApiError::AccountActivitiesPageLimitExceeded {
                pages: MAX_ACCOUNT_ACTIVITIES_PAGES
            }
        ));
    }

    fn full_page(last_id: &str) -> serde_json::Value {
        serde_json::json!(
            (0..ACCOUNT_ACTIVITIES_PAGE_SIZE)
                .map(|idx| {
                    let id = if idx + 1 == ACCOUNT_ACTIVITIES_PAGE_SIZE {
                        last_id.to_owned()
                    } else {
                        format!("{last_id}-{idx}")
                    };
                    serde_json::json!({
                        "activity_type": "FEE",
                        "id": id,
                        "date": "2026-06-03",
                        "net_amount": "-0.01"
                    })
                })
                .collect::<Vec<_>>()
        )
    }
}
