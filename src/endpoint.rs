//! Credential-bearing endpoint validation.
//!
//! Every URL that receives Alpaca credentials (Basic, APCA headers, or a
//! bearer token) is either a configured origin or a path resolved against
//! one. Configured origins must be HTTPS, or HTTP only on a loopback host
//! (local mocks), and must not embed credentials, a query, or a fragment.
//! Request paths are resolved against the configured origin and must stay on
//! it, so a caller cannot redirect credentials to another host by passing an
//! absolute or scheme-relative URL.

use std::fmt;
use std::net::IpAddr;

use url::Url;

/// Which configured URL failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointRole {
    /// The Alpaca API base URL requests are resolved against.
    BaseUrl,
    /// The authx token endpoint JWT credentials mint at.
    TokenUrl,
}

impl fmt::Display for EndpointRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BaseUrl => "base URL",
            Self::TokenUrl => "token URL",
        })
    }
}

/// A credential-bearing URL was rejected before any credential was attached.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("Alpaca {role} is not a valid URL")]
    Parse {
        role: EndpointRole,
        #[source]
        source: url::ParseError,
    },
    #[error("Alpaca {role} must use HTTPS (plain HTTP is accepted only for loopback hosts)")]
    InsecureScheme { role: EndpointRole },
    #[error("Alpaca {role} has no host")]
    MissingHost { role: EndpointRole },
    #[error("Alpaca {role} must not embed credentials")]
    EmbeddedCredentials { role: EndpointRole },
    #[error("Alpaca {role} must not carry a query or fragment")]
    QueryOrFragment { role: EndpointRole },
    /// URL parsers resolve `.` and `..` (even percent-encoded) and collapse
    /// empty segments, so such a value could address another endpoint.
    #[error("request path segment {segment:?} is empty or a dot segment")]
    InvalidPathSegment { segment: String },
}

/// Parses and validates a configured credential-bearing origin.
pub(crate) fn validate_origin(value: &str, role: EndpointRole) -> Result<Url, EndpointError> {
    let url = Url::parse(value).map_err(|source| EndpointError::Parse { role, source })?;

    let Some(host) = url.host_str() else {
        return Err(EndpointError::MissingHost { role });
    };

    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());

    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(EndpointError::InsecureScheme { role }),
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::EmbeddedCredentials { role });
    }

    if url.query().is_some() || url.fragment().is_some() {
        return Err(EndpointError::QueryOrFragment { role });
    }

    Ok(url)
}

/// Builds a URL on the configured `base` origin from literal path
/// `segments`, keeping any path prefix on `base`.
///
/// Every segment is percent-encoded as one path segment, so a value that
/// contains `/`, `?`, `#`, `%`, or `\\` cannot change the endpoint or leave
/// the origin. Empty and dot segments are rejected.
#[cfg(feature = "issuer")]
pub(crate) fn resolve_segments(base: &Url, segments: &[&str]) -> Result<Url, EndpointError> {
    if let Some(segment) = segments.iter().find(|segment| {
        let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
        segment.is_empty() || decoded == "." || decoded == ".."
    }) {
        return Err(EndpointError::InvalidPathSegment {
            segment: (*segment).to_string(),
        });
    }

    let mut url = base.clone();
    // `validate_origin` guarantees a host, so the URL can always be a base
    // and `path_segments_mut` cannot fail.
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_require_https_or_http_loopback() {
        for accepted in [
            "https://broker-api.alpaca.markets",
            "http://localhost:1234",
            "http://127.0.0.1:1234",
            "http://[::1]:1234",
        ] {
            validate_origin(accepted, EndpointRole::BaseUrl).unwrap();
        }

        assert!(matches!(
            validate_origin("http://broker-api.alpaca.markets", EndpointRole::BaseUrl),
            Err(EndpointError::InsecureScheme {
                role: EndpointRole::BaseUrl
            })
        ));
        assert!(matches!(
            validate_origin("http://192.0.2.1:1234", EndpointRole::TokenUrl),
            Err(EndpointError::InsecureScheme {
                role: EndpointRole::TokenUrl
            })
        ));
        assert!(matches!(
            validate_origin(
                "https://user:pass@broker-api.alpaca.markets",
                EndpointRole::BaseUrl
            ),
            Err(EndpointError::EmbeddedCredentials { .. })
        ));
        assert!(matches!(
            validate_origin(
                "https://broker-api.alpaca.markets?token=secret",
                EndpointRole::BaseUrl
            ),
            Err(EndpointError::QueryOrFragment { .. })
        ));
        assert!(matches!(
            validate_origin("javascript:alert(1)", EndpointRole::BaseUrl),
            Err(EndpointError::MissingHost { .. })
        ));
        assert!(matches!(
            validate_origin("not a url", EndpointRole::BaseUrl),
            Err(EndpointError::Parse { .. })
        ));
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn segments_resolve_on_the_configured_origin_keeping_its_prefix() {
        let base =
            validate_origin("https://broker-api.alpaca.markets/", EndpointRole::BaseUrl).unwrap();
        assert_eq!(
            resolve_segments(
                &base,
                &["v1", "accounts", "abc", "tokenization", "requests", "xyz"]
            )
            .unwrap()
            .as_str(),
            "https://broker-api.alpaca.markets/v1/accounts/abc/tokenization/requests/xyz"
        );

        let prefixed =
            validate_origin("http://127.0.0.1:9000/proxy", EndpointRole::BaseUrl).unwrap();
        assert_eq!(
            resolve_segments(&prefixed, &["v1", "orders"])
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9000/proxy/v1/orders"
        );
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn url_syntax_in_a_segment_is_encoded_and_stays_on_the_endpoint() {
        let base =
            validate_origin("https://broker-api.alpaca.markets", EndpointRole::BaseUrl).unwrap();

        for (segment, encoded) in [
            ("a?b=c", "a%3Fb=c"),
            ("a#frag", "a%23frag"),
            ("a/b", "a%2Fb"),
            ("a%2Fb", "a%252Fb"),
            ("a\\b", "a%5Cb"),
            ("//attacker.example", "%2F%2Fattacker.example"),
        ] {
            let url = resolve_segments(&base, &["v1", "accounts", segment, "x"]).unwrap();
            assert_eq!(
                url.host_str(),
                Some("broker-api.alpaca.markets"),
                "{segment}"
            );
            assert_eq!(url.query(), None, "{segment}");
            assert_eq!(url.fragment(), None, "{segment}");
            assert_eq!(url.path(), format!("/v1/accounts/{encoded}/x"), "{segment}");
        }
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn empty_and_dot_segments_are_rejected() {
        let base =
            validate_origin("https://broker-api.alpaca.markets", EndpointRole::BaseUrl).unwrap();

        for segment in ["", ".", "..", "%2e", "%2E%2e", ".%2E"] {
            assert!(
                matches!(
                    resolve_segments(&base, &["v1", "accounts", segment, "x"]),
                    Err(EndpointError::InvalidPathSegment { .. })
                ),
                "{segment:?}"
            );
        }
    }
}
