//! Authenticated connection to the corporate-action stream.
//!
//! An authenticated endpoint receives the credentials through the shared
//! APCA treatment: the `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` pair for
//! Basic credentials, or the bearer token for either JWT mode. A development
//! loopback endpoint receives no credentials, and its client never builds
//! them. Redirects are refused, so credentials cannot follow a `Location`
//! to another host.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;

use super::endpoint::{CorporateActionStreamEndpoint, CorporateActionStreamTransport};
use super::replay::CorporateActionReplay;
use super::sse::{CorporateActionDecodeBatch, CorporateActionSseDecoder};
use crate::auth::{AuthRuntime, KmsJwtError};
use crate::core::AlpacaAuth;

/// HTTP client for one validated corporate-action stream endpoint.
#[derive(Clone)]
pub struct CorporateActionStreamClient {
    http: reqwest::Client,
    endpoint: CorporateActionStreamEndpoint,
    /// `None` exactly when the endpoint is credential-free development.
    auth: Option<AuthRuntime>,
}

impl fmt::Debug for CorporateActionStreamClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CorporateActionStreamClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// The stream client could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionStreamBuildError {
    #[error("failed to build corporate-action HTTP client")]
    Client(#[from] reqwest::Error),
    #[error(transparent)]
    Auth(#[from] KmsJwtError),
}

/// Opening the stream, or reading from it, failed.
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionStreamError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("corporate-action stream returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("corporate-action stream returned content type {0}")]
    InvalidContentType(String),
    #[error(transparent)]
    Auth(#[from] KmsJwtError),
}

impl CorporateActionStreamClient {
    /// Builds a client with the given connect timeout and idle-read timeout.
    /// There is no total request timeout: the stream is long-lived, and the
    /// read timeout bounds the time between chunks.
    ///
    /// `auth` and `token_url` are used only for an authenticated endpoint;
    /// `token_url` is ignored for Basic auth and must name the environment's
    /// authx token endpoint for either JWT mode.
    ///
    /// # Errors
    ///
    /// Returns [`CorporateActionStreamBuildError::Auth`] for invalid
    /// credentials or token URL on an authenticated endpoint, or
    /// [`CorporateActionStreamBuildError::Client`] when the HTTP client cannot
    /// be built.
    pub fn new(
        endpoint: CorporateActionStreamEndpoint,
        auth: AlpacaAuth,
        token_url: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, CorporateActionStreamBuildError> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let auth = match endpoint.transport() {
            CorporateActionStreamTransport::AuthenticatedAlpaca => {
                Some(AuthRuntime::build(auth, token_url)?)
            }
            CorporateActionStreamTransport::CredentialFreeDevelopment => None,
        };
        Ok(Self {
            http,
            endpoint,
            auth,
        })
    }

    #[must_use]
    pub const fn endpoint(&self) -> &CorporateActionStreamEndpoint {
        &self.endpoint
    }

    #[must_use]
    pub const fn transport(&self) -> CorporateActionStreamTransport {
        self.endpoint.transport()
    }

    /// Opens the stream at `replay` and checks the response is a successful
    /// `text/event-stream`.
    ///
    /// # Errors
    ///
    /// Returns [`CorporateActionStreamError::Http`] on transport failure,
    /// [`CorporateActionStreamError::HttpStatus`] for any non-2xx status
    /// (including a refused redirect),
    /// [`CorporateActionStreamError::InvalidContentType`] for any other
    /// content type, or [`CorporateActionStreamError::Auth`] when a bearer
    /// token cannot be minted.
    pub async fn connect(
        &self,
        replay: &CorporateActionReplay,
    ) -> Result<CorporateActionStream, CorporateActionStreamError> {
        let request = self.http.get(self.endpoint.url().clone());
        let request = match &self.auth {
            Some(auth) => auth.apply_apca(request).await?,
            None => request,
        };
        let query = replay.query_pairs();
        let request = if query.is_empty() {
            request
        } else {
            request.query(&query)
        };

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(CorporateActionStreamError::HttpStatus(response.status()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(CorporateActionStreamError::InvalidContentType(
                content_type.to_string(),
            ));
        }

        Ok(CorporateActionStream {
            response,
            decoder: CorporateActionSseDecoder::default(),
        })
    }
}

/// An open corporate-action stream feeding its own SSE decoder.
#[derive(Debug)]
pub struct CorporateActionStream {
    response: reqwest::Response,
    decoder: CorporateActionSseDecoder,
}

impl CorporateActionStream {
    /// Reads the next body chunk and decodes it. `Ok(None)` is EOF.
    ///
    /// After a [`CorporateActionDecodeBatch::Poison`] the caller must stop
    /// reading: the stream is positioned past the rejected frame.
    ///
    /// # Errors
    ///
    /// Returns the transport error, including an idle-read timeout or a body
    /// that ended before its declared length.
    pub async fn next_batch(
        &mut self,
    ) -> Result<Option<CorporateActionDecodeBatch>, reqwest::Error> {
        let Some(chunk) = self.response.chunk().await? else {
            return Ok(None);
        };
        Ok(Some(self.decoder.push(&chunk)))
    }

    /// True while a partial frame is buffered. At EOF of a bounded replay
    /// this means the replay ended inside a frame.
    #[must_use]
    pub const fn has_pending_frame(&self) -> bool {
        self.decoder.has_pending_frame()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use httpmock::prelude::*;
    use p256::{SecretKey, pkcs8};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_test::traced_test;

    use super::*;
    use crate::corporate_actions::endpoint::DevelopmentLoopback;
    use crate::corporate_actions::event::{CorporateActionEventId, CorporateActionMutation};
    use crate::corporate_actions::replay::{
        CorporateActionBootstrapSince, CorporateActionReplayUntil,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn basic_auth() -> AlpacaAuth {
        AlpacaAuth::Basic {
            api_key: "test-key".to_string(),
            api_secret: "test-secret".to_string(),
        }
    }

    fn authenticated_client(server: &MockServer) -> CorporateActionStreamClient {
        let endpoint = CorporateActionStreamEndpoint::authenticated_loopback(&format!(
            "{}/corporate-actions",
            server.base_url()
        ))
        .unwrap();
        CorporateActionStreamClient::new(endpoint, basic_auth(), "", TIMEOUT, TIMEOUT).unwrap()
    }

    fn development_client(endpoint: &str) -> CorporateActionStreamClient {
        let endpoint =
            CorporateActionStreamEndpoint::parse(endpoint, DevelopmentLoopback::Allow).unwrap();
        CorporateActionStreamClient::new(endpoint, basic_auth(), "", TIMEOUT, TIMEOUT).unwrap()
    }

    fn sse_frame(event_id: &str, mutation: &str, action_id: &str, ex_date: &str) -> String {
        format!(
            "id: {event_id}\nevent: {mutation}\ndata: {{\"event_type\":\"cash_dividend_corporateaction_event\",\"region\":\"us\",\"ca\":{{\"id\":\"{action_id}\",\"symbol\":\"AAPL\",\"ex_date\":\"{ex_date}\"}}}}\n\n"
        )
    }

    fn bootstrap_since() -> CorporateActionBootstrapSince {
        "2026-08-31T00:00:00Z".parse().unwrap()
    }

    fn startup_cutoff() -> CorporateActionReplayUntil {
        CorporateActionReplayUntil::at(
            DateTime::parse_from_rfc3339("2026-09-01T23:59:59Z")
                .unwrap()
                .with_timezone(&Utc),
        )
    }

    async fn drain(stream: &mut CorporateActionStream) -> Vec<CorporateActionMutation> {
        let mut mutations = Vec::new();
        while let Some(batch) = stream.next_batch().await.unwrap() {
            match batch {
                CorporateActionDecodeBatch::Complete(decoded) => mutations.extend(decoded),
                CorporateActionDecodeBatch::Poison { error, .. } => {
                    panic!("unexpected poison frame: {error}")
                }
            }
        }
        mutations
    }

    #[tokio::test]
    async fn authenticated_cursor_replay_sends_apca_headers_and_since_id() {
        let cursor = CorporateActionEventId::new("01J9RPMV5TKB8WX3M4F1KZ7QH2").unwrap();
        let next_event_id = "01J9RVB6Y4ZK8M3N7QD2WX1RFP";
        let body = format!(
            "{}{}",
            sse_frame(cursor.as_str(), "insert", "ca-1", "2026-08-14"),
            sse_frame(next_event_id, "update", "ca-1", "2026-08-21"),
        );
        let server = MockServer::start();
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret")
                .header_missing("authorization")
                .query_param("since_id", cursor.as_str())
                .query_param_missing("since")
                .query_param_missing("until");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(body);
        });

        let mut stream = authenticated_client(&server)
            .connect(&CorporateActionReplay::SinceId(cursor.clone()))
            .await
            .unwrap();
        let mutations = drain(&mut stream).await;

        stream_mock.assert();
        let event_ids: Vec<_> = mutations
            .iter()
            .map(|mutation| mutation.event_id.as_str())
            .collect();
        assert_eq!(event_ids, vec![cursor.as_str(), next_event_id]);
    }

    #[traced_test]
    #[tokio::test]
    async fn authenticated_first_install_replays_from_explicit_timestamp() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let server = MockServer::start();
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret")
                .query_param("since", "2026-08-31T00:00:00Z")
                .query_param_missing("since_id")
                .query_param_missing("until");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse_frame(event_id, "insert", "ca-bootstrap", "2026-08-14"));
        });

        let mut stream = authenticated_client(&server)
            .connect(&CorporateActionReplay::Since(bootstrap_since()))
            .await
            .unwrap();
        let mutations = drain(&mut stream).await;

        stream_mock.assert();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].event_id.as_str(), event_id);
        assert!(!logs_contain("test-key"));
        assert!(!logs_contain("test-secret"));
    }

    #[traced_test]
    #[tokio::test]
    async fn authenticated_startup_replay_is_bounded_by_since_and_until() {
        let server = MockServer::start();
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .header("APCA-API-KEY-ID", "test-key")
                .header("APCA-API-SECRET-KEY", "test-secret")
                .query_param("since", "2026-08-31T00:00:00Z")
                .query_param("until", "2026-09-01T23:59:59Z")
                .query_param_missing("since_id");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("");
        });

        let mut stream = authenticated_client(&server)
            .connect(&CorporateActionReplay::Window {
                since: bootstrap_since(),
                until: startup_cutoff(),
            })
            .await
            .unwrap();

        assert!(drain(&mut stream).await.is_empty());
        assert!(!stream.has_pending_frame());
        stream_mock.assert();
        assert!(!logs_contain("test-key"));
        assert!(!logs_contain("test-secret"));
    }

    #[tokio::test]
    async fn bounded_replay_eof_inside_an_sse_frame_leaves_a_pending_frame() {
        let server = MockServer::start();
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .query_param("since", "2026-08-31T00:00:00Z")
                .query_param("until", "2026-09-01T23:59:59Z")
                .query_param_missing("since_id");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("id: 01J9RPMV5TKB8WX3M4F1KZ7QH2\nevent: insert\ndata: {");
        });

        let mut stream = authenticated_client(&server)
            .connect(&CorporateActionReplay::Window {
                since: bootstrap_since(),
                until: startup_cutoff(),
            })
            .await
            .unwrap();

        assert!(drain(&mut stream).await.is_empty());
        stream_mock.assert();
        assert!(stream.has_pending_frame());
    }

    #[tokio::test]
    async fn transport_error_follows_the_accepted_frames_without_credentials() {
        let event_id = "01J9RPMV5TKB8WX3M4F1KZ7QH2";
        let body = sse_frame(event_id, "insert", "ca-1", "2026-08-14");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 1000\r\nconnection: close\r\n\r\n{body}"
        );
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let request_bytes = connection.read(&mut request).await.unwrap();
            assert!(request_bytes > 0);
            let request = std::str::from_utf8(&request[..request_bytes])
                .unwrap()
                .to_ascii_lowercase();
            assert!(!request.contains("apca-api-key-id:"));
            assert!(!request.contains("apca-api-secret-key:"));
            assert!(!request.contains("authorization:"));
            assert!(
                request.starts_with("get /corporate-actions?since_id=01j9rpmv5tkb8wx3m4f1kz7qh2 ")
            );
            connection.write_all(response.as_bytes()).await.unwrap();
            connection.shutdown().await.unwrap();
        });
        let client = development_client(&format!("http://{address}/corporate-actions"));
        assert_eq!(
            client.transport(),
            CorporateActionStreamTransport::CredentialFreeDevelopment
        );

        let mut stream = client
            .connect(&CorporateActionReplay::SinceId(
                CorporateActionEventId::new(event_id).unwrap(),
            ))
            .await
            .unwrap();
        let mut accepted = Vec::new();
        let transport_error = loop {
            match stream.next_batch().await {
                Ok(Some(CorporateActionDecodeBatch::Complete(mutations))) => {
                    accepted.extend(mutations);
                }
                Ok(Some(CorporateActionDecodeBatch::Poison { error, .. })) => {
                    panic!("unexpected poison frame: {error}")
                }
                Ok(None) => panic!("a truncated body must surface a transport error"),
                Err(error) => break error,
            }
        };

        server.await.unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].event_id.as_str(), event_id);
        assert!(transport_error.is_body() || transport_error.is_decode());
    }

    #[tokio::test]
    async fn credential_free_live_connection_sends_no_replay_query() {
        let server = MockServer::start();
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .header_missing("APCA-API-KEY-ID")
                .header_missing("APCA-API-SECRET-KEY")
                .header_missing("authorization")
                .query_param_missing("since")
                .query_param_missing("since_id")
                .query_param_missing("until");
            then.status(200)
                .header("content-type", "text/event-stream; charset=utf-8")
                .body(sse_frame(
                    "01J9RPMV5TKB8WX3M4F1KZ7QH2",
                    "insert",
                    "ca-1",
                    "2026-08-14",
                ));
        });

        let mut stream = development_client(&format!("{}/corporate-actions", server.base_url()))
            .connect(&CorporateActionReplay::Live)
            .await
            .unwrap();

        assert_eq!(drain(&mut stream).await.len(), 1);
        stream_mock.assert();
    }

    #[tokio::test]
    async fn non_event_stream_content_type_is_rejected() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/corporate-actions");
            then.status(200)
                .header("content-type", "application/json")
                .body("{}");
        });

        let error = development_client(&format!("{}/corporate-actions", server.base_url()))
            .connect(&CorporateActionReplay::Live)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            CorporateActionStreamError::InvalidContentType(content_type)
                if content_type == "application/json"
        ));
    }

    #[tokio::test]
    async fn non_success_status_and_redirects_are_reported_as_http_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/corporate-actions");
            then.status(429);
        });
        server.mock(|when, then| {
            when.method(GET).path("/redirect");
            then.status(302)
                .header("location", "https://attacker.example/collect");
        });
        let client = authenticated_client(&server);

        let error = client
            .connect(&CorporateActionReplay::Live)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CorporateActionStreamError::HttpStatus(StatusCode::TOO_MANY_REQUESTS)
        ));

        let redirecting = CorporateActionStreamEndpoint::authenticated_loopback(&format!(
            "{}/redirect",
            server.base_url()
        ))
        .unwrap();
        let error =
            CorporateActionStreamClient::new(redirecting, basic_auth(), "", TIMEOUT, TIMEOUT)
                .unwrap()
                .connect(&CorporateActionReplay::Live)
                .await
                .unwrap_err();
        assert!(matches!(
            error,
            CorporateActionStreamError::HttpStatus(StatusCode::FOUND)
        ));
    }

    #[tokio::test]
    async fn keyless_auth_sends_the_bearer_token_instead_of_apca_headers() {
        let server = MockServer::start();
        let token_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .body_includes("client_id=CKLOCAL");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-local", "expires_in": 900 }));
        });
        let stream_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/corporate-actions")
                .header("authorization", "Bearer tok-local")
                .header_missing("APCA-API-KEY-ID")
                .header_missing("APCA-API-SECRET-KEY");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("");
        });
        let pem = SecretKey::from_slice(&[0x37; 32])
            .unwrap()
            .to_sec1_pem(pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let endpoint = CorporateActionStreamEndpoint::authenticated_loopback(&format!(
            "{}/corporate-actions",
            server.base_url()
        ))
        .unwrap();
        let client = CorporateActionStreamClient::new(
            endpoint,
            AlpacaAuth::PrivateKeyJwt {
                client_id: "CKLOCAL".to_string(),
                private_key_pem: pem,
            },
            &server.url("/token"),
            TIMEOUT,
            TIMEOUT,
        )
        .unwrap();

        client.connect(&CorporateActionReplay::Live).await.unwrap();

        token_mock.assert();
        stream_mock.assert();
    }

    #[test]
    fn credentials_are_built_only_for_an_authenticated_endpoint() {
        let invalid_header = AlpacaAuth::Basic {
            api_key: "bad\nkey".to_string(),
            api_secret: "test-secret".to_string(),
        };
        let development = CorporateActionStreamEndpoint::parse(
            "http://127.0.0.1:1/corporate-actions",
            DevelopmentLoopback::Allow,
        )
        .unwrap();
        CorporateActionStreamClient::new(development, invalid_header.clone(), "", TIMEOUT, TIMEOUT)
            .unwrap();

        let authenticated =
            CorporateActionStreamEndpoint::authenticated_loopback("http://127.0.0.1:1/stream")
                .unwrap();
        assert!(matches!(
            CorporateActionStreamClient::new(authenticated, invalid_header, "", TIMEOUT, TIMEOUT),
            Err(CorporateActionStreamBuildError::Auth(
                KmsJwtError::InvalidHeader(_)
            ))
        ));
    }
}
