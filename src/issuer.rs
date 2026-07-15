//! ITN issuer-callback surface: the endpoints an Instant Tokenization
//! Network issuer calls on Alpaca — mint-completion callback, redeem
//! initiation, and keyed tokenization-request polling.
//!
//! Symbols and share quantities use the shared `st0x-finance` domain types;
//! issuer-specific identifiers and lifecycle states stay local to this API.

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use st0x_finance::{EmptySymbolError, FractionalShares, Symbol};
use uuid::Uuid;

use crate::core::{AlpacaClient, AlpacaError, Network, TokenizationRequestId};
use crate::rate_limit::retry_after_from_response_headers;

/// Issuer-side operations against Alpaca's tokenization endpoints.
///
/// Implemented by [`AlpacaClient`] for real HTTP calls and by
/// [`mock::MockIssuerApi`] for consumer tests.
#[async_trait]
pub trait IssuerApi: Send + Sync {
    /// Sends a mint callback to Alpaca to confirm mint completion.
    ///
    /// Calls `POST /v1/accounts/{account_id}/tokenization/callback/mint`.
    /// Returns `Ok(())` on success (200 OK response from Alpaca).
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] if the HTTP request fails, authentication
    /// fails, or Alpaca returns an error response.
    async fn send_mint_callback(&self, request: MintCallbackRequest) -> Result<(), AlpacaError>;

    /// Calls Alpaca's redeem endpoint to initiate a redemption.
    ///
    /// Calls `POST /v1/accounts/{account_id}/tokenization/callback/redeem`.
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] if the HTTP request fails, authentication
    /// fails, the response cannot be parsed, or Alpaca returns an error
    /// response.
    async fn call_redeem_endpoint(
        &self,
        request: RedeemRequest,
    ) -> Result<RedeemResponse, AlpacaError>;

    /// Polls Alpaca's keyed request endpoint for a specific tokenization
    /// request.
    ///
    /// Calls
    /// `GET /v1/accounts/{account_id}/tokenization/requests/{tokenization_request_id}`
    /// and deserializes the single-object response as
    /// [`TokenizationRequest`]. Maps HTTP 404 to
    /// [`AlpacaError::RequestNotFound`].
    ///
    /// # Errors
    ///
    /// Returns [`AlpacaError`] if the HTTP request fails, authentication
    /// fails, the response cannot be parsed, the request is not found
    /// (404), or the returned id differs from the requested one.
    async fn poll_request_status(
        &self,
        tokenization_request_id: &TokenizationRequestId,
    ) -> Result<TokenizationRequest, AlpacaError>;
}

/// Request payload for Alpaca's mint callback endpoint.
///
/// This struct is serialized to JSON and sent to:
/// `POST /v1/accounts/{account_id}/tokenization/callback/mint`
///
/// The JSON format uses `snake_case` field names per Alpaca's API convention.
#[derive(Debug, Clone, Serialize)]
pub struct MintCallbackRequest {
    pub tokenization_request_id: TokenizationRequestId,
    pub client_id: ClientId,
    pub wallet_address: Address,
    pub tx_hash: B256,
    pub network: Network,
}

/// Request payload for Alpaca's redeem endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct RedeemRequest {
    pub issuer_request_id: IssuerRequestId,
    #[serde(rename = "underlying_symbol")]
    pub underlying: UnderlyingSymbol,
    #[serde(rename = "token_symbol")]
    pub token: TokenSymbol,
    pub client_id: ClientId,
    #[serde(rename = "qty")]
    pub quantity: Qty,
    pub network: Network,
    #[serde(rename = "wallet_address")]
    pub wallet: Address,
    pub tx_hash: B256,
}

/// Response payload from Alpaca's redeem endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct RedeemResponse {
    pub tokenization_request_id: TokenizationRequestId,
    pub issuer_request_id: IssuerRequestId,
    pub created_at: DateTime<Utc>,
    #[serde(rename = "type")]
    pub r#type: TokenizationRequestType,
    pub status: RedeemRequestStatus,
    #[serde(rename = "underlying_symbol")]
    pub underlying: UnderlyingSymbol,
    #[serde(rename = "token_symbol")]
    pub token: TokenSymbol,
    #[serde(rename = "qty")]
    pub quantity: Qty,
    pub issuer: String,
    pub network: Network,
    #[serde(rename = "wallet_address")]
    pub wallet: Address,
    pub tx_hash: B256,
    pub fees: Option<Fees>,
}

/// Direction of a tokenization request as reported by Alpaca.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenizationRequestType {
    Mint,
    Redeem,
}

/// Lifecycle status of a redemption request as reported by Alpaca.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RedeemRequestStatus {
    Pending,
    Completed,
    Rejected,
}

/// Fee amount attached to a redeem response, as a decimal string on the
/// wire.
#[derive(Debug, Clone, Deserialize)]
pub struct Fees(pub Decimal);

/// A tokenization request returned by Alpaca's keyed request endpoint.
///
/// The endpoint returns a single object. This enum deserializes both Mint
/// and Redeem variants via `#[serde(tag = "type")]`, with each variant
/// carrying the appropriate `issuer_request_id` type.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TokenizationRequest {
    Mint {},
    Redeem {
        #[serde(rename = "tokenization_request_id")]
        id: TokenizationRequestId,
        #[serde(rename = "issuer_request_id")]
        issuer_request_id: IssuerRequestId,
        status: RedeemRequestStatus,
        #[serde(rename = "underlying_symbol")]
        underlying: UnderlyingSymbol,
        #[serde(rename = "token_symbol")]
        token: TokenSymbol,
        #[serde(rename = "qty")]
        quantity: Qty,
        #[serde(rename = "wallet_address")]
        wallet: Address,
        #[serde(
            rename = "tx_hash",
            default,
            deserialize_with = "deserialize_optional_b256"
        )]
        tx_hash: Option<B256>,
        updated_at: Option<DateTime<Utc>>,
    },
}

/// Client (AP account) identifier on the wire: a UUID string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub Uuid);

/// Issuer-assigned request identifier as it appears on the wire.
///
/// Issuance derives this from a transaction hash and serializes it as a
/// `0x`-prefixed hex string; on the wire it is just a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IssuerRequestId(pub String);

/// Ticker of the underlying equity (e.g. `AAPL`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnderlyingSymbol(pub Symbol);

impl UnderlyingSymbol {
    /// # Errors
    ///
    /// Returns [`EmptySymbolError`] when the symbol is empty or whitespace-only.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptySymbolError> {
        Symbol::new(value).map(Self)
    }
}

/// Ticker of the tokenized representation (e.g. `tAAPL`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenSymbol(pub Symbol);

impl TokenSymbol {
    /// # Errors
    ///
    /// Returns [`EmptySymbolError`] when the symbol is empty or whitespace-only.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptySymbolError> {
        Symbol::new(value).map(Self)
    }
}

/// Share quantity on the wire. Alpaca encodes quantities as JSON strings
/// (e.g. `"100.5"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Qty(pub FractionalShares);

#[async_trait]
impl IssuerApi for AlpacaClient {
    async fn send_mint_callback(&self, request: MintCallbackRequest) -> Result<(), AlpacaError> {
        let url = format!(
            "{}/v1/accounts/{}/tokenization/callback/mint",
            self.base_url(),
            self.account_id()
        );

        self.with_retry(|| async {
            let response = self.post(&url).await?.json(&request).send().await?;

            let status = response.status();
            let retry_after = retry_after_from_response_headers(response.headers());

            match status {
                StatusCode::OK => Ok(()),
                StatusCode::TOO_MANY_REQUESTS => {
                    let body = response.text().await?;
                    Err(AlpacaError::RateLimited { body, retry_after })
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    let body = response.text().await?;
                    Err(AlpacaError::Auth(body))
                }
                status => {
                    let body = response.text().await?;
                    Err(AlpacaError::Api {
                        status_code: status.as_u16(),
                        body,
                    })
                }
            }
        })
        .await
    }

    async fn call_redeem_endpoint(
        &self,
        request: RedeemRequest,
    ) -> Result<RedeemResponse, AlpacaError> {
        let url = format!(
            "{}/v1/accounts/{}/tokenization/callback/redeem",
            self.base_url(),
            self.account_id()
        );

        self.with_retry(|| async {
            let response = self.post(&url).await?.json(&request).send().await?;

            let status = response.status();
            let retry_after = retry_after_from_response_headers(response.headers());

            match status {
                StatusCode::OK => {
                    let body = response.text().await?;
                    serde_json::from_str(&body)
                        .map_err(|source| AlpacaError::Parse { body, source })
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    let body = response.text().await?;
                    Err(AlpacaError::Auth(body))
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let body = response.text().await?;
                    Err(AlpacaError::RateLimited { body, retry_after })
                }
                status => {
                    let body = response.text().await?;
                    Err(AlpacaError::Api {
                        status_code: status.as_u16(),
                        body,
                    })
                }
            }
        })
        .await
    }

    async fn poll_request_status(
        &self,
        tokenization_request_id: &TokenizationRequestId,
    ) -> Result<TokenizationRequest, AlpacaError> {
        // Alpaca tokenization_request_ids are server-generated UUIDs, so the
        // path segment needs no percent-encoding.
        let url = format!(
            "{}/v1/accounts/{}/tokenization/requests/{}",
            self.base_url(),
            self.account_id(),
            tokenization_request_id
        );

        self.with_retry(|| async {
            let response = self.get(&url).await?.send().await?;

            let status = response.status();
            let retry_after = retry_after_from_response_headers(response.headers());

            match status {
                StatusCode::OK => {
                    let body = response.text().await?;
                    let request: TokenizationRequest = serde_json::from_str(&body)
                        .map_err(|source| AlpacaError::Parse { body, source })?;
                    // Keyed endpoint retrieves aged requests that no longer
                    // appear in the list endpoint (empirically verified
                    // 2026-06-12). A 404 here means definitively absent; see
                    // [`AlpacaError::RequestNotFound`].
                    if let TokenizationRequest::Redeem { id, .. } = &request
                        && *id != *tokenization_request_id
                    {
                        return Err(AlpacaError::ResponseIdMismatch {
                            requested: tokenization_request_id.clone(),
                            returned: id.clone(),
                        });
                    }
                    Ok(request)
                }
                StatusCode::NOT_FOUND => {
                    let body = response.text().await?;
                    Err(AlpacaError::RequestNotFound {
                        id: tokenization_request_id.clone(),
                        body,
                    })
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    let body = response.text().await?;
                    Err(AlpacaError::Auth(body))
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let body = response.text().await?;
                    Err(AlpacaError::RateLimited { body, retry_after })
                }
                status => {
                    let body = response.text().await?;
                    Err(AlpacaError::Api {
                        status_code: status.as_u16(),
                        body,
                    })
                }
            }
        })
        .await
    }
}

fn deserialize_optional_b256<'de, D>(deserializer: D) -> Result<Option<B256>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;

    match raw {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => value.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

pub mod mock {
    //! Configurable in-memory [`IssuerApi`] double.
    //!
    //! Not gated behind `#[cfg(test)]` because consumers construct it in
    //! their own test setups (e.g. spinning up a test service), which
    //! compile this library without its test configuration.

    use alloy_primitives::{address, b256};
    use async_trait::async_trait;
    use chrono::Utc;
    use rust_decimal::Decimal;
    use st0x_finance::FractionalShares;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        Fees, IssuerApi, IssuerRequestId, MintCallbackRequest, Qty, RedeemRequest,
        RedeemRequestStatus, RedeemResponse, TokenSymbol, TokenizationRequest,
        TokenizationRequestType, UnderlyingSymbol,
    };
    use crate::core::{AlpacaError, TokenizationRequestId};

    fn underlying_symbol(value: &str) -> UnderlyingSymbol {
        UnderlyingSymbol::new(value)
            .unwrap_or_else(|error| panic!("invalid mock underlying symbol: {error}"))
    }

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid mock token symbol: {error}"))
    }

    fn quantity(value: &str) -> Qty {
        Qty(value
            .parse::<FractionalShares>()
            .unwrap_or_else(|error| panic!("invalid mock share quantity: {error}")))
    }

    /// Mock issuer API for testing.
    ///
    /// Can be configured to either succeed or fail, and tracks the total
    /// number of calls made across all three endpoints.
    pub struct MockIssuerApi {
        behavior: Behavior,
        call_count: Arc<AtomicUsize>,
    }

    impl MockIssuerApi {
        /// Creates a mock that will succeed on all calls.
        #[must_use]
        pub fn new_success() -> Self {
            Self {
                behavior: Behavior::Succeed,
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Creates a mock that will fail on all calls.
        ///
        /// # Arguments
        ///
        /// * `error_message` - Error message to return in the failure
        #[must_use]
        pub fn new_failure(error_message: impl Into<String>) -> Self {
            Self {
                behavior: Behavior::Fail {
                    error_message: error_message.into(),
                },
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Returns the number of endpoint calls made so far.
        #[must_use]
        pub fn get_call_count(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl IssuerApi for MockIssuerApi {
        async fn send_mint_callback(
            &self,
            _request: MintCallbackRequest,
        ) -> Result<(), AlpacaError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            match &self.behavior {
                Behavior::Succeed => Ok(()),
                Behavior::Fail { error_message } => Err(AlpacaError::Api {
                    status_code: 500,
                    body: error_message.clone(),
                }),
            }
        }

        async fn call_redeem_endpoint(
            &self,
            request: RedeemRequest,
        ) -> Result<RedeemResponse, AlpacaError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            match &self.behavior {
                Behavior::Succeed => Ok(RedeemResponse {
                    tokenization_request_id: TokenizationRequestId("mock-tok-123".to_string()),
                    issuer_request_id: request.issuer_request_id,
                    created_at: Utc::now(),
                    r#type: TokenizationRequestType::Redeem,
                    status: RedeemRequestStatus::Pending,
                    underlying: request.underlying,
                    token: request.token,
                    quantity: request.quantity,
                    issuer: "mock-issuer".to_string(),
                    network: request.network,
                    wallet: request.wallet,
                    tx_hash: request.tx_hash,
                    fees: Some(Fees(Decimal::ZERO)),
                }),
                Behavior::Fail { error_message } => Err(AlpacaError::Api {
                    status_code: 500,
                    body: error_message.clone(),
                }),
            }
        }

        async fn poll_request_status(
            &self,
            tokenization_request_id: &TokenizationRequestId,
        ) -> Result<TokenizationRequest, AlpacaError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            match &self.behavior {
                Behavior::Succeed => Ok(TokenizationRequest::Redeem {
                    id: tokenization_request_id.clone(),
                    issuer_request_id: IssuerRequestId(
                        "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                            .to_string(),
                    ),
                    status: RedeemRequestStatus::Completed,
                    underlying: underlying_symbol("AAPL"),
                    token: token_symbol("tAAPL"),
                    quantity: quantity("100"),
                    wallet: address!("0x1234567890abcdef1234567890abcdef12345678"),
                    tx_hash: Some(b256!(
                        "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                    )),
                    updated_at: Some(Utc::now()),
                }),
                Behavior::Fail { error_message } => Err(AlpacaError::RequestNotFound {
                    id: tokenization_request_id.clone(),
                    body: error_message.clone(),
                }),
            }
        }
    }

    enum Behavior {
        Succeed,
        Fail { error_message: String },
    }

    #[cfg(test)]
    mod tests {
        use alloy_primitives::{address, b256};
        use uuid::Uuid;

        use super::{MockIssuerApi, quantity, token_symbol, underlying_symbol};
        use crate::core::{Network, TokenizationRequestId};
        use crate::issuer::{
            ClientId, IssuerApi, IssuerRequestId, MintCallbackRequest, RedeemRequest,
            RedeemRequestStatus, TokenizationRequest,
        };

        #[tokio::test]
        async fn test_mock_success_service() {
            let mock = MockIssuerApi::new_success();

            let request = MintCallbackRequest {
                tokenization_request_id: TokenizationRequestId::new("test-123"),
                client_id: ClientId(Uuid::new_v4()),
                wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
                tx_hash: b256!(
                    "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                ),
                network: Network::Base,
            };

            let result = mock.send_mint_callback(request).await;

            assert!(result.is_ok(), "Expected Ok, got {result:?}");
            assert_eq!(mock.get_call_count(), 1);
        }

        #[tokio::test]
        async fn test_mock_failure_service() {
            let mock = MockIssuerApi::new_failure("Network timeout");

            let request = MintCallbackRequest {
                tokenization_request_id: TokenizationRequestId::new("test-789"),
                client_id: ClientId(Uuid::new_v4()),
                wallet_address: address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                tx_hash: b256!(
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                ),
                network: Network::Base,
            };

            let result = mock.send_mint_callback(request).await;

            assert!(result.is_err(), "Expected Err, got Ok");
            assert_eq!(mock.get_call_count(), 1);

            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("Network timeout"),
                "Expected error message to contain 'Network timeout', got: {err}"
            );
        }

        #[tokio::test]
        async fn test_mock_tracks_multiple_calls() {
            let mock = MockIssuerApi::new_success();

            let request = MintCallbackRequest {
                tokenization_request_id: TokenizationRequestId::new("test"),
                client_id: ClientId(Uuid::new_v4()),
                wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
                tx_hash: b256!(
                    "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                ),
                network: Network::Base,
            };

            mock.send_mint_callback(request.clone()).await.unwrap();
            mock.send_mint_callback(request.clone()).await.unwrap();
            mock.send_mint_callback(request).await.unwrap();

            assert_eq!(mock.get_call_count(), 3);
        }

        fn create_redeem_request() -> RedeemRequest {
            let tx_hash =
                b256!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd");
            RedeemRequest {
                issuer_request_id: IssuerRequestId(
                    "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                        .to_string(),
                ),
                underlying: underlying_symbol("AAPL"),
                token: token_symbol("tAAPL"),
                client_id: ClientId(Uuid::new_v4()),
                quantity: quantity("100"),
                network: Network::Base,
                wallet: address!("0x1234567890abcdef1234567890abcdef12345678"),
                tx_hash,
            }
        }

        #[tokio::test]
        async fn test_mock_redeem_success() {
            let mock = MockIssuerApi::new_success();

            let request = create_redeem_request();
            let expected_issuer_request_id = request.issuer_request_id.clone();
            let response = mock.call_redeem_endpoint(request).await.unwrap();

            assert_eq!(mock.get_call_count(), 1);
            assert_eq!(response.tokenization_request_id.0, "mock-tok-123");
            assert_eq!(response.issuer_request_id, expected_issuer_request_id);
        }

        #[tokio::test]
        async fn test_mock_redeem_failure() {
            let mock = MockIssuerApi::new_failure("API timeout");

            let request = create_redeem_request();
            let result = mock.call_redeem_endpoint(request).await;

            assert!(result.is_err(), "Expected Err, got Ok");
            assert_eq!(mock.get_call_count(), 1);

            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("API timeout"),
                "Expected error message to contain 'API timeout', got: {err}"
            );
        }

        #[tokio::test]
        async fn test_mock_redeem_tracks_multiple_calls() {
            let mock = MockIssuerApi::new_success();

            let request = create_redeem_request();

            mock.call_redeem_endpoint(request.clone()).await.unwrap();
            mock.call_redeem_endpoint(request.clone()).await.unwrap();
            mock.call_redeem_endpoint(request).await.unwrap();

            assert_eq!(mock.get_call_count(), 3);
        }

        #[tokio::test]
        async fn test_mock_shares_call_count_between_endpoints() {
            let mock = MockIssuerApi::new_success();

            let mint_request = MintCallbackRequest {
                tokenization_request_id: TokenizationRequestId::new("test"),
                client_id: ClientId(Uuid::new_v4()),
                wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
                tx_hash: b256!(
                    "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                ),
                network: Network::Base,
            };

            let redeem_request = create_redeem_request();

            mock.send_mint_callback(mint_request).await.unwrap();
            mock.call_redeem_endpoint(redeem_request).await.unwrap();

            assert_eq!(mock.get_call_count(), 2);
        }

        #[tokio::test]
        async fn test_mock_poll_request_status_success() {
            let mock = MockIssuerApi::new_success();

            let tokenization_request_id = TokenizationRequestId::new("tok-123");
            let result = mock.poll_request_status(&tokenization_request_id).await;

            assert!(result.is_ok(), "Expected Ok, got {result:?}");
            let request = result.unwrap();
            assert!(matches!(
                request,
                TokenizationRequest::Redeem {
                    status: RedeemRequestStatus::Completed,
                    ..
                }
            ));
            assert_eq!(mock.get_call_count(), 1);
        }

        #[tokio::test]
        async fn test_mock_poll_request_status_failure() {
            let mock = MockIssuerApi::new_failure("Request not found");

            let tokenization_request_id = TokenizationRequestId::new("tok-not-found");
            let result = mock.poll_request_status(&tokenization_request_id).await;

            assert!(result.is_err(), "Expected Err, got Ok");
            assert_eq!(mock.get_call_count(), 1);

            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("tok-not-found"),
                "Expected error message to contain 'tok-not-found', got: {err}"
            );
        }

        #[tokio::test]
        async fn test_mock_poll_request_status_tracks_multiple_calls() {
            let mock = MockIssuerApi::new_success();

            let tokenization_request_id = TokenizationRequestId::new("tok-test");

            mock.poll_request_status(&tokenization_request_id)
                .await
                .unwrap();
            mock.poll_request_status(&tokenization_request_id)
                .await
                .unwrap();
            mock.poll_request_status(&tokenization_request_id)
                .await
                .unwrap();

            assert_eq!(mock.get_call_count(), 3);
        }

        #[tokio::test]
        async fn test_mock_poll_shares_call_count_with_other_endpoints() {
            let mock = MockIssuerApi::new_success();

            let mint_request = MintCallbackRequest {
                tokenization_request_id: TokenizationRequestId::new("mint-tok"),
                client_id: ClientId(Uuid::new_v4()),
                wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
                tx_hash: b256!(
                    "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                ),
                network: Network::Base,
            };

            let redeem_request = create_redeem_request();
            let poll_id = TokenizationRequestId::new("poll-tok");

            mock.send_mint_callback(mint_request).await.unwrap();
            mock.call_redeem_endpoint(redeem_request).await.unwrap();
            mock.poll_request_status(&poll_id).await.unwrap();

            assert_eq!(mock.get_call_count(), 3);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use httpmock::prelude::*;
    use serde_json::{Value, json};
    use st0x_finance::FractionalShares;
    use std::time::Duration;
    use uuid::Uuid;

    use super::{
        ClientId, IssuerApi, IssuerRequestId, MintCallbackRequest, Qty, RedeemRequest,
        RedeemRequestStatus, TokenSymbol, TokenizationRequest, TokenizationRequestType,
        UnderlyingSymbol,
    };
    use crate::core::{AlpacaClient, AlpacaError, Network, TokenizationRequestId};

    fn underlying_symbol(value: &str) -> UnderlyingSymbol {
        UnderlyingSymbol::new(value)
            .unwrap_or_else(|error| panic!("invalid test underlying symbol: {error}"))
    }

    fn token_symbol(value: &str) -> TokenSymbol {
        TokenSymbol::new(value).unwrap_or_else(|error| panic!("invalid test token symbol: {error}"))
    }

    fn quantity(value: &str) -> Qty {
        Qty(value
            .parse::<FractionalShares>()
            .unwrap_or_else(|error| panic!("invalid test share quantity: {error}")))
    }

    fn make_client(
        server: &MockServer,
        account_id: &str,
        api_key: &str,
        api_secret: &str,
    ) -> AlpacaClient {
        AlpacaClient::new(
            server.base_url(),
            account_id.to_string(),
            api_key.to_string(),
            api_secret.to_string(),
            Duration::from_secs(10),
            Duration::from_secs(30),
        )
        .unwrap()
    }

    fn redeem_tokenization_request_json(tx_hash: Option<Value>) -> Value {
        let mut request = json!({
            "type": "redeem",
            "tokenization_request_id": "tok-456",
            "issuer_request_id": "red-574378e0",
            "status": "pending",
            "underlying_symbol": "AAPL",
            "token_symbol": "tAAPL",
            "qty": "50.00",
            "wallet_address": "0x9999999999999999999999999999999999999999",
            "updated_at": "2025-09-12T17:30:00.000000-04:00"
        });

        if let Some(tx_hash) = tx_hash {
            request
                .as_object_mut()
                .unwrap()
                .insert("tx_hash".to_string(), tx_hash);
        }

        request
    }

    fn assert_redeem_tx_hash_is_none(request: &TokenizationRequest) {
        assert!(matches!(
            request,
            TokenizationRequest::Redeem { tx_hash: None, .. }
        ));
    }

    #[test]
    fn test_mint_callback_request_serialization() {
        let client_id = ClientId("55051234-0000-4abc-9000-4aabcdef0045".parse().unwrap());

        let request = MintCallbackRequest {
            tokenization_request_id: TokenizationRequestId::new("12345-678-90AB"),
            client_id,
            wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
            tx_hash: b256!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"),
            network: Network::Base,
        };

        let serialized = serde_json::to_value(&request).unwrap();

        assert_eq!(
            serialized["tokenization_request_id"],
            json!("12345-678-90AB")
        );
        assert_eq!(
            serialized["client_id"],
            json!("55051234-0000-4abc-9000-4aabcdef0045")
        );
        assert_eq!(
            serialized["wallet_address"],
            json!("0x1234567890abcdef1234567890abcdef12345678")
        );
        assert_eq!(
            serialized["tx_hash"],
            json!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd")
        );
        assert_eq!(serialized["network"], json!("base"));
    }

    #[test]
    fn test_redeem_request_serialization() {
        let client_id = ClientId("55051234-0000-4abc-9000-4aabcdef0045".parse().unwrap());
        let tx_hash = b256!("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef");

        let request = RedeemRequest {
            issuer_request_id: IssuerRequestId(
                "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef".to_string(),
            ),
            underlying: underlying_symbol("AAPL"),
            token: token_symbol("tAAPL"),
            client_id,
            quantity: quantity("100.50"),
            network: Network::Base,
            wallet: address!("0x9999999999999999999999999999999999999999"),
            tx_hash,
        };

        let serialized = serde_json::to_value(&request).unwrap();

        assert_eq!(
            serialized["issuer_request_id"],
            json!("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
        );
        assert_eq!(serialized["underlying_symbol"], json!("AAPL"));
        assert_eq!(serialized["token_symbol"], json!("tAAPL"));
        assert_eq!(
            serialized["client_id"],
            json!("55051234-0000-4abc-9000-4aabcdef0045")
        );
        assert_eq!(serialized["qty"], json!("100.5"));
        assert_eq!(serialized["network"], json!("base"));
        assert_eq!(
            serialized["wallet_address"],
            json!("0x9999999999999999999999999999999999999999")
        );
        assert_eq!(
            serialized["tx_hash"],
            json!("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
        );
    }

    #[test]
    fn test_address_serialization_includes_0x_prefix() {
        let request = MintCallbackRequest {
            tokenization_request_id: TokenizationRequestId::new("test"),
            client_id: ClientId(Uuid::new_v4()),
            wallet_address: address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            tx_hash: b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            network: Network::Base,
        };

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""));
        assert!(
            json.contains("\"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"")
        );
    }

    #[test]
    fn test_tokenization_request_deserializes_mint_variant() {
        let json = json!({
            "type": "mint",
            "tokenization_request_id": "tok-123",
            "issuer_request_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            "status": "completed",
            "underlying_symbol": "AAPL",
            "token_symbol": "tAAPL",
            "qty": "100.00",
            "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
            "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
        });

        let request: TokenizationRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(request, TokenizationRequest::Mint { .. }));
    }

    #[test]
    fn test_tokenization_request_deserializes_redeem_variant() {
        let json = json!({
            "type": "redeem",
            "tokenization_request_id": "tok-456",
            "issuer_request_id": "red-574378e0",
            "status": "completed",
            "underlying_symbol": "AAPL",
            "token_symbol": "tAAPL",
            "qty": "50.00",
            "wallet_address": "0x9999999999999999999999999999999999999999",
            "tx_hash": "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
            "updated_at": "2025-09-12T17:30:00.000000-04:00"
        });

        let request: TokenizationRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(request, TokenizationRequest::Redeem { .. }));
    }

    #[test]
    fn test_tokenization_request_redeem_accepts_omitted_tx_hash() {
        let request = serde_json::from_value(redeem_tokenization_request_json(None)).unwrap();

        assert_redeem_tx_hash_is_none(&request);
    }

    #[test]
    fn test_tokenization_request_redeem_accepts_null_tx_hash() {
        let request =
            serde_json::from_value(redeem_tokenization_request_json(Some(Value::Null))).unwrap();

        assert_redeem_tx_hash_is_none(&request);
    }

    #[test]
    fn test_tokenization_request_redeem_accepts_empty_tx_hash() {
        let request =
            serde_json::from_value(redeem_tokenization_request_json(Some(json!("")))).unwrap();

        assert_redeem_tx_hash_is_none(&request);
    }

    #[test]
    fn test_tokenization_request_list_deserializes_mixed_types() {
        let json = json!([
            {
                "type": "mint",
                "tokenization_request_id": "tok-mint-1",
                "issuer_request_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
                "status": "completed",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100.00",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            },
            {
                "type": "redeem",
                "tokenization_request_id": "tok-red-2",
                "issuer_request_id": "red-574378e0",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "50.00",
                "wallet_address": "0x9999999999999999999999999999999999999999",
                "tx_hash": "",
                "updated_at": "2025-09-12T17:30:00.000000-04:00"
            }
        ]);

        let requests: Vec<TokenizationRequest> = serde_json::from_value(json).unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], TokenizationRequest::Mint { .. }));
        assert!(matches!(requests[1], TokenizationRequest::Redeem { .. }));
    }

    fn create_test_request() -> MintCallbackRequest {
        let client_id = ClientId("55051234-0000-4abc-9000-4aabcdef0045".parse().unwrap());

        MintCallbackRequest {
            tokenization_request_id: TokenizationRequestId::new("12345-678-90AB"),
            client_id,
            wallet_address: address!("0x1234567890abcdef1234567890abcdef12345678"),
            tx_hash: b256!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"),
            network: Network::Base,
        }
    }

    #[tokio::test]
    async fn test_send_mint_callback_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint")
                .header("authorization", "Basic dGVzdC1rZXk6dGVzdC1zZWNyZXQ=")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret");
            then.status(200).body("");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_unauthorized() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint");
            then.status(401).body("Unauthorized");
        });

        let client = make_client(&server, "test-account", "wrong-key", "wrong-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_forbidden() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint");
            then.status(403).body("Forbidden");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint");
            then.status(400).body("Bad Request");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        match result {
            Err(AlpacaError::Api { status_code, body }) => {
                assert_eq!(status_code, 400);
                assert_eq!(body, "Bad Request");
            }
            _ => panic!("Expected AlpacaError::Api, got {result:?}"),
        }

        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_sends_correct_json() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint")
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "tokenization_request_id": "12345-678-90AB",
                    "client_id": "55051234-0000-4abc-9000-4aabcdef0045",
                    "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                    "network": "base"
                }));
            then.status(200).body("");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_uses_legacy_auth() {
        let server = MockServer::start();

        // Legacy auth requires both Basic auth AND the APCA headers
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/mint")
                .header("authorization", "Basic bXlrZXk6bXlzZWNyZXQ=")
                .header("APCA-API-KEY-ID", "mykey")
                .header("APCA-API-SECRET-KEY", "mysecret");
            then.status(200).body("");
        });

        let client = make_client(&server, "test-account", "mykey", "mysecret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_send_mint_callback_constructs_correct_url() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/my-special-account/tokenization/callback/mint");
            then.status(200).body("");
        });

        let client = make_client(&server, "my-special-account", "test-key", "test-secret");

        let request = create_test_request();
        let result = client.send_mint_callback(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    fn create_redeem_request() -> RedeemRequest {
        let client_id = ClientId("00000000-0000-0000-0000-000000000456".parse().unwrap());

        let tx_hash = b256!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd");
        RedeemRequest {
            issuer_request_id: IssuerRequestId(
                "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
            ),
            underlying: underlying_symbol("AAPL"),
            token: token_symbol("tAAPL"),
            client_id,
            quantity: quantity("100"),
            network: Network::Base,
            wallet: address!("0x1234567890abcdef1234567890abcdef12345678"),
            tx_hash,
        }
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem")
                .header("authorization", "Basic dGVzdC1rZXk6dGVzdC1zZWNyZXQ=")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-456",
                "issuer_request_id": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.5"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.tokenization_request_id.0, "tok-456");
        assert_eq!(
            response.issuer_request_id,
            IssuerRequestId(
                "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string()
            ),
        );
        assert!(matches!(response.r#type, TokenizationRequestType::Redeem));
        assert!(matches!(response.status, RedeemRequestStatus::Pending));
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_rejects_blank_external_symbol() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-456",
                "issuer_request_id": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "   ",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");
        let error = client
            .call_redeem_endpoint(create_redeem_request())
            .await
            .unwrap_err();

        mock.assert();
        assert!(matches!(error, AlpacaError::Parse { .. }));
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_success_without_fees() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-456",
                "issuer_request_id": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "rejected",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(result.is_ok(), "Expected Ok, got: {result:?}");
        let response = result.unwrap();
        assert_eq!(response.tokenization_request_id.0, "tok-456");
        assert!(matches!(response.status, RedeemRequestStatus::Rejected));
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_unauthorized() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(401).body("Unauthorized");
        });

        let client = make_client(&server, "test-account", "wrong-key", "wrong-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_forbidden() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(403).body("Forbidden");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(400).body("Invalid request");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        match result {
            Err(AlpacaError::Api { status_code, body }) => {
                assert_eq!(status_code, 400);
                assert_eq!(body, "Invalid request");
            }
            _ => panic!("Expected AlpacaError::Api, got {result:?}"),
        }

        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_constructs_correct_url() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/my-special-account/tokenization/callback/redeem");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-789",
                "issuer_request_id": "red-abcdefab",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "my-special-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_uses_legacy_auth() {
        let server = MockServer::start();

        // Legacy auth requires both Basic auth AND the APCA headers
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem")
                .header("authorization", "Basic bXlrZXk6bXlzZWNyZXQ=")
                .header("APCA-API-KEY-ID", "mykey")
                .header("APCA-API-SECRET-KEY", "mysecret");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-001",
                "issuer_request_id": "red-abcdefab",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "test-account", "mykey", "mysecret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_sends_correct_json() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem")
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "issuer_request_id": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                    "underlying_symbol": "AAPL",
                    "token_symbol": "tAAPL",
                    "client_id": "00000000-0000-0000-0000-000000000456",
                    "qty": "100",
                    "network": "base",
                    "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                    "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                }));
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-002",
                "issuer_request_id": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-123")
                .header("authorization", "Basic dGVzdC1rZXk6dGVzdC1zZWNyZXQ=")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-123",
                "issuer_request_id": "red-11223344",
                "type": "redeem",
                "status": "completed",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "client_external_account_id": "1481094OM",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "updated_at": "2025-09-12T17:30:00.000000-04:00",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "network": "base",
                "issuer": "test-issuer",
                "fees": "0.5",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let tokenization_request_id = TokenizationRequestId::new("tok-123");
        let result = client.poll_request_status(&tokenization_request_id).await;

        let request = result.unwrap();
        assert!(matches!(
            request,
            TokenizationRequest::Redeem {
                status: RedeemRequestStatus::Completed,
                ..
            }
        ));
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_not_found() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-NOT-FOUND");
            then.status(404).body("not found");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let tokenization_request_id = TokenizationRequestId::new("tok-NOT-FOUND");
        let result = client.poll_request_status(&tokenization_request_id).await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, AlpacaError::RequestNotFound { ref id, ref body } if id.0 == "tok-NOT-FOUND" && body == "not found"),
            "Expected RequestNotFound with correct id and body, got {err:?}"
        );
        assert!(!err.is_retryable(), "RequestNotFound must not be retryable");
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn test_poll_request_status_unauthorized() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-123");
            then.status(401).body("Unauthorized");
        });

        let client = make_client(&server, "test-account", "wrong-key", "wrong-secret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-123"))
            .await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-123");
            then.status(500).body("Internal Server Error");
        });

        let client =
            make_client(&server, "test-account", "test-key", "test-secret").with_max_retries(0);

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-123"))
            .await;

        match result {
            Err(AlpacaError::Api { status_code, .. }) => {
                assert_eq!(status_code, 500);
            }
            _ => panic!("Expected Api error, got {result:?}"),
        }

        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_uses_legacy_auth() {
        let server = MockServer::start();

        // Legacy auth requires both Basic auth AND the APCA headers
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-auth-test")
                .header("authorization", "Basic bXlrZXk6bXlzZWNyZXQ=")
                .header("APCA-API-KEY-ID", "mykey")
                .header("APCA-API-SECRET-KEY", "mysecret");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-auth-test",
                "issuer_request_id": "red-abcdefab",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "updated_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "redeem",
                "status": "pending",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "test-account", "mykey", "mysecret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-auth-test"))
            .await;

        assert!(result.is_ok());
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_constructs_correct_url() {
        let server = MockServer::start();

        // The mock only matches the exact path with account id and request id
        // as path segments — verifying the URL is constructed correctly.
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/my-special-account/tokenization/requests/tok-url-test");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-url-test",
                "issuer_request_id": "red-11223344",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "updated_at": "2025-09-12T17:30:00.000000-04:00",
                "type": "redeem",
                "status": "completed",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "my-special-account", "test-key", "test-secret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-url-test"))
            .await;

        assert!(result.is_ok(), "Expected Ok, got {result:?}");
        // mock.assert() verifies exactly this path was called — both account id
        // and tokenization_request_id appear as segments in the URL.
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_response_id_mismatch() {
        let server = MockServer::start();

        // Body returns tok-b but request was for tok-a — mismatch must be rejected.
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-a");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-b",
                "issuer_request_id": "red-11223344",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "updated_at": "2025-09-12T17:30:00.000000-04:00",
                "type": "redeem",
                "status": "completed",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-a"))
            .await;

        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                AlpacaError::ResponseIdMismatch {
                    ref requested,
                    ref returned
                } if requested.0 == "tok-a" && returned.0 == "tok-b"
            ),
            "Expected ResponseIdMismatch, got {err:?}"
        );
        assert!(
            !err.is_retryable(),
            "ResponseIdMismatch must not be retryable"
        );
        mock.assert();
    }

    #[tokio::test]
    async fn test_poll_request_status_returns_mint_variant_on_200() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/accounts/test-account/tokenization/requests/tok-mint-1");
            then.status(200).json_body(serde_json::json!({
                "tokenization_request_id": "tok-mint-1",
                "issuer_request_id": "00000000-0000-4abc-9000-4aabcdef0045",
                "created_at": "2025-09-12T17:28:48.642437-04:00",
                "type": "mint",
                "status": "completed",
                "underlying_symbol": "AAPL",
                "token_symbol": "tAAPL",
                "qty": "100",
                "issuer": "test-issuer",
                "network": "base",
                "wallet_address": "0x1234567890abcdef1234567890abcdef12345678",
                "tx_hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
                "fees": "0.0"
            }));
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new("tok-mint-1"))
            .await;

        assert!(
            matches!(result, Ok(TokenizationRequest::Mint { .. })),
            "Expected Mint variant without Parse error, got {result:?}"
        );
        mock.assert();
    }

    #[tokio::test]
    async fn test_401_returns_non_retryable_auth_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(401).body("Unauthorized");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(matches!(result, Err(AlpacaError::Auth(_))));
        assert!(!result.unwrap_err().is_retryable());
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn test_400_returns_non_retryable_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(400).body("Bad Request");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(matches!(
            result,
            Err(AlpacaError::Api {
                status_code: 400,
                ..
            })
        ));
        assert!(!result.unwrap_err().is_retryable());
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn test_500_returns_retryable_api_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(500).body("Internal Server Error");
        });

        let client =
            make_client(&server, "test-account", "test-key", "test-secret").with_max_retries(0);

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        assert!(matches!(
            result,
            Err(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));
        assert!(result.unwrap_err().is_retryable());
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn test_429_preserves_retry_after_backpressure() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(429)
                .header("retry-after", "120")
                .body("Slow down");
        });

        let client =
            make_client(&server, "test-account", "test-key", "test-secret").with_max_retries(0);
        let error = client
            .call_redeem_endpoint(create_redeem_request())
            .await
            .unwrap_err();

        assert_eq!(
            error.backpressure(),
            Some(crate::Backpressure {
                retry_after: Some(Duration::from_mins(2)),
            })
        );
        assert_eq!(error.permanence(), crate::Permanence::Transient);
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn test_call_redeem_endpoint_retries_transient_server_errors() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(500).body("Internal Server Error");
        });

        let client =
            make_client(&server, "test-account", "test-key", "test-secret").with_max_retries(2);

        let result = client.call_redeem_endpoint(create_redeem_request()).await;

        assert!(matches!(
            result,
            Err(AlpacaError::Api {
                status_code: 500,
                ..
            })
        ));
        mock.assert_calls(3);
    }

    #[tokio::test]
    async fn test_poll_request_status_parses_full_production_response() {
        let target_id = "00000000-0000-0000-0000-000000000001";
        let prod_json = r#"{"tokenization_request_id":"00000000-0000-0000-0000-000000000001","issuer_request_id":"0x1111111111111111111111111111111111111111111111111111111111111111","type":"redeem","status":"completed","underlying_symbol":"SPYM","token_symbol":"tSPYM","qty":"0.000064248","client_external_account_id":"00000000-0000-0000-0000-000000000002","created_at":"2026-06-11T00:02:27.467568Z","updated_at":"2026-06-11T04:02:33.530523Z","wallet_address":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","network":"base","issuer":"st0x","fees":"0.01","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111"}"#;

        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path(format!(
                "/v1/accounts/test-account/tokenization/requests/{target_id}"
            ));
            then.status(200).body(prod_json);
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let result = client
            .poll_request_status(&TokenizationRequestId::new(target_id))
            .await;

        let request = result.unwrap();
        match &request {
            TokenizationRequest::Redeem { id, status, .. } => {
                assert_eq!(id.0, target_id);
                assert!(matches!(status, RedeemRequestStatus::Completed));
            }
            other @ TokenizationRequest::Mint { .. } => {
                panic!("Expected Redeem variant, got {other:?}")
            }
        }
        mock.assert();
    }

    #[tokio::test]
    async fn test_200_with_invalid_json_returns_non_retryable_error() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/accounts/test-account/tokenization/callback/redeem");
            then.status(200).body("invalid json");
        });

        let client = make_client(&server, "test-account", "test-key", "test-secret");

        let request = create_redeem_request();
        let result = client.call_redeem_endpoint(request).await;

        let err = result.unwrap_err();
        assert!(matches!(err, AlpacaError::Parse { .. }));
        assert!(!err.is_retryable(), "Parse errors must NOT be retryable");
        mock.assert_calls(1);
    }
}
