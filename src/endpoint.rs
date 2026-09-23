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
    #[error("request path {path:?} must be an absolute path on the configured Alpaca origin")]
    ForeignPath { path: String },
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

/// Resolves `path` (for example `/v1/accounts/{id}/...`, optionally with a
/// query string) against the configured `base` origin, keeping any path
/// prefix on `base`.
///
/// Rejects anything that is not an absolute path, including scheme-relative
/// (`//host`) and absolute URLs, and re-checks that the resolved URL kept the
/// base origin.
#[cfg(feature = "issuer")]
pub(crate) fn resolve_path(base: &Url, path: &str) -> Result<Url, EndpointError> {
    let foreign = || EndpointError::ForeignPath {
        path: path.to_string(),
    };

    if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return Err(foreign());
    }

    let joined = format!("{}{path}", base.as_str().trim_end_matches('/'));
    let url = Url::parse(&joined).map_err(|_| foreign())?;

    if url.origin() != base.origin() {
        return Err(foreign());
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
    fn paths_resolve_on_the_configured_origin_keeping_its_prefix() {
        let base =
            validate_origin("https://broker-api.alpaca.markets/", EndpointRole::BaseUrl).unwrap();
        assert_eq!(
            resolve_path(&base, "/v1/accounts/abc/tokenization/requests/xyz")
                .unwrap()
                .as_str(),
            "https://broker-api.alpaca.markets/v1/accounts/abc/tokenization/requests/xyz"
        );

        let prefixed =
            validate_origin("http://127.0.0.1:9000/proxy", EndpointRole::BaseUrl).unwrap();
        assert_eq!(
            resolve_path(&prefixed, "/v1/orders?status=open")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9000/proxy/v1/orders?status=open"
        );
    }

    #[cfg(feature = "issuer")]
    #[test]
    fn foreign_targets_are_rejected_before_credentials_attach() {
        let base =
            validate_origin("https://broker-api.alpaca.markets", EndpointRole::BaseUrl).unwrap();

        for foreign in [
            "https://attacker.example/collect",
            "//attacker.example/collect",
            "attacker.example/collect",
            "",
            "/\\attacker.example",
        ] {
            assert!(
                matches!(
                    resolve_path(&base, foreign),
                    Err(EndpointError::ForeignPath { .. })
                ),
                "{foreign}"
            );
        }

        assert!(matches!(
            resolve_path(&base, "@attacker.example/collect"),
            Err(EndpointError::ForeignPath { .. })
        ));
        assert_eq!(
            resolve_path(&base, "/@attacker.example/collect")
                .unwrap()
                .host_str(),
            Some("broker-api.alpaca.markets")
        );
    }
}
