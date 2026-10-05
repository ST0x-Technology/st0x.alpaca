//! Typed HTTP client for the gateway.
//!
//! One client talks to one deployment on one tier. Bots use [`Tier::Bot`]
//! with [`MetadataIdToken`]; operator tools use a human tier with the ID
//! token their OAuth flow produced ([`StaticToken`]). The client never
//! retries a request: retry decisions belong to the caller, guided by
//! [`ErrorBody::retryable`] and [`ErrorBody::retryable_with_same_key`].

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::ON_BEHALF_OF_HEADER;
use crate::access::Tier;
use crate::failure::ErrorBody;
use crate::ops::{Method, Operation};

/// Margin above an operation's deadline before the client gives up, so the
/// gateway's own deadline answer arrives first.
const DEADLINE_MARGIN: Duration = Duration::from_secs(15);

const METADATA_IDENTITY_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/identity";

/// Produces the bearer token sent with every request.
pub trait TokenSource: Send + Sync {
    /// Returns a currently valid token.
    fn token(&self) -> impl Future<Output = Result<String, ClientError>> + Send;
}

/// A token the caller already holds, such as an operator's OAuth ID token.
pub struct StaticToken(pub String);

impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String, ClientError> {
        Ok(self.0.clone())
    }
}

/// A Google ID token for the runtime service account, from the metadata
/// server, with the email claim so the gateway can audit it.
pub struct MetadataIdToken {
    audience: String,
    http: reqwest::Client,
}

impl MetadataIdToken {
    #[must_use]
    pub fn new(audience: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            audience: audience.into(),
            http,
        }
    }
}

impl TokenSource for MetadataIdToken {
    async fn token(&self) -> Result<String, ClientError> {
        let response = self
            .http
            .get(METADATA_IDENTITY_URL)
            .header("Metadata-Flavor", "Google")
            .query(&[("audience", self.audience.as_str()), ("format", "full")])
            .send()
            .await
            .map_err(ClientError::Token)?
            .error_for_status()
            .map_err(ClientError::Token)?;
        response.text().await.map_err(ClientError::Token)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not get a token: {0}")]
    Token(#[source] reqwest::Error),
    #[error("invalid gateway URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("missing path parameter {0}")]
    MissingParameter(&'static str),
    /// The request did not get an answer: connect failure, transport error
    /// or the client's own timeout. For a mutation this is as ambiguous as
    /// `outcome_unknown`.
    #[error("no answer from the gateway: {0}")]
    Transport(#[source] reqwest::Error),
    /// The gateway answered with an error body.
    #[error("{} {:?}: {}", .status, .body.code, .body.message)]
    Gateway { status: u16, body: ErrorBody },
    /// The gateway answered something that is not the contract.
    #[error("unexpected answer {status}: {detail}")]
    Unexpected { status: u16, detail: String },
}

/// Client of one gateway deployment on one tier.
pub struct GatewayClient<Token> {
    base: Url,
    tier: Tier,
    http: reqwest::Client,
    token: Token,
}

impl<Token: TokenSource> GatewayClient<Token> {
    /// `base` is the deployment's origin: the Cloud Run URL for the bot
    /// tier, the load balancer URL for the human tiers.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Url`] for an unparsable base URL.
    pub fn new(
        base: &str,
        tier: Tier,
        http: reqwest::Client,
        token: Token,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            base: Url::parse(base)?,
            tier,
            http,
            token,
        })
    }

    /// Calls `operation`. `params` fills the path's `{name}` segments,
    /// `query` becomes the query string, `body` the JSON body.
    /// `on_behalf_of` names the human a bot acts for, for the audit record.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Gateway`] with the contract error body for any
    /// non 2xx answer, [`ClientError::Transport`] when no answer arrived, and
    /// the other variants for local failures.
    pub async fn call<Query, Request, Response>(
        &self,
        operation: Operation,
        params: &[(&'static str, &str)],
        query: Option<&Query>,
        body: Option<&Request>,
        on_behalf_of: Option<&str>,
    ) -> Result<Response, ClientError>
    where
        Query: Serialize + ?Sized,
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let url = self.url(operation, params)?;
        let token = self.token.token().await?;

        let mut request = match operation.method() {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url),
        }
        .bearer_auth(token)
        .timeout(operation.deadline() + DEADLINE_MARGIN);

        if let Some(query) = query {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(human) = on_behalf_of {
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

    fn url(
        &self,
        operation: Operation,
        params: &[(&'static str, &str)],
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
                Some(name) => params
                    .iter()
                    .find(|(param, _)| *param == name)
                    .map(|(_, value)| *value)
                    .ok_or(ClientError::MissingParameter(name))?,
                None => segment,
            };
            segments.push(value);
        }

        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| ClientError::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .clear()
            .extend(segments);
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(tier: Tier) -> GatewayClient<StaticToken> {
        GatewayClient::new(
            "https://t0-alpaca.example.com",
            tier,
            reqwest::Client::new(),
            StaticToken("token".into()),
        )
        .unwrap()
    }

    #[test]
    fn path_parameters_are_encoded_into_the_tier_prefix() {
        let url = client(Tier::Bot)
            .url(Operation::AccountPositionMark, &[("symbol", "BRK/B")])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://t0-alpaca.example.com/bot/v1/account/positions/BRK%2FB/mark"
        );
    }

    #[test]
    fn a_missing_path_parameter_is_refused() {
        let error = client(Tier::Read)
            .url(Operation::AccountPositionMark, &[])
            .unwrap_err();
        assert!(matches!(error, ClientError::MissingParameter(_)), "{error}");
    }
}
