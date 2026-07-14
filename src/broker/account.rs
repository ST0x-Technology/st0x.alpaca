//! Trading-account verification and cash-balance details.

use serde::Deserialize;
use st0x_finance::Usd;
use uuid::Uuid;

use super::{BrokerApiError, get_json};
use crate::core::AlpacaClient;

/// Account status from the Alpaca Broker API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AccountStatus {
    Onboarding,
    SubmissionFailed,
    Submitted,
    AccountUpdated,
    ApprovalPending,
    Active,
    Rejected,
    Disabled,
    DisableRequested,
    AccountClosed,
}

/// Identity and status fields of the trading-account response, used to
/// verify the configured account before trading.
#[derive(Debug, Deserialize)]
pub struct Account {
    pub id: Uuid,
    pub status: AccountStatus,
}

/// Cash-balance fields of the trading-account response.
#[derive(Debug, Deserialize)]
pub struct AccountDetails {
    pub cash: Usd,
    /// Settled cash that can be withdrawn -- excludes T+1 unsettled
    /// equity-sale proceeds. `None` if the broker omits the field.
    /// Alpaca documents this field on the trading-account response and
    /// defines it as cash available to withdraw, excluding unsettled
    /// memoposts:
    /// <https://docs.alpaca.markets/us/reference/gettradingaccount>
    /// <https://docs.alpaca.markets/us/docs/instant-funding-1>
    pub cash_withdrawable: Option<Usd>,
}

/// Fetches the trading account's identity and status.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses (e.g. 401 invalid credentials), and unparseable response
/// bodies.
pub async fn verify_account(client: &AlpacaClient) -> Result<Account, BrokerApiError> {
    get_json(client, &account_url(client)).await
}

/// Fetches the trading account's cash balances.
///
/// # Errors
///
/// Returns [`BrokerApiError::Alpaca`] on transport failures, non-2xx API
/// responses, and unparseable response bodies (including a response
/// missing the required `cash` field).
pub async fn get_account_details(client: &AlpacaClient) -> Result<AccountDetails, BrokerApiError> {
    get_json(client, &account_url(client)).await
}

fn account_url(client: &AlpacaClient) -> String {
    format!(
        "{}/v1/trading/accounts/{}/account",
        client.base_url(),
        client.account_id()
    )
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;
    use uuid::uuid;

    use super::super::{TEST_ACCOUNT_ID, test_client};
    use super::*;
    use crate::core::AlpacaError;

    fn usd(value: &str) -> Usd {
        value.parse().unwrap()
    }

    #[tokio::test]
    async fn verify_account_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/account"))
                .header(
                    "authorization",
                    "Basic dGVzdF9rZXlfaWQ6dGVzdF9zZWNyZXRfa2V5",
                );
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": "904837e3-3b76-47ec-b432-046db621571b",
                    "status": "ACTIVE",
                    "currency": "USD",
                    "buying_power": "100000.00"
                }));
        });

        let client = test_client(server.base_url());
        let account = verify_account(&client).await.unwrap();

        mock.assert();
        assert_eq!(account.id, uuid!("904837e3-3b76-47ec-b432-046db621571b"));
        assert_eq!(account.status, AccountStatus::Active);
    }

    #[tokio::test]
    async fn verify_account_unauthorized() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/account"));
            then.status(401)
                .header("content-type", "application/json")
                .json_body(json!({
                    "code": 40_110_000,
                    "message": "Invalid credentials"
                }));
        });

        let client = test_client(server.base_url());
        let error = verify_account(&client).await.unwrap_err();

        mock.assert();
        let BrokerApiError::Alpaca(AlpacaError::Api { status_code, .. }) = error else {
            panic!("expected Api error, got {error:?}");
        };
        assert_eq!(status_code, 401);
    }

    #[tokio::test]
    async fn get_account_details_extracts_cash_and_withdrawable() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/account"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "cash": "50000.00",
                    "cash_withdrawable": "32000.00"
                }));
        });

        let client = test_client(server.base_url());
        let details = get_account_details(&client).await.unwrap();

        mock.assert();
        assert_eq!(details.cash, usd("50000.00"));
        assert_eq!(details.cash_withdrawable, Some(usd("32000.00")));
    }

    #[tokio::test]
    async fn get_account_details_leaves_withdrawable_none_when_omitted() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/account"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "cash": "50000.00"
                }));
        });

        let client = test_client(server.base_url());
        let details = get_account_details(&client).await.unwrap();

        mock.assert();
        assert_eq!(details.cash, usd("50000.00"));
        assert_eq!(details.cash_withdrawable, None);
    }

    #[tokio::test]
    async fn get_account_details_requires_cash() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{TEST_ACCOUNT_ID}/account"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({}));
        });

        let client = test_client(server.base_url());
        let error = get_account_details(&client).await.unwrap_err();

        mock.assert();
        assert!(matches!(
            error,
            BrokerApiError::Alpaca(AlpacaError::Parse { .. })
        ));
    }
}
