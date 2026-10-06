//! Test harness: an Alpaca mock, Google and IAP key servers, signed caller
//! tokens, and the gateway router driven in process.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use httpmock::prelude::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePrivateKey as _;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::traits::PublicKeyParts as _;
use serde::Serialize;
use serde_json::{Value, json};
use st0x_alpaca_gateway::audit::MemorySink;
use st0x_alpaca_gateway::config::GatewayConfig;
use st0x_alpaca_gateway::routes::{Verifiers, app};
use st0x_alpaca_gateway::state::AppState;
use st0x_alpaca_gateway_api::client::{GatewayClient, StaticToken};
use st0x_alpaca_gateway_api::{AuditEvent, Tier};
use tower::ServiceExt as _;

pub const ACCOUNT_ID: &str = "904837e3-3b76-47ec-b432-046db621571b";
pub const ACCOUNT_NUMBER: &str = "T0-0001";
pub const BOT_AUDIENCE: &str = "https://t0-alpaca.example.run.app";
pub const BOT_SUBJECT: &str = "100000000000000000001";
pub const READ_AUDIENCE: &str = "/projects/1/global/backendServices/11";
pub const WRITE_AUDIENCE: &str = "/projects/1/global/backendServices/22";
pub const MARKET_MAKER_WALLET: &str = "0x1111111111111111111111111111111111111111";
pub const BOT_WALLET: &str = "0x2222222222222222222222222222222222222222";
pub const JOURNAL_COUNTERPARTY: &str = "5d0de74b-6c5e-4a2e-9d5f-0a2c6f6b0c11";

const RSA_KID: &str = "google-test";
const EC_KID: &str = "iap-test";
const RSA_PEM: &str = include_str!("fixtures/bot-signing-key.pem");
const EC_SECRET: [u8; 32] = [7; 32];

pub struct Harness {
    pub alpaca: MockServer,
    /// Held so the key server keeps answering for the harness's lifetime.
    _keys: MockServer,
    pub app: Router,
    pub state: AppState,
    pub audit: MemorySink,
}

#[derive(Serialize)]
struct Claims<'a> {
    sub: &'a str,
    email: &'a str,
    aud: &'a str,
    iss: &'a str,
    exp: u64,
}

/// The trading account answer: identity for the startup check, funds for
/// `account.*`.
pub fn account_body(account_number: &str) -> Value {
    json!({
        "id": ACCOUNT_ID,
        "status": "ACTIVE",
        "account_number": account_number,
        "cash": "1500.25",
        "buying_power": "1400.00",
        "cash_withdrawable": "1000.10"
    })
}

/// Config text for a gateway pointed at the two mock servers. `extra` is
/// spliced in as top level keys; `tables` is appended as extra tables.
pub fn config_text(alpaca: &MockServer, keys: &MockServer, extra: &str, tables: &str) -> String {
    format!(
        r#"
profile = "t0"
environment = "test"
listen = "127.0.0.1:0"
expected_account_number = "{ACCOUNT_NUMBER}"
{extra}

[broker]
api_key = "key"
api_secret = "secret"
account_id = "{ACCOUNT_ID}"
mode = {{ type = "mock", base_url = "{alpaca}" }}

[identity]
bot_audience = "{BOT_AUDIENCE}"
bot_principals = ["{BOT_SUBJECT}"]
read_audience = "{READ_AUDIENCE}"
write_audience = "{WRITE_AUDIENCE}"
google_jwks_url = "{keys}/google"
iap_jwks_url = "{keys}/iap"

[wallet]
bot_withdrawal_destinations = ["{MARKET_MAKER_WALLET}"]
travel_rule_beneficiary = "T0 Trade Ltd"

[tokenization]
networks = ["base"]
mint_recipients = ["{BOT_WALLET}"]

[journal.counterparties]
issuer = "{JOURNAL_COUNTERPARTY}"
{tables}
"#,
        alpaca = alpaca.base_url(),
        keys = keys.base_url(),
    )
}

pub fn serve_keys(keys: &MockServer) {
    let rsa = rsa::RsaPrivateKey::from_pkcs1_pem(RSA_PEM).unwrap();
    let n = URL_SAFE_NO_PAD.encode(rsa.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(rsa.e().to_bytes_be());
    keys.mock(|when, then| {
        when.method(GET).path("/google");
        then.status(200).json_body(json!({
            "keys": [{ "kid": RSA_KID, "kty": "RSA", "alg": "RS256", "n": n, "e": e }]
        }));
    });

    let point = SigningKey::from_bytes(&EC_SECRET.into())
        .unwrap()
        .verifying_key()
        .to_encoded_point(false);
    let x = URL_SAFE_NO_PAD.encode(point.x().unwrap());
    let y = URL_SAFE_NO_PAD.encode(point.y().unwrap());
    keys.mock(|when, then| {
        when.method(GET).path("/iap");
        then.status(200).json_body(json!({
            "keys": [{ "kid": EC_KID, "kty": "EC", "crv": "P-256", "x": x, "y": y }]
        }));
    });
}

impl Harness {
    /// A gateway whose startup account check passed, with the default
    /// config.
    pub async fn start() -> Self {
        Self::start_with("", "").await
    }

    pub async fn start_with(extra: &str, tables: &str) -> Self {
        Self::start_full(extra, tables, None).await
    }

    /// A gateway whose every operation deadline is `deadline`.
    pub async fn start_with_deadline(deadline: Duration) -> Self {
        Self::start_full("", "", Some(deadline)).await
    }

    async fn start_full(extra: &str, tables: &str, deadline: Option<Duration>) -> Self {
        let alpaca = MockServer::start_async().await;
        let keys = MockServer::start_async().await;
        serve_keys(&keys);
        alpaca.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/trading/accounts/{ACCOUNT_ID}/account"));
            then.status(200).json_body(account_body(ACCOUNT_NUMBER));
        });

        let config = GatewayConfig::parse(&config_text(&alpaca, &keys, extra, tables)).unwrap();
        let audit = MemorySink::default();
        let state = AppState::connect_with_deadline(
            config,
            Arc::new(audit.clone()),
            "test".to_string(),
            deadline,
        )
        .await
        .unwrap();
        let verifiers = Verifiers::from_state(&state).unwrap();
        let app = app(state.clone(), &verifiers);

        Self {
            alpaca,
            _keys: keys,
            app,
            state,
            audit,
        }
    }

    pub fn audit_events(&self) -> Vec<AuditEvent> {
        self.audit.events()
    }

    /// Sends a request as `tier` with a valid credential.
    pub async fn call(
        &self,
        tier: Tier,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = authorized(tier, method, &format!("{}{path}", tier.prefix()), body);
        self.send(request).await
    }

    pub async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Serves the router on a loopback port and returns a typed bot tier
    /// client carrying a valid bot token. Human tiers need IAP in front to
    /// turn the bearer token into its assertion header, so they are tested
    /// through [`Self::call`].
    pub async fn bot_client(&self) -> GatewayClient<StaticToken> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = self.app.clone();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        GatewayClient::new(
            &format!("http://{address}"),
            Tier::Bot,
            reqwest::Client::new(),
            StaticToken(bot_token(BOT_SUBJECT, BOT_AUDIENCE)),
        )
        .unwrap()
    }

    /// Waits for every detached mutation to finish.
    pub async fn settle(&self) {
        self.state.tasks.close();
        self.state.tasks.wait().await;
    }
}

fn exp() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap() + 600
}

/// A Google ID token as the metadata server mints it for a service account.
pub fn bot_token(subject: &str, audience: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(RSA_KID.to_string());
    encode(
        &header,
        &Claims {
            sub: subject,
            email: "t0-liquidity@t0-liquidity.iam.gserviceaccount.com",
            aud: audience,
            iss: "https://accounts.google.com",
            exp: exp(),
        },
        &EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// An IAP assertion for a human admitted by the backend with `audience`.
pub fn iap_token(audience: &str) -> String {
    let pem = SigningKey::from_bytes(&EC_SECRET.into())
        .unwrap()
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .unwrap();
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(EC_KID.to_string());
    encode(
        &header,
        &Claims {
            sub: "accounts.google.com:1234",
            email: "operator@t0trade.com",
            aud: audience,
            iss: "https://cloud.google.com/iap",
            exp: exp(),
        },
        &EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// A request carrying the valid credential for `tier`.
pub fn authorized(tier: Tier, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let builder = match tier {
        Tier::Bot => builder.header(
            "authorization",
            format!("Bearer {}", bot_token(BOT_SUBJECT, BOT_AUDIENCE)),
        ),
        Tier::Read => builder.header("x-goog-iap-jwt-assertion", iap_token(READ_AUDIENCE)),
        Tier::Write => builder.header("x-goog-iap-jwt-assertion", iap_token(WRITE_AUDIENCE)),
    };
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}
