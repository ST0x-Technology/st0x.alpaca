//! Corporate-action stream endpoint validation.
//!
//! Credentials go only to `stream.data.alpaca.markets` over HTTPS. The one
//! exception is a development deployment pointed at a plain-HTTP loopback IP
//! (a local stream simulator), which is served credential-free. The replay
//! query parameters are reserved for the client, so a configured URL cannot
//! pin its own replay position.

use std::fmt;

use url::{Host, Url};

/// The production corporate-actions stream, filtered to US cash and stock
/// dividends.
pub const DEFAULT_CORPORATE_ACTIONS_STREAM_URL: &str = "https://stream.data.alpaca.markets/v1beta1/events/corporate-actions?type=cash_dividend_corporateaction_event,stock_dividend_corporateaction_event&region=us";

const ALPACA_STREAM_HOST: &str = "stream.data.alpaca.markets";
const RESERVED_REPLAY_QUERY_PARAMETERS: [&str; 3] = ["since", "since_id", "until"];

/// Whether a plain-HTTP loopback endpoint may be used credential-free.
/// Consumers pass [`Self::Allow`] only in their development environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevelopmentLoopback {
    Allow,
    Deny,
}

/// How a validated endpoint is contacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorporateActionStreamTransport {
    /// The Alpaca stream host over HTTPS, with Alpaca credentials.
    AuthenticatedAlpaca,
    /// A development loopback simulator, without any credentials.
    CredentialFreeDevelopment,
}

/// A validated corporate-action stream URL and the transport it implies.
#[derive(Clone, PartialEq, Eq)]
pub struct CorporateActionStreamEndpoint {
    url: Url,
    transport: CorporateActionStreamTransport,
}

impl fmt::Debug for CorporateActionStreamEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CorporateActionStreamEndpoint")
            .field("url", &self.url.as_str())
            .field("transport", &self.transport)
            .finish()
    }
}

/// A configured corporate-action stream URL was rejected.
#[derive(Debug, thiserror::Error)]
pub enum CorporateActionEndpointError {
    #[error("invalid corporate-action stream URL")]
    InvalidEndpoint(#[from] url::ParseError),
    #[error("corporate-action stream URL must use HTTPS, got {0}")]
    InsecureEndpointScheme(String),
    #[error("corporate-action stream URL must target stream.data.alpaca.markets")]
    UnexpectedEndpointHost,
    #[error("corporate-action stream URL contains reserved replay query parameter {0}")]
    ReservedReplayQueryParameter(String),
}

impl CorporateActionStreamEndpoint {
    /// Validates a configured stream URL.
    ///
    /// # Errors
    ///
    /// Returns [`CorporateActionEndpointError`] for an unparseable URL, a
    /// reserved replay query parameter (`since`, `since_id`, `until`), a
    /// non-HTTPS scheme (outside the allowed development loopback), or a host
    /// other than `stream.data.alpaca.markets`.
    pub fn parse(
        endpoint: &str,
        development_loopback: DevelopmentLoopback,
    ) -> Result<Self, CorporateActionEndpointError> {
        let url = Url::parse(endpoint)?;
        reject_reserved_replay_parameters(&url)?;
        if is_development_loopback_endpoint(&url, development_loopback) {
            return Ok(Self {
                url,
                transport: CorporateActionStreamTransport::CredentialFreeDevelopment,
            });
        }
        if url.scheme() != "https" {
            return Err(CorporateActionEndpointError::InsecureEndpointScheme(
                url.scheme().to_string(),
            ));
        }
        if url.host_str() != Some(ALPACA_STREAM_HOST) {
            return Err(CorporateActionEndpointError::UnexpectedEndpointHost);
        }
        Ok(Self {
            url,
            transport: CorporateActionStreamTransport::AuthenticatedAlpaca,
        })
    }

    /// An authenticated endpoint on a plain-HTTP loopback IP, so consumer
    /// test suites can assert the credentialed request shape against a local
    /// mock server.
    ///
    /// # Errors
    ///
    /// Returns [`CorporateActionEndpointError`] for an unparseable URL, a
    /// reserved replay query parameter, or a URL that is not plain HTTP on a
    /// loopback IP.
    #[cfg(any(test, feature = "test-support"))]
    pub fn authenticated_loopback(endpoint: &str) -> Result<Self, CorporateActionEndpointError> {
        let url = Url::parse(endpoint)?;
        reject_reserved_replay_parameters(&url)?;
        if !is_development_loopback_endpoint(&url, DevelopmentLoopback::Allow) {
            return Err(CorporateActionEndpointError::UnexpectedEndpointHost);
        }
        Ok(Self {
            url,
            transport: CorporateActionStreamTransport::AuthenticatedAlpaca,
        })
    }

    #[must_use]
    pub const fn transport(&self) -> CorporateActionStreamTransport {
        self.transport
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub(super) const fn url(&self) -> &Url {
        &self.url
    }
}

fn reject_reserved_replay_parameters(url: &Url) -> Result<(), CorporateActionEndpointError> {
    if let Some((parameter, _)) = url
        .query_pairs()
        .find(|(parameter, _)| RESERVED_REPLAY_QUERY_PARAMETERS.contains(&parameter.as_ref()))
    {
        return Err(CorporateActionEndpointError::ReservedReplayQueryParameter(
            parameter.into_owned(),
        ));
    }
    Ok(())
}

fn is_development_loopback_endpoint(url: &Url, development_loopback: DevelopmentLoopback) -> bool {
    if development_loopback != DevelopmentLoopback::Allow || url.scheme() != "http" {
        return false;
    }

    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(_)) | None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(
        endpoint: &str,
        development_loopback: DevelopmentLoopback,
    ) -> Result<CorporateActionStreamTransport, CorporateActionEndpointError> {
        CorporateActionStreamEndpoint::parse(endpoint, development_loopback)
            .map(|endpoint| endpoint.transport())
    }

    #[test]
    fn corporate_action_endpoint_restricts_credentials_to_trusted_hosts() {
        assert_eq!(
            transport(
                "https://stream.data.alpaca.markets/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Deny,
            )
            .unwrap(),
            CorporateActionStreamTransport::AuthenticatedAlpaca
        );
        assert!(matches!(
            transport(
                "http://stream.data.alpaca.markets/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Deny,
            ),
            Err(CorporateActionEndpointError::InsecureEndpointScheme(_))
        ));
        assert!(matches!(
            transport(
                "https://attacker.example/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Deny,
            ),
            Err(CorporateActionEndpointError::UnexpectedEndpointHost)
        ));
        assert_eq!(
            transport(
                "http://127.0.0.1:12345/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Allow,
            )
            .unwrap(),
            CorporateActionStreamTransport::CredentialFreeDevelopment
        );
        assert!(matches!(
            transport(
                "http://127.0.0.1:12345/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Deny,
            ),
            Err(CorporateActionEndpointError::InsecureEndpointScheme(_))
        ));
        assert!(matches!(
            transport(
                "http://attacker.example/v1beta1/events/corporate-actions",
                DevelopmentLoopback::Allow,
            ),
            Err(CorporateActionEndpointError::InsecureEndpointScheme(_))
        ));
        for parameter in ["since", "since_id", "until"] {
            let endpoint = format!(
                "https://stream.data.alpaca.markets/v1beta1/events/corporate-actions?{parameter}=reserved"
            );
            assert!(matches!(
                transport(&endpoint, DevelopmentLoopback::Deny),
                Err(CorporateActionEndpointError::ReservedReplayQueryParameter(value))
                    if value == parameter
            ));
        }
    }

    #[test]
    fn default_stream_url_is_an_authenticated_alpaca_endpoint() {
        let endpoint = CorporateActionStreamEndpoint::parse(
            DEFAULT_CORPORATE_ACTIONS_STREAM_URL,
            DevelopmentLoopback::Deny,
        )
        .unwrap();

        assert_eq!(
            endpoint.transport(),
            CorporateActionStreamTransport::AuthenticatedAlpaca
        );
        assert_eq!(endpoint.as_str(), DEFAULT_CORPORATE_ACTIONS_STREAM_URL);
    }

    #[test]
    fn authenticated_loopback_accepts_only_plain_http_loopback_ips() {
        assert_eq!(
            CorporateActionStreamEndpoint::authenticated_loopback("http://127.0.0.1:1/stream")
                .unwrap()
                .transport(),
            CorporateActionStreamTransport::AuthenticatedAlpaca
        );
        assert!(matches!(
            CorporateActionStreamEndpoint::authenticated_loopback("http://localhost:1/stream"),
            Err(CorporateActionEndpointError::UnexpectedEndpointHost)
        ));
        assert!(matches!(
            CorporateActionStreamEndpoint::authenticated_loopback(
                "http://127.0.0.1:1/stream?since_id=reserved"
            ),
            Err(CorporateActionEndpointError::ReservedReplayQueryParameter(
                _
            ))
        ));
    }
}
