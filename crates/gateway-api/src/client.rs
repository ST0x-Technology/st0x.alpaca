//! Typed HTTP client for the gateway.
//!
//! One client talks to one deployment on one tier. Bots use [`Tier::Bot`]
//! with [`MetadataIdToken`]; operator tools use a human tier with the ID
//! token their OAuth flow produced ([`StaticToken`]). The client never
//! retries a request: retry decisions belong to the caller, guided by
//! [`ErrorBody::retryable`] and [`ErrorBody::retryable_with_same_key`].
//!
//! Every call, token included, is bounded by the operation's deadline plus a
//! margin, so the gateway's own deadline answer arrives first. No redirect is
//! followed. For a mutation, [`ClientError::Transport`] (unless the request
//! never built or never connected), [`ClientError::Timeout`] and
//! [`ClientError::Unexpected`] are as ambiguous as `outcome_unknown`: the
//! request may have reached Alpaca.
//!
//! [`GatewayClient::broker`], [`GatewayClient::wallet`] and
//! [`GatewayClient::tokenization`] are the operations, shaped like the
//! library's own services with its types and errors, so the library's poll
//! loops run over the gateway unchanged.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use st0x_alpaca::endpoint::{EndpointError, validate_credential_origin};
use url::Url;

use crate::ON_BEHALF_OF_HEADER;
use crate::access::Tier;
use crate::failure::ErrorBody;
use crate::ops::{Method, Operation};

mod adapter;

pub use adapter::{GatewayBroker, GatewayTokenization, GatewayWallet};

/// Margin above an operation's deadline before the client gives up, so the
/// gateway's own deadline answer arrives first.
const DEADLINE_MARGIN: Duration = Duration::from_secs(15);

/// How long the gateway client waits for a connection, the connect bound
/// the wallet, broker and tokenization clients use. A connection that never
/// opens means the request never left, and the error it ends in is a
/// [`ClientError::Transport`] that reads as never left, so the caller may
/// send again. Without it a blackholed connect would run into the
/// operation's whole bound and end as [`ClientError::Timeout`], which for a
/// mutation reads as an unknown outcome.
const GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

/// Bounds of one metadata server token request.
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// Produces the credential sent with every request.
pub trait TokenSource: Send + Sync {
    /// Returns a currently valid token.
    fn token(&self) -> impl Future<Output = Result<String, ClientError>> + Send;

    /// The header the token travels in. `authorization` carries it as a
    /// bearer token; any other header carries the bare token, as a proxy in
    /// front of the gateway would set it.
    fn header_name(&self) -> HeaderName {
        AUTHORIZATION
    }
}

/// A token the caller already holds, such as an operator's OAuth ID token.
#[derive(Clone)]
pub struct StaticToken(pub String);

impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String, ClientError> {
        Ok(self.0.clone())
    }
}

/// A Google ID token for the runtime service account, from the metadata
/// server, with the email claim so the gateway can audit it.
///
/// Uses its own HTTP client: no proxy (the token must not leave the host),
/// no redirects (a redirected answer is an error, never a hop), and its own
/// connect and total timeouts.
#[derive(Clone)]
pub struct MetadataIdToken {
    audience: String,
    endpoint: String,
    http: reqwest::Client,
}

impl MetadataIdToken {
    /// # Errors
    ///
    /// [`ClientError::Token`] when the HTTP client cannot be built.
    pub fn new(audience: impl Into<String>) -> Result<Self, ClientError> {
        Self::with_endpoint(audience, METADATA_IDENTITY_URL)
    }

    fn with_endpoint(
        audience: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(METADATA_CONNECT_TIMEOUT)
            .timeout(METADATA_TIMEOUT)
            .build()
            .map_err(ClientError::Token)?;
        Ok(Self {
            audience: audience.into(),
            endpoint: endpoint.into(),
            http,
        })
    }
}

impl TokenSource for MetadataIdToken {
    async fn token(&self) -> Result<String, ClientError> {
        let response = self
            .http
            .get(&self.endpoint)
            .header("Metadata-Flavor", "Google")
            .query(&[("audience", self.audience.as_str()), ("format", "full")])
            .send()
            .await
            .map_err(ClientError::Token)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::TokenStatus(status.as_u16()));
        }
        response.text().await.map_err(ClientError::Token)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not get a token: {0}")]
    Token(#[source] reqwest::Error),
    /// The metadata server answered without a token, including a redirect,
    /// which is never followed.
    #[error("the metadata server answered {0} instead of a token")]
    TokenStatus(u16),
    /// The base URL failed the rule `st0x-alpaca` applies to every
    /// credential bearing origin, so no token was attached to it.
    #[error("refusing to send a gateway token to this URL: {0}")]
    Origin(#[from] EndpointError),
    /// The HTTP client could not be built, so nothing was sent.
    #[error("could not build the HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
    /// A method did not supply a parameter its path names. A bug in the
    /// client, caught before anything is sent.
    #[error("missing path parameter {0}")]
    MissingParameter(&'static str),
    /// A path parameter value is empty or a dot segment.
    /// `PathSegmentsMut::extend` drops `.` and `..` segments, and an empty
    /// one leaves an empty segment, so such a value could address another
    /// route. Caught before anything is sent.
    #[error("path parameter {name} value {value:?} is empty or a dot segment")]
    InvalidParameter { name: &'static str, value: String },
    /// The request did not get an answer. A connect failure, the connect
    /// bound included, never left. Any other transport error, for a
    /// mutation, is as ambiguous as `outcome_unknown`: the request may have
    /// reached the gateway and Alpaca.
    #[error("no answer from the gateway: {0}")]
    Transport(#[source] reqwest::Error),
    /// No answer within the operation's deadline plus the client's margin,
    /// token fetch included. For a mutation this is as ambiguous as
    /// `outcome_unknown`.
    #[error("no answer from the gateway within {0:?}")]
    Timeout(Duration),
    /// The gateway answered with an error body.
    #[error("{} {:?}: {}", .status, .body.code, .body.message)]
    Gateway { status: u16, body: ErrorBody },
    /// The gateway, or something in front of it, answered something that is
    /// not the contract: a 2xx body that does not decode, or a non 2xx body
    /// that is not an [`ErrorBody`] (a load balancer page). For a mutation
    /// this is as ambiguous as `outcome_unknown`: a 2xx means Alpaca applied
    /// it, and an intermediary's error page says nothing about whether the
    /// request reached the gateway.
    #[error("unexpected answer {status}: {detail}")]
    Unexpected { status: u16, detail: String },
}

/// Client of one gateway deployment on one tier.
#[derive(Clone)]
pub struct GatewayClient<Token> {
    base: Url,
    tier: Tier,
    http: reqwest::Client,
    token: Token,
    on_behalf_of: Option<String>,
}

/// No path parameter, no query, no body.
const NONE: Option<&()> = None;

impl<Token: TokenSource> GatewayClient<Token> {
    /// `base` is the deployment's origin: the Cloud Run URL for the bot
    /// tier, the load balancer URL for the human tiers. A path on it is kept
    /// as the prefix of every request path.
    ///
    /// The client builds its own HTTP client, which follows no redirect: a
    /// redirected answer is [`ClientError::Unexpected`], never a hop. reqwest
    /// strips only `Authorization` and cookies when a redirect leaves the
    /// origin, so following one would hand a token in any other header to
    /// that origin, and a 307 or 308 would send a mutation a second time.
    ///
    /// It bounds the connect by `GATEWAY_CONNECT_TIMEOUT` and sets no total
    /// request timeout, so the only total bound is the operation's own, in
    /// `send`. A connect that never opens never left, so cutting it reads as
    /// retryable; a client wide total timeout shorter than an operation's
    /// bound could cut a mutation that already left, which then reads as an
    /// unknown outcome.
    ///
    /// # Errors
    ///
    /// [`ClientError::Origin`] for a base URL that does not parse, is not
    /// HTTPS (plain HTTP only on a loopback host), has no host, or carries
    /// credentials, a query or a fragment. [`ClientError::HttpClient`] when
    /// the HTTP client cannot be built.
    pub fn new(base: &str, tier: Tier, token: Token) -> Result<Self, ClientError> {
        let base = validate_credential_origin(base)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(GATEWAY_CONNECT_TIMEOUT)
            .build()
            .map_err(ClientError::HttpClient)?;
        Ok(Self {
            base,
            tier,
            http,
            token,
            on_behalf_of: None,
        })
    }

    /// A copy of this client that names `human` in `X-On-Behalf-Of` on every
    /// request, for a bot route acting for an IAP verified operator. Audit
    /// only; the gateway never authorizes on it.
    #[must_use]
    pub fn acting_for(&self, human: impl Into<String>) -> Self
    where
        Token: Clone,
    {
        Self {
            on_behalf_of: Some(human.into()),
            ..self.clone()
        }
    }

    /// Sends one operation, bounded as a whole (token included) by the
    /// operation's deadline plus [`DEADLINE_MARGIN`].
    pub(crate) async fn send<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, ClientError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let bound = operation.deadline() + DEADLINE_MARGIN;
        tokio::time::timeout(bound, self.exchange(operation, params, query, body))
            .await
            .map_err(|_| ClientError::Timeout(bound))?
    }

    async fn exchange<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
        query: Option<&Query>,
        body: Option<&Request>,
    ) -> Result<Response, ClientError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let url = self.url(operation, params)?;
        let token = self.token.token().await?;

        let request = match operation.method() {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url),
        };
        let mut request = match self.token.header_name() {
            header if header == AUTHORIZATION => request.bearer_auth(token),
            header => match HeaderValue::from_str(&token) {
                Ok(mut value) => {
                    value.set_sensitive(true);
                    request.header(header, value)
                }
                // Refused as a builder error before anything is sent, as
                // `bearer_auth` refuses a token that is no header value.
                Err(_) => request.header(header, token),
            },
        };

        if let Some(query) = query {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(human) = &self.on_behalf_of {
            request = request.header(ON_BEHALF_OF_HEADER, human);
        }

        let response = request.send().await.map_err(ClientError::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(ClientError::Transport)?;

        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|error| ClientError::Unexpected {
                status: status.as_u16(),
                detail: error.to_string(),
            });
        }

        match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(body) => Err(ClientError::Gateway {
                status: status.as_u16(),
                body,
            }),
            Err(_) => Err(ClientError::Unexpected {
                status: status.as_u16(),
                detail: String::from_utf8_lossy(&bytes).chars().take(512).collect(),
            }),
        }
    }

    /// Builds the URL from the base URL's path, the tier prefix and the
    /// operation's path, each parameter encoded as one path segment. Like
    /// `st0x_alpaca`'s own path building, it refuses an empty or dot
    /// segment value, even percent encoded.
    fn url(
        &self,
        operation: Operation,
        params: &[(&'static str, String)],
    ) -> Result<Url, ClientError> {
        let mut segments: Vec<&str> = self
            .tier
            .prefix()
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        for segment in operation
            .path()
            .split('/')
            .filter(|segment| !segment.is_empty())
        {
            let value = match segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
            {
                Some(name) => {
                    let value = params
                        .iter()
                        .find(|(param, _)| *param == name)
                        .map(|(_, value)| value.as_str())
                        .ok_or(ClientError::MissingParameter(name))?;
                    let decoded = value.to_ascii_lowercase().replace("%2e", ".");
                    if value.is_empty() || decoded == "." || decoded == ".." {
                        return Err(ClientError::InvalidParameter {
                            name,
                            value: value.to_string(),
                        });
                    }
                    value
                }
                None => segment,
            };
            segments.push(value);
        }

        let mut url = self.base.clone();
        // `validate_credential_origin` guarantees a host, so the URL can
        // always be a base and `path_segments_mut` cannot fail.
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::dto::wallet::WithdrawRequest;

    fn client(tier: Tier) -> GatewayClient<StaticToken> {
        GatewayClient::new(
            "https://t0-alpaca.example.com",
            tier,
            StaticToken("token".into()),
        )
        .unwrap()
    }

    #[test]
    fn only_https_or_loopback_http_origins_get_a_token() {
        for accepted in [
            "https://t0-alpaca.example.com",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            GatewayClient::new(accepted, Tier::Bot, StaticToken("token".into())).unwrap();
        }

        for refused in [
            "http://t0-alpaca.example.com",
            "http://10.0.0.5:8080",
            "https://user:pass@t0-alpaca.example.com",
            "https://t0-alpaca.example.com?audience=x",
            "unix:/run/gateway.sock",
            "not a url",
        ] {
            let Err(error) = GatewayClient::new(refused, Tier::Bot, StaticToken("token".into()))
            else {
                panic!("{refused} was accepted");
            };
            assert!(
                matches!(error, ClientError::Origin(_)),
                "{refused}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_redirect_from_the_metadata_server_is_an_error_not_a_hop() {
        let server = httpmock::MockServer::start_async().await;
        let redirect = server
            .mock_async(|when, then| {
                when.path("/identity")
                    .header("Metadata-Flavor", "Google")
                    .query_param("audience", "https://t0-alpaca.example.com");
                then.status(302)
                    .header("Location", format!("{}/other", server.base_url()));
            })
            .await;
        let target = server
            .mock_async(|when, then| {
                when.path("/other");
                then.status(200).body("must-not-be-read");
            })
            .await;
        let source = MetadataIdToken::with_endpoint(
            "https://t0-alpaca.example.com",
            format!("{}/identity", server.base_url()),
        )
        .unwrap();

        let error = source.token().await.unwrap_err();

        assert!(matches!(error, ClientError::TokenStatus(302)), "{error}");
        redirect.assert_async().await;
        target.assert_calls_async(0).await;
    }

    /// A token in a header of its own, as the IAP assertion travels.
    struct HeaderToken;

    impl TokenSource for HeaderToken {
        async fn token(&self) -> Result<String, ClientError> {
            Ok("assertion".into())
        }

        fn header_name(&self) -> HeaderName {
            HeaderName::from_static("x-goog-iap-jwt-assertion")
        }
    }

    /// reqwest drops only `Authorization` and cookies on a cross origin
    /// hop, and resends a POST on a 307, so a followed redirect would hand
    /// the token to another origin and could apply the mutation twice.
    #[tokio::test]
    async fn a_redirected_mutation_is_not_followed_to_another_origin() {
        let gateway = httpmock::MockServer::start_async().await;
        let elsewhere = httpmock::MockServer::start_async().await;
        let redirect = gateway
            .mock_async(|when, then| {
                when.path("/alpaca-write/v1/wallet/withdrawals")
                    .header("x-goog-iap-jwt-assertion", "assertion");
                then.status(307).header(
                    "Location",
                    format!(
                        "{}/alpaca-write/v1/wallet/withdrawals",
                        elsewhere.base_url()
                    ),
                );
            })
            .await;
        let target = elsewhere
            .mock_async(|when, then| {
                when.any_request();
                then.status(200).body("{}");
            })
            .await;
        let client = GatewayClient::new(&gateway.base_url(), Tier::Write, HeaderToken).unwrap();
        let withdrawal: WithdrawRequest = serde_json::from_value(serde_json::json!({
            "amount": "10",
            "asset": "USDC",
            "address": "0x3333333333333333333333333333333333333333",
            "operationId": Uuid::new_v4(),
        }))
        .unwrap();

        let error = client
            .send::<(), _, serde_json::Value>(
                Operation::WalletWithdraw,
                &[],
                NONE,
                Some(&withdrawal),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, ClientError::Unexpected { status: 307, .. }),
            "{error}"
        );
        redirect.assert_async().await;
        target.assert_calls_async(0).await;
    }

    #[test]
    fn path_parameters_are_encoded_into_the_tier_prefix() {
        let url = client(Tier::Bot)
            .url(
                Operation::AccountPositionMark,
                &[("symbol", "BRK/B".to_string())],
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://t0-alpaca.example.com/bot/v1/account/positions/BRK%2FB/mark"
        );
    }

    /// `PathSegmentsMut::extend` drops `.` and `..` segments, so `..` as a
    /// request id would turn the keyed read into the list route.
    #[test]
    fn an_empty_or_dot_segment_parameter_is_refused() {
        for value in ["..", "%2E%2e", "."] {
            let error = client(Tier::Bot)
                .url(
                    Operation::TokenizationRequest,
                    &[("tokenization_request_id", value.to_string())],
                )
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    ClientError::InvalidParameter {
                        name: "tokenization_request_id",
                        ..
                    }
                ),
                "{value}: {error}"
            );
        }
        let error = client(Tier::Bot)
            .url(Operation::AccountPositionMark, &[("symbol", String::new())])
            .unwrap_err();
        assert!(
            matches!(error, ClientError::InvalidParameter { name: "symbol", .. }),
            "{error}"
        );
    }

    /// A load balancer routing by path prefix serves the gateway below it.
    #[test]
    fn a_path_on_the_base_url_prefixes_every_request() {
        for base in [
            "https://lb.example.com/t0-alpaca",
            "https://lb.example.com/t0-alpaca/",
        ] {
            let url = GatewayClient::new(base, Tier::Read, StaticToken("token".into()))
                .unwrap()
                .url(Operation::AccountFunds, &[])
                .unwrap();
            assert_eq!(
                url.as_str(),
                "https://lb.example.com/t0-alpaca/alpaca-read/v1/account/funds",
                "{base}"
            );
        }
    }
}
