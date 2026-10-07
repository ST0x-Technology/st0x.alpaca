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
//!
//! The check runs inside each operation's route, so a refusal is answered
//! and audited as that operation: `not_applied` on a mutation, principal
//! [`UNVERIFIED`] when no credential verified, or the refused subject.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use st0x_alpaca_gateway_api::{ErrorCode, Tier};
use tokio::sync::RwLock;
use tracing::warn;

use crate::answer::Failure;
use crate::state::{AppState, Call, Intent};

const IAP_HEADER: &str = "x-goog-iap-jwt-assertion";
const IAP_ISSUER: &str = "https://cloud.google.com/iap";
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
const JWKS_TTL: Duration = Duration::from_secs(3600);
const REFRESH_FLOOR: Duration = Duration::from_secs(60);
const LEEWAY_SECS: u64 = 60;

/// The audit principal of a caller whose credential did not verify.
pub const UNVERIFIED: &str = "unverified";

/// The verified caller of one request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub tier: Tier,
    /// Stable unique id: the IAP user id or the service account's unique id.
    pub subject: String,
    pub email: Option<String>,
}

impl Principal {
    fn unverified(tier: Tier) -> Self {
        Self {
            tier,
            subject: UNVERIFIED.to_string(),
            email: None,
        }
    }
}

/// A caller the identity check turned away: the answer, and the caller as
/// far as the check got.
struct Refusal {
    failure: Failure,
    principal: Principal,
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

#[derive(Debug, thiserror::Error)]
enum KeyFetchError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Kept apart from success so a response this code cannot use never
    /// replaces keys that still verify genuine tokens.
    #[error("the key set holds no usable key")]
    NoUsableKey,
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

    fn token<'headers>(&self, headers: &'headers HeaderMap) -> Result<&'headers str, TokenError> {
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

    /// The verified and allowed caller of a request with `headers`.
    async fn identify(&self, headers: &HeaderMap) -> Result<Principal, Refusal> {
        let verified = match self.token(headers) {
            Ok(token) => self.verify(token).await,
            Err(error) => Err(error),
        };
        let principal = verified.map_err(|error| {
            let code = if matches!(error, TokenError::KeysUnavailable) {
                ErrorCode::Unavailable
            } else {
                ErrorCode::Unauthenticated
            };
            Refusal {
                failure: Failure::new(code, error.to_string()),
                principal: Principal::unverified(self.tier),
            }
        })?;

        if let Some(allowed) = &self.allowed_subjects
            && !allowed.contains(&principal.subject)
        {
            return Err(Refusal {
                failure: Failure::new(
                    ErrorCode::Forbidden,
                    "caller is not an allowed bot principal",
                ),
                principal,
            });
        }
        Ok(principal)
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

    async fn fetch_keys(&self) -> Result<Vec<(String, DecodingKey)>, KeyFetchError> {
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
        let keys: Vec<_> = set
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
            .collect();
        if keys.is_empty() {
            return Err(KeyFetchError::NoUsableKey);
        }
        Ok(keys)
    }
}

/// State of one tier's identity check: its verifier, and the app state a
/// refusal is audited through.
#[derive(Clone)]
pub struct Guard {
    verifier: Arc<TokenVerifier>,
    state: AppState,
}

impl Guard {
    #[must_use]
    pub fn new(verifier: Arc<TokenVerifier>, state: AppState) -> Self {
        Self { verifier, state }
    }
}

/// Middleware: stores the verified [`Principal`] for the handler, or
/// answers and audits the refusal as the route's operation.
pub async fn require(State(guard): State<Guard>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    let Refusal { failure, principal } = match guard.verifier.identify(&parts.headers).await {
        Ok(principal) => {
            parts.extensions.insert(principal);
            return next.run(Request::from_parts(parts, body)).await;
        }
        Err(refusal) => refusal,
    };

    warn!(
        tier = ?principal.tier,
        path = %parts.uri.path(),
        subject = %principal.subject,
        email = principal.email.as_deref().unwrap_or("<none>"),
        reason = %failure.message,
        "Refused a caller at the identity check"
    );
    parts.extensions.insert(principal);
    match Call::of(&mut parts) {
        Ok(call) => guard.state.refuse(&call, &Intent::default(), failure),
        Err(_) => failure,
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use httpmock::prelude::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey as _;
    use rsa::pkcs1::DecodeRsaPrivateKey as _;
    use rsa::traits::PublicKeyParts as _;
    use serde::Serialize;
    use serde_json::{Value, json};

    use super::*;

    const KID: &str = "test-key";
    const BOT_AUDIENCE: &str = "https://t0-alpaca.example.run.app";
    const BOT_SUBJECT: &str = "100000000000000000001";
    const READ_AUDIENCE: &str = "/projects/1/global/backendServices/11";
    const WRITE_AUDIENCE: &str = "/projects/1/global/backendServices/22";
    const RSA_PEM: &str = include_str!("../tests/gateway/fixtures/bot-signing-key.pem");
    /// Fixed bytes rather than a random key: a test that generates its own
    /// key can pass while the code under test ignores the key entirely.
    const EC_SECRET: [u8; 32] = [7; 32];
    const KINDS: [KeyKind; 2] = [KeyKind::Ec, KeyKind::Rsa];

    #[derive(Serialize)]
    struct Claims<'a> {
        sub: &'a str,
        aud: &'a str,
        iss: &'a str,
        exp: u64,
    }

    fn ec_signing_key() -> SigningKey {
        SigningKey::from_bytes(&EC_SECRET.into()).unwrap()
    }

    fn jwk(kind: KeyKind) -> Value {
        match kind {
            KeyKind::Ec => {
                let point = ec_signing_key().verifying_key().to_encoded_point(false);
                json!({
                    "kid": KID,
                    "kty": "EC",
                    "crv": "P-256",
                    "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
                    "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
                })
            }
            KeyKind::Rsa => {
                let key = rsa::RsaPrivateKey::from_pkcs1_pem(RSA_PEM).unwrap();
                json!({
                    "kid": KID,
                    "kty": "RSA",
                    "alg": "RS256",
                    "n": URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),
                    "e": URL_SAFE_NO_PAD.encode(key.e().to_bytes_be()),
                })
            }
        }
    }

    fn decoding_key(kind: KeyKind) -> DecodingKey {
        let jwk = jwk(kind);
        let field = |name: &str| jwk[name].as_str().unwrap().to_string();
        match kind {
            KeyKind::Ec => DecodingKey::from_ec_components(&field("x"), &field("y")).unwrap(),
            KeyKind::Rsa => DecodingKey::from_rsa_components(&field("n"), &field("e")).unwrap(),
        }
    }

    /// A key server answering `/keys` with `keys`.
    fn key_server(keys: &[Value]) -> MockServer {
        let server = MockServer::start();
        let body = json!({ "keys": keys });
        server.mock(|when, then| {
            when.method(GET).path("/keys");
            then.status(200).json_body(body);
        });
        server
    }

    fn failing_key_server() -> MockServer {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/keys");
            then.status(500);
        });
        server
    }

    /// The issuer and audience a genuine token of `kind` carries.
    fn genuine(kind: KeyKind) -> (&'static str, &'static str) {
        match kind {
            KeyKind::Ec => (IAP_ISSUER, READ_AUDIENCE),
            KeyKind::Rsa => (GOOGLE_ISSUERS[0], BOT_AUDIENCE),
        }
    }

    fn token(kind: KeyKind, key_id: &str, audience: &str, issuer: &str, expires_in: i64) -> String {
        let exp = u64::try_from(chrono::Utc::now().timestamp() + expires_in).unwrap();
        let (algorithm, key) = match kind {
            KeyKind::Ec => (
                Algorithm::ES256,
                EncodingKey::from_ec_pem(
                    ec_signing_key()
                        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
                        .unwrap()
                        .as_bytes(),
                )
                .unwrap(),
            ),
            KeyKind::Rsa => (
                Algorithm::RS256,
                EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
            ),
        };
        let mut header = Header::new(algorithm);
        header.kid = Some(key_id.to_string());
        encode(
            &header,
            &Claims {
                sub: BOT_SUBJECT,
                aud: audience,
                iss: issuer,
                exp,
            },
            &key,
        )
        .unwrap()
    }

    fn valid_token(kind: KeyKind) -> String {
        let (issuer, audience) = genuine(kind);
        token(kind, KID, audience, issuer, 300)
    }

    /// The verifier for `kind`: IAP for the read tier, or the bot tier.
    fn verifier(kind: KeyKind, keys: &MockServer) -> Arc<TokenVerifier> {
        let url = keys.url("/keys");
        let http = reqwest::Client::new();
        Arc::new(match kind {
            KeyKind::Ec => TokenVerifier::iap(Tier::Read, READ_AUDIENCE, url, http),
            KeyKind::Rsa => TokenVerifier::bot(BOT_AUDIENCE, &[BOT_SUBJECT.to_string()], url, http),
        })
    }

    /// The IAP verifier of the write tier.
    fn write_verifier(keys: &MockServer) -> Arc<TokenVerifier> {
        Arc::new(TokenVerifier::iap(
            Tier::Write,
            WRITE_AUDIENCE,
            keys.url("/keys"),
            reqwest::Client::new(),
        ))
    }

    /// Presents `token` where a caller of `tier` puts its credential and
    /// returns the status and error code the caller would get.
    async fn present(
        verifier: Arc<TokenVerifier>,
        tier: Tier,
        token: &str,
    ) -> (StatusCode, Option<String>) {
        let (name, value) = match tier {
            Tier::Bot => (axum::http::header::AUTHORIZATION, format!("Bearer {token}")),
            Tier::Read | Tier::Write => (HeaderName::from_static(IAP_HEADER), token.to_string()),
        };
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(&value).unwrap());

        let response = match verifier.identify(&headers).await {
            Ok(_) => return (StatusCode::OK, None),
            Err(refusal) => refusal.failure.into_response(),
        };
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|body| body["code"].as_str().map(str::to_string));
        (status, code)
    }

    fn tier_of(kind: KeyKind) -> Tier {
        match kind {
            KeyKind::Ec => Tier::Read,
            KeyKind::Rsa => Tier::Bot,
        }
    }

    /// Seeds the cache as if the keys were fetched two TTLs ago, with the
    /// refresh floor long expired so a refresh attempt is permitted.
    async fn seed_stale_cache(verifier: &TokenVerifier, kind: KeyKind) {
        let long_ago = Instant::now().checked_sub(JWKS_TTL * 2).unwrap();
        *verifier.keys.write().await = Some(CachedKeys {
            keys: vec![(KID.to_string(), decoding_key(kind))],
            fetched_at: long_ago,
        });
        *verifier.last_refresh_attempt.lock().unwrap() = Some(long_ago);
    }

    #[tokio::test]
    async fn accepts_a_current_token_of_either_kind() {
        for kind in KINDS {
            let keys = key_server(&[jwk(kind)]);
            let (status, _) =
                present(verifier(kind, &keys), tier_of(kind), &valid_token(kind)).await;
            assert_eq!(status, StatusCode::OK, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn rejects_an_expired_token() {
        for kind in KINDS {
            let keys = key_server(&[jwk(kind)]);
            let (issuer, audience) = genuine(kind);
            let expired = token(kind, KID, audience, issuer, -3600);

            let (status, code) = present(verifier(kind, &keys), tier_of(kind), &expired).await;

            assert_eq!(status, StatusCode::UNAUTHORIZED, "{kind:?}");
            assert_eq!(code.as_deref(), Some("unauthenticated"), "{kind:?}");
        }
    }

    /// A correctly signed token from the other Google issuer is not this
    /// kind's credential and must not pass for one.
    #[tokio::test]
    async fn rejects_a_token_from_another_issuer() {
        for kind in KINDS {
            let keys = key_server(&[jwk(kind)]);
            let (_, audience) = genuine(kind);
            let foreign_issuer = match kind {
                KeyKind::Ec => GOOGLE_ISSUERS[0],
                KeyKind::Rsa => IAP_ISSUER,
            };
            let foreign = token(kind, KID, audience, foreign_issuer, 300);

            let (status, _) = present(verifier(kind, &keys), tier_of(kind), &foreign).await;

            assert_eq!(status, StatusCode::UNAUTHORIZED, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn rejects_a_token_signed_by_a_key_the_key_set_does_not_hold() {
        for kind in KINDS {
            let keys = key_server(&[jwk(kind)]);
            let (issuer, audience) = genuine(kind);
            let unknown = token(kind, "rotated-away", audience, issuer, 300);

            let (status, code) = present(verifier(kind, &keys), tier_of(kind), &unknown).await;

            assert_eq!(status, StatusCode::UNAUTHORIZED, "{kind:?}");
            assert_eq!(code.as_deref(), Some("unauthenticated"), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn rejects_a_token_signed_by_a_foreign_key_under_a_known_key_id() {
        let keys = key_server(&[jwk(KeyKind::Ec)]);
        let impostor = SigningKey::from_bytes(&[9; 32].into())
            .unwrap()
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(KID.to_string());
        let forged = encode(
            &header,
            &Claims {
                sub: BOT_SUBJECT,
                aud: READ_AUDIENCE,
                iss: IAP_ISSUER,
                exp: u64::try_from(chrono::Utc::now().timestamp() + 300).unwrap(),
            },
            &EncodingKey::from_ec_pem(impostor.as_bytes()).unwrap(),
        )
        .unwrap();

        let (status, _) = present(verifier(KeyKind::Ec, &keys), Tier::Read, &forged).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// A cold cache plus an unreachable key endpoint is the one case the
    /// verifier cannot judge: it must fail closed and say it is unavailable,
    /// not that the caller is unknown.
    #[tokio::test]
    async fn a_cold_cache_with_unreachable_keys_answers_unavailable() {
        for kind in KINDS {
            let keys = failing_key_server();

            let (status, code) =
                present(verifier(kind, &keys), tier_of(kind), &valid_token(kind)).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{kind:?}");
            assert_eq!(code.as_deref(), Some("unavailable"), "{kind:?}");
        }
    }

    /// A stale key set beats refusing every caller over a transient failure
    /// to reach Google: after a failed refresh the retained keys are served.
    #[tokio::test]
    async fn serves_the_retained_keys_when_a_stale_cache_cannot_be_refreshed() {
        for kind in KINDS {
            let keys = MockServer::start();
            let fetch = keys.mock(|when, then| {
                when.method(GET).path("/keys");
                then.status(500);
            });
            let verifier = verifier(kind, &keys);
            seed_stale_cache(&verifier, kind).await;

            let (status, _) = present(verifier, tier_of(kind), &valid_token(kind)).await;

            assert_eq!(status, StatusCode::OK, "{kind:?}");
            fetch.assert_calls(1);
        }
    }

    /// A key service outage on a stale cache must not turn every request
    /// into an outbound fetch: the refresh floor holds.
    #[tokio::test]
    async fn a_refresh_inside_the_floor_does_not_fetch_again() {
        for kind in KINDS {
            let keys = MockServer::start();
            let fetch = keys.mock(|when, then| {
                when.method(GET).path("/keys");
                then.status(500);
            });
            let verifier = verifier(kind, &keys);
            seed_stale_cache(&verifier, kind).await;

            for _ in 0..3 {
                let (status, _) =
                    present(Arc::clone(&verifier), tier_of(kind), &valid_token(kind)).await;
                assert_eq!(status, StatusCode::OK, "{kind:?}");
            }

            fetch.assert_calls(1);
        }
    }

    /// An answer with no key this verifier can use is a key service failure,
    /// not a rotation to nothing: the retained keys keep admitting callers.
    #[tokio::test]
    async fn an_empty_key_set_does_not_replace_a_good_cache() {
        for kind in KINDS {
            let keys = MockServer::start();
            let fetch = keys.mock(|when, then| {
                when.method(GET).path("/keys");
                then.status(200).json_body(json!({ "keys": [] }));
            });
            let verifier = verifier(kind, &keys);
            seed_stale_cache(&verifier, kind).await;

            let (status, _) = present(verifier, tier_of(kind), &valid_token(kind)).await;

            assert_eq!(status, StatusCode::OK, "{kind:?}");
            fetch.assert_calls(1);
        }
    }

    /// A set holding only keys of the other kind is as unusable as an empty
    /// one; on a cold cache that leaves nothing to judge with.
    #[tokio::test]
    async fn a_key_set_with_no_usable_key_on_a_cold_cache_answers_unavailable() {
        for (kind, other) in [(KeyKind::Ec, KeyKind::Rsa), (KeyKind::Rsa, KeyKind::Ec)] {
            let keys = key_server(&[jwk(other)]);

            let (status, code) =
                present(verifier(kind, &keys), tier_of(kind), &valid_token(kind)).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{kind:?}");
            assert_eq!(code.as_deref(), Some("unavailable"), "{kind:?}");
        }
    }

    /// Both key kinds sit behind one key id here, so only the algorithm and
    /// issuer checks keep a bot's Google ID token off a human tier.
    #[tokio::test]
    async fn a_google_id_token_in_the_iap_header_is_refused() {
        let keys = key_server(&[jwk(KeyKind::Ec), jwk(KeyKind::Rsa)]);
        let iap = verifier(KeyKind::Ec, &keys);
        let (status, _) = present(Arc::clone(&iap), Tier::Read, &valid_token(KeyKind::Ec)).await;
        assert_eq!(status, StatusCode::OK, "the genuine assertion passes");

        let google = token(KeyKind::Rsa, KID, READ_AUDIENCE, GOOGLE_ISSUERS[0], 300);
        let (status, _) = present(iap, Tier::Read, &google).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_iap_assertion_as_a_bot_bearer_is_refused() {
        let keys = key_server(&[jwk(KeyKind::Ec), jwk(KeyKind::Rsa)]);
        let bot = verifier(KeyKind::Rsa, &keys);
        let (status, _) = present(Arc::clone(&bot), Tier::Bot, &valid_token(KeyKind::Rsa)).await;
        assert_eq!(status, StatusCode::OK, "the genuine bot token passes");

        let assertion = token(KeyKind::Ec, KID, BOT_AUDIENCE, IAP_ISSUER, 300);
        let (status, _) = present(bot, Tier::Bot, &assertion).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// The property the human tiers rest on: IAP binds an assertion to the
    /// backend that admitted it, so a read tier caller replaying theirs
    /// against the write tier is refused even though its signature, issuer
    /// and expiry are all valid.
    #[tokio::test]
    async fn rejects_a_token_minted_for_another_role() {
        let keys = key_server(&[jwk(KeyKind::Ec)]);
        let read_assertion = token(KeyKind::Ec, KID, READ_AUDIENCE, IAP_ISSUER, 300);
        let write_assertion = token(KeyKind::Ec, KID, WRITE_AUDIENCE, IAP_ISSUER, 300);

        let (status, _) = present(verifier(KeyKind::Ec, &keys), Tier::Read, &read_assertion).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the read tier admits its own assertion"
        );
        let (status, _) = present(write_verifier(&keys), Tier::Write, &write_assertion).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the write tier admits its own assertion"
        );

        let (status, code) = present(write_verifier(&keys), Tier::Write, &read_assertion).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(code.as_deref(), Some("unauthenticated"));
    }
}
