//! Caller identity: IAP assertions for humans, Google ID tokens for bots.
//!
//! Ported from st0x.liquidity's `iap_auth.rs`, which verifies IAP on the ops
//! API. The differences are deliberate: one verifier type serves both token
//! kinds (IAP signs with ES256 keys, Google ID tokens with RS256 keys), and
//! the bot tier also checks the caller against an allowlist of service
//! account ids, because Cloud Run `run.invoker` is not the only way a Google
//! signed token for the gateway's audience can exist.
//!
//! Authorization of humans stays with IAP and Workspace groups: the audience
//! pinned per tier is what keeps a read token off the write tier.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use st0x_alpaca_gateway_api::{ErrorCode, Tier};
use tokio::sync::RwLock;
use tracing::warn;

use crate::answer::Failure;

const IAP_HEADER: &str = "x-goog-iap-jwt-assertion";
const IAP_ISSUER: &str = "https://cloud.google.com/iap";
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
const JWKS_TTL: Duration = Duration::from_secs(3600);
const REFRESH_FLOOR: Duration = Duration::from_secs(60);
const LEEWAY_SECS: u64 = 60;

/// The verified caller of one request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub tier: Tier,
    /// Stable unique id: the IAP user id or the service account's unique id.
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum KeyKind {
    /// IAP: ES256 keys with `x`/`y` coordinates.
    Ec,
    /// Google ID tokens: RS256 keys with `n`/`e`.
    Rsa,
}

struct CachedKeys {
    keys: Vec<(String, DecodingKey)>,
    fetched_at: Instant,
}

#[derive(Debug, thiserror::Error)]
enum TokenError {
    #[error("missing credential")]
    Missing,
    #[error("malformed credential")]
    Malformed,
    #[error("signing keys unavailable")]
    KeysUnavailable,
    #[error("unknown signing key")]
    UnknownKey,
    #[error("credential rejected")]
    Rejected,
}

/// Verifies one token kind against one audience.
pub struct TokenVerifier {
    tier: Tier,
    kind: KeyKind,
    http: reqwest::Client,
    jwks_url: String,
    validation: Validation,
    /// Allowed subjects; `None` leaves authorization to IAP.
    allowed_subjects: Option<HashSet<String>>,
    keys: RwLock<Option<CachedKeys>>,
    last_refresh_attempt: std::sync::Mutex<Option<Instant>>,
}

impl TokenVerifier {
    /// IAP assertions for a human tier.
    #[must_use]
    pub fn iap(tier: Tier, audience: &str, jwks_url: String, http: reqwest::Client) -> Self {
        Self::build(
            tier,
            KeyKind::Ec,
            &[IAP_ISSUER],
            audience,
            jwks_url,
            http,
            None,
        )
    }

    /// Google ID tokens of the allowed bot service accounts.
    #[must_use]
    pub fn bot(
        audience: &str,
        principals: &[String],
        jwks_url: String,
        http: reqwest::Client,
    ) -> Self {
        Self::build(
            Tier::Bot,
            KeyKind::Rsa,
            &GOOGLE_ISSUERS,
            audience,
            jwks_url,
            http,
            Some(principals.iter().cloned().collect()),
        )
    }

    fn build(
        tier: Tier,
        kind: KeyKind,
        issuers: &[&str],
        audience: &str,
        jwks_url: String,
        http: reqwest::Client,
        allowed_subjects: Option<HashSet<String>>,
    ) -> Self {
        let algorithm = match kind {
            KeyKind::Ec => Algorithm::ES256,
            KeyKind::Rsa => Algorithm::RS256,
        };
        let mut validation = Validation::new(algorithm);
        validation.set_audience(&[audience]);
        validation.set_issuer(issuers);
        validation.leeway = LEEWAY_SECS;
        validation.required_spec_claims = ["exp", "aud", "iss", "sub"]
            .into_iter()
            .map(String::from)
            .collect();

        Self {
            tier,
            kind,
            http,
            jwks_url,
            validation,
            allowed_subjects,
            keys: RwLock::new(None),
            last_refresh_attempt: std::sync::Mutex::new(None),
        }
    }

    fn token<'request>(&self, request: &'request Request) -> Result<&'request str, TokenError> {
        let headers = request.headers();
        let raw = match self.tier {
            Tier::Bot => headers
                .get(axum::http::header::AUTHORIZATION)
                .ok_or(TokenError::Missing)?
                .to_str()
                .map_err(|_| TokenError::Malformed)?
                .strip_prefix("Bearer ")
                .ok_or(TokenError::Malformed)?,
            Tier::Read | Tier::Write => headers
                .get(IAP_HEADER)
                .ok_or(TokenError::Missing)?
                .to_str()
                .map_err(|_| TokenError::Malformed)?,
        };
        Ok(raw)
    }

    async fn verify(&self, token: &str) -> Result<Principal, TokenError> {
        let header = decode_header(token).map_err(|_| TokenError::Malformed)?;
        let kid = header.kid.ok_or(TokenError::Malformed)?;
        let key = self.decoding_key(&kid).await?;
        let claims = decode::<Claims>(token, &key, &self.validation)
            .map_err(|error| {
                warn!(tier = ?self.tier, %error, "Caller credential failed validation");
                TokenError::Rejected
            })?
            .claims;

        Ok(Principal {
            tier: self.tier,
            subject: claims.sub,
            email: claims.email,
        })
    }

    async fn decoding_key(&self, kid: &str) -> Result<DecodingKey, TokenError> {
        if let Some(key) = self.cached_key(kid, false).await {
            return Ok(key);
        }
        self.refresh(kid).await?;
        self.cached_key(kid, true)
            .await
            .ok_or(TokenError::UnknownKey)
    }

    async fn cached_key(&self, kid: &str, allow_stale: bool) -> Option<DecodingKey> {
        let guard = self.keys.read().await;
        let key = guard.as_ref().and_then(|cached| {
            if !allow_stale && cached.fetched_at.elapsed() > JWKS_TTL {
                return None;
            }
            cached
                .keys
                .iter()
                .find(|(id, _)| id == kid)
                .map(|(_, key)| key.clone())
        });
        drop(guard);
        key
    }

    async fn refresh(&self, kid: &str) -> Result<(), TokenError> {
        let cache_is_cold = {
            let guard = self.keys.read().await;
            if let Some(cached) = guard.as_ref()
                && cached.fetched_at.elapsed() <= JWKS_TTL
                && cached.keys.iter().any(|(id, _)| id == kid)
            {
                return Ok(());
            }
            guard.is_none()
        };

        {
            let mut attempt = self
                .last_refresh_attempt
                .lock()
                .map_err(|_| TokenError::KeysUnavailable)?;
            if attempt.is_some_and(|at| at.elapsed() < REFRESH_FLOOR) {
                return if cache_is_cold {
                    Err(TokenError::KeysUnavailable)
                } else {
                    Ok(())
                };
            }
            *attempt = Some(Instant::now());
        }

        match self.fetch_keys().await {
            Ok(keys) => {
                *self.keys.write().await = Some(CachedKeys {
                    keys,
                    fetched_at: Instant::now(),
                });
                Ok(())
            }
            Err(error) => {
                warn!(tier = ?self.tier, %error, "Could not fetch signing keys");
                if self.keys.read().await.is_some() {
                    Ok(())
                } else {
                    Err(TokenError::KeysUnavailable)
                }
            }
        }
    }

    async fn fetch_keys(&self) -> Result<Vec<(String, DecodingKey)>, reqwest::Error> {
        #[derive(Deserialize)]
        struct JwkSet {
            keys: Vec<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct EcJwk {
            kid: String,
            x: String,
            y: String,
        }
        #[derive(Deserialize)]
        struct RsaJwk {
            kid: String,
            n: String,
            e: String,
        }

        let set: JwkSet = self
            .http
            .get(&self.jwks_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // One entry this code cannot use is skipped, not allowed to take the
        // whole key set down.
        let kind = self.kind;
        Ok(set
            .keys
            .into_iter()
            .filter_map(|entry| match kind {
                KeyKind::Ec => {
                    let jwk: EcJwk = serde_json::from_value(entry).ok()?;
                    DecodingKey::from_ec_components(&jwk.x, &jwk.y)
                        .ok()
                        .map(|key| (jwk.kid, key))
                }
                KeyKind::Rsa => {
                    let jwk: RsaJwk = serde_json::from_value(entry).ok()?;
                    DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
                        .ok()
                        .map(|key| (jwk.kid, key))
                }
            })
            .collect())
    }
}

/// Middleware: rejects a request without a valid credential for the tier
/// and stores the verified [`Principal`] for the handler.
pub async fn require(
    State(verifier): State<Arc<TokenVerifier>>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match verifier.token(&request) {
        Ok(token) => verifier.verify(token).await,
        Err(error) => Err(error),
    };

    let principal = match principal {
        Ok(principal) => principal,
        Err(error) => {
            warn!(
                tier = ?verifier.tier,
                path = %request.uri().path(),
                %error,
                "Refused unauthenticated request"
            );
            let code = if matches!(error, TokenError::KeysUnavailable) {
                ErrorCode::Unavailable
            } else {
                ErrorCode::Unauthenticated
            };
            return Failure::new(code, error.to_string()).into_response();
        }
    };

    if let Some(allowed) = &verifier.allowed_subjects
        && !allowed.contains(&principal.subject)
    {
        warn!(
            subject = %principal.subject,
            email = principal.email.as_deref().unwrap_or("<none>"),
            "Refused a bot token from a service account outside bot_principals"
        );
        return Failure::new(
            ErrorCode::Forbidden,
            "caller is not an allowed bot principal",
        )
        .into_response();
    }

    request.extensions_mut().insert(principal);
    next.run(request).await
}
