//! Alpaca Broker API journal types for transferring securities
//! between accounts under the same firm.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use st0x_finance::{FractionalShares, Usd};
use uuid::Uuid;

use super::{BrokerApiError, Symbol, post_json};
use crate::core::AlpacaClient;

/// Type of journal entry in the Alpaca Broker API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum JournalEntryType {
    /// Journal of securities (stock positions).
    Jnls,
}

/// Request body for creating a security journal (JNLS) between accounts.
#[derive(Debug, Serialize)]
struct JournalRequest {
    from_account: String,
    to_account: Uuid,
    entry_type: JournalEntryType,
    symbol: Symbol,
    #[serde(rename = "qty")]
    quantity: FractionalShares,
}

/// Response from the Alpaca Broker API when creating a journal.
#[derive(Debug, Deserialize)]
pub struct JournalResponse {
    pub id: Uuid,
    pub status: JournalStatus,
    pub symbol: Symbol,
    #[serde(rename = "qty")]
    pub quantity: FractionalShares,
    pub price: Option<Usd>,
    pub from_account: Uuid,
    pub to_account: Uuid,
    pub settle_date: Option<NaiveDate>,
    pub system_date: Option<NaiveDate>,
    pub description: Option<String>,
}

/// Status of an Alpaca journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
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

impl std::fmt::Display for JournalStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(formatter, "queued"),
            Self::SentToClearing => write!(formatter, "sent_to_clearing"),
            Self::Pending => write!(formatter, "pending"),
            Self::Executed => write!(formatter, "executed"),
            Self::Rejected => write!(formatter, "rejected"),
            Self::Canceled => write!(formatter, "canceled"),
            Self::Refused => write!(formatter, "refused"),
            Self::Deleted => write!(formatter, "deleted"),
            Self::Correct => write!(formatter, "correct"),
        }
    }
}

/// Creates a security journal (JNLS) transferring `quantity` of `symbol`
/// from the client's account to `to_account`.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses (e.g. 403 insufficient assets, 404 unknown account), and
/// unparseable response bodies.
pub async fn create_journal(
    client: &AlpacaClient,
    to_account: Uuid,
    symbol: Symbol,
    quantity: FractionalShares,
) -> Result<JournalResponse, BrokerApiError> {
    let url = format!("{}/v1/journals", client.base_url());

    let request = JournalRequest {
        from_account: client.account_id().to_string(),
        to_account,
        entry_type: JournalEntryType::Jnls,
        symbol,
        quantity,
    };

    post_json(client, &url, &request).await
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::uuid;

    use super::super::test_client;
    use super::*;
    use crate::core::AlpacaError;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap_or_else(|error| panic!("invalid test symbol: {error}"))
    }

    const DESTINATION_ACCOUNT_ID: Uuid = uuid!("11111111-2222-3333-4444-555555555555");

    fn shares(value: &str) -> FractionalShares {
        value.parse().unwrap()
    }

    fn usd(value: &str) -> Usd {
        value.parse().unwrap()
    }

    #[test]
    fn journal_status_display_matches_serde_names() {
        assert_eq!(JournalStatus::Queued.to_string(), "queued");
        assert_eq!(
            JournalStatus::SentToClearing.to_string(),
            "sent_to_clearing"
        );
        assert_eq!(JournalStatus::Pending.to_string(), "pending");
        assert_eq!(JournalStatus::Executed.to_string(), "executed");
        assert_eq!(JournalStatus::Rejected.to_string(), "rejected");
        assert_eq!(JournalStatus::Canceled.to_string(), "canceled");
        assert_eq!(JournalStatus::Refused.to_string(), "refused");
        assert_eq!(JournalStatus::Deleted.to_string(), "deleted");
        assert_eq!(JournalStatus::Correct.to_string(), "correct");
    }

    #[test]
    fn journal_entry_type_serializes_as_screaming_snake_case() {
        let json = serde_json::to_string(&JournalEntryType::Jnls).unwrap();
        assert_eq!(json, "\"JNLS\"");
    }

    #[test]
    fn journal_request_serializes_quantity_as_string() {
        let request = JournalRequest {
            from_account: "904837e3-3b76-47ec-b432-046db621571b".to_string(),
            to_account: DESTINATION_ACCOUNT_ID,
            entry_type: JournalEntryType::Jnls,
            symbol: symbol("AAPL"),
            quantity: shares("10.5"),
        };

        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["qty"], json!("10.5"));
        assert_eq!(json["entry_type"], json!("JNLS"));
        assert_eq!(json["symbol"], json!("AAPL"));
    }

    #[test]
    fn deserialize_dates_as_naive_date() {
        let json = serde_json::json!({
            "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "status": "executed",
            "symbol": "AAPL",
            "qty": "10",
            "from_account": "904837e3-3b76-47ec-b432-046db621571b",
            "to_account": "11111111-2222-3333-4444-555555555555",
            "settle_date": "2026-02-28",
            "system_date": "2026-02-26"
        });

        let response: JournalResponse = serde_json::from_value(json).unwrap();

        assert_eq!(response.settle_date, NaiveDate::from_ymd_opt(2026, 2, 28));
        assert_eq!(response.system_date, NaiveDate::from_ymd_opt(2026, 2, 26));
    }

    #[tokio::test]
    async fn create_journal_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/journals").json_body(json!({
                "from_account": "904837e3-3b76-47ec-b432-046db621571b",
                "to_account": "11111111-2222-3333-4444-555555555555",
                "entry_type": "JNLS",
                "symbol": "AAPL",
                "qty": "10.5"
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    "status": "pending",
                    "symbol": "AAPL",
                    "qty": "10.5",
                    "price": "150.25",
                    "from_account": "904837e3-3b76-47ec-b432-046db621571b",
                    "to_account": "11111111-2222-3333-4444-555555555555",
                    "settle_date": "2026-02-28",
                    "system_date": "2026-02-26",
                    "description": null
                }));
        });

        let client = test_client(server.base_url());
        let response = create_journal(
            &client,
            DESTINATION_ACCOUNT_ID,
            symbol("AAPL"),
            shares("10.5"),
        )
        .await
        .unwrap();

        mock.assert();
        assert_eq!(response.id, uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert_eq!(response.status, JournalStatus::Pending);
        assert_eq!(response.symbol, symbol("AAPL"));
        assert_eq!(response.quantity, shares("10.5"));
        assert_eq!(response.price, Some(usd("150.25")));
    }

    #[tokio::test]
    async fn create_journal_insufficient_assets() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/journals");
            then.status(403)
                .header("content-type", "application/json")
                .json_body(json!({
                    "code": 40_310_000_u64,
                    "message": "insufficient assets"
                }));
        });

        let client = test_client(server.base_url());
        let error = create_journal(
            &client,
            DESTINATION_ACCOUNT_ID,
            symbol("AAPL"),
            shares("999999"),
        )
        .await
        .unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, body }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 403);
        assert!(body.contains("insufficient assets"));
    }

    #[tokio::test]
    async fn create_journal_account_not_found() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/journals");
            then.status(404)
                .header("content-type", "application/json")
                .json_body(json!({
                    "code": 40_410_000_u64,
                    "message": "account not found"
                }));
        });

        let client = test_client(server.base_url());
        let error = create_journal(
            &client,
            DESTINATION_ACCOUNT_ID,
            symbol("AAPL"),
            shares("10"),
        )
        .await
        .unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, .. }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 404);
    }
}
