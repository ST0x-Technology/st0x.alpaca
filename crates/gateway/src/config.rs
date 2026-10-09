//! Deployment config. The Alpaca account, its credential and every pinned
//! destination come from here and nowhere else.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use st0x_alpaca::broker::{AlpacaAccountId, AlpacaBrokerApiCtx, AlpacaBrokerApiMode};
use st0x_alpaca::core::{AlpacaAuth, Network};
use st0x_alpaca::corporate_actions::{
    CorporateActionEndpointError, CorporateActionStreamEndpoint,
    DEFAULT_CORPORATE_ACTIONS_STREAM_URL,
};
use st0x_alpaca::endpoint::validate_credential_origin;
use st0x_alpaca_gateway_api::{Operation, Profile};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The text is not TOML or does not fit the config. Only the parser's
    /// message is kept: the parser's own display quotes the offending line,
    /// which may hold a credential, and this error is logged at startup and
    /// by `--validate-config`.
    #[error("cannot parse config: {}", .0.to_string().trim_end())]
    Parse(#[source] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole deployment config, loaded once at startup.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// The deployment this is: its name and its capability matrix.
    pub profile: Profile,
    /// Written into every audit record. A production deployment must sign
    /// with its Cloud KMS key.
    pub environment: Environment,
    pub listen: SocketAddr,
    /// The account number Alpaca must report for `broker.account_id`.
    /// Startup refuses to serve on a mismatch.
    pub expected_account_number: String,
    /// Credential, account and mode. The only place an account id appears.
    pub broker: AlpacaBrokerApiCtx,
    pub identity: IdentityConfig,
    /// Operations switched off in this deployment. They answer
    /// `403 capability_disabled`.
    #[serde(default)]
    pub disabled_operations: Vec<Operation>,
    /// Shared budget of the read and write tiers, in calls per minute. Each
    /// process keeps its own, and a rollout runs two side by side, so this
    /// is half the human share of the credential's rate limit.
    #[serde(default = "default_human_budget")]
    pub human_budget_per_minute: u32,
    #[serde(default)]
    pub journal: JournalConfig,
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub tokenization: TokenizationConfig,
    /// Read by the `s01` profile only.
    #[serde(default)]
    pub corporate_actions: CorporateActionsConfig,
}

const fn default_human_budget() -> u32 {
    60
}

/// The deployment's environment. Closed, so a misspelled `production` fails
/// to parse instead of escaping the production credential rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    /// Must sign with its Cloud KMS key.
    Production,
    Staging,
}

impl Environment {
    /// The name config and audit records use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Staging => "staging",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    /// Audience bot ID tokens must carry: the Cloud Run service URL.
    pub bot_audience: String,
    /// Unique ids (`sub`) of the service accounts allowed on the bot tier.
    pub bot_principals: Vec<String>,
    /// IAP audience of the read backend.
    pub read_audience: String,
    /// IAP audience of the write backend.
    pub write_audience: String,
    /// Overrides Google's ID token keys, for tests. HTTPS, or HTTP on a
    /// loopback host.
    #[serde(default = "default_google_jwks_url")]
    pub google_jwks_url: String,
    /// Overrides IAP's assertion keys, for tests. HTTPS, or HTTP on a
    /// loopback host.
    #[serde(default = "default_iap_jwks_url")]
    pub iap_jwks_url: String,
}

fn default_google_jwks_url() -> String {
    "https://www.googleapis.com/oauth2/v3/certs".to_string()
}

fn default_iap_jwks_url() -> String {
    "https://www.gstatic.com/iap/verify/public_key-jwk".to_string()
}

/// Refuses a signing key URL that breaks the rule every Alpaca origin
/// follows (HTTPS, or HTTP only on a loopback host for local mocks; no
/// embedded credentials, query or fragment): these keys decide every
/// caller's identity.
fn check_jwks_url(field: &str, value: &str) -> Result<(), ConfigError> {
    validate_credential_origin(value)
        .map(drop)
        .map_err(|error| ConfigError::Invalid(format!("identity.{field}: {error}")))
}

/// Counterparties `journals.create` may send shares to, by name. Empty means
/// the operation answers `403 capability_disabled`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalConfig {
    #[serde(default)]
    pub counterparties: BTreeMap<String, AlpacaAccountId>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletConfig {
    /// The only addresses the bot tier may withdraw to. The write tier may
    /// withdraw to any approved whitelist entry except these. Empty means
    /// the bot's withdrawals answer `403 capability_disabled` while the
    /// write tier's keep serving.
    #[serde(default)]
    pub bot_withdrawal_destinations: Vec<Address>,
    /// Travel Rule beneficiary the whitelist operations attach. Never taken
    /// from a request.
    pub travel_rule_beneficiary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenizationConfig {
    /// Networks the deployment holds a tokenization client for.
    #[serde(default)]
    pub networks: Vec<Network>,
    /// The only recipients `tokenization.mint` accepts, on every tier.
    #[serde(default)]
    pub mint_recipients: Vec<Address>,
}

/// The corporate action stream the `s01` profile relays.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateActionsConfig {
    /// The stream URL, carrying its type and region filter.
    #[serde(default = "default_stream_url")]
    pub stream_url: String,
    /// Longest wait for the next stream chunk before the relay ends.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for CorporateActionsConfig {
    fn default() -> Self {
        Self {
            stream_url: default_stream_url(),
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

impl CorporateActionsConfig {
    /// The stream endpoint, checked the same way at startup and by
    /// `--validate-config`.
    ///
    /// # Errors
    ///
    /// When `stream_url` is not an endpoint the stream client may call.
    pub fn endpoint(&self) -> Result<CorporateActionStreamEndpoint, CorporateActionEndpointError> {
        // Tests serve the stream from a loopback mock that still gets the
        // credentials; a deployed gateway sends them to Alpaca's host only.
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(endpoint) =
            CorporateActionStreamEndpoint::authenticated_loopback(&self.stream_url)
        {
            return Ok(endpoint);
        }
        CorporateActionStreamEndpoint::parse(
            &self.stream_url,
            st0x_alpaca::corporate_actions::DevelopmentLoopback::Deny,
        )
    }
}

fn default_stream_url() -> String {
    DEFAULT_CORPORATE_ACTIONS_STREAM_URL.to_string()
}

const fn default_idle_timeout_secs() -> u64 {
    90
}

impl GatewayConfig {
    /// Loads and validates the config at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or parsed, or a
    /// value is invalid.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates config text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the text does not parse or a value is
    /// invalid.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|mut source: toml::de::Error| {
            source.set_input(None);
            ConfigError::Parse(source)
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError::Invalid(message.to_string()));

        // The broker mode picks the endpoint independently of the label, so
        // a staging deployment pointed at the real money Broker API signs
        // with KMS too, and a production deployment cannot escape the rule
        // by selecting the sandbox.
        let real_money = self.environment == Environment::Production
            || self.broker.mode() == AlpacaBrokerApiMode::Production;
        if real_money && !matches!(self.broker.auth, AlpacaAuth::KmsJwt { .. }) {
            return invalid(
                "a production deployment or one using the production Broker API signs with its \
                 Cloud KMS key: [broker] takes client_id and kms_key_version",
            );
        }
        // The wallet and tokenization clients mint JWT credentials at the
        // production token endpoint only, and refuse the local key: either
        // shape elsewhere starts clean and fails every wallet and
        // tokenization call, or fails startup.
        match self.broker.auth {
            AlpacaAuth::PrivateKeyJwt { .. } => {
                return invalid(
                    "the wallet and tokenization clients cannot sign with a local private key: \
                     [broker] takes api_key and api_secret, or client_id and kms_key_version",
                );
            }
            AlpacaAuth::KmsJwt { .. } if self.broker.mode() != AlpacaBrokerApiMode::Production => {
                return invalid(
                    "the wallet and tokenization clients mint Cloud KMS tokens at the production \
                     token endpoint only: a KMS credential needs [broker] mode = \"production\"",
                );
            }
            AlpacaAuth::Basic { .. } | AlpacaAuth::KmsJwt { .. } => {}
        }

        let identity = &self.identity;
        let audiences = [
            &identity.bot_audience,
            &identity.read_audience,
            &identity.write_audience,
        ];
        if audiences.iter().collect::<HashSet<_>>().len() != audiences.len() {
            return invalid("bot, read and write audiences must differ");
        }
        check_jwks_url("google_jwks_url", &identity.google_jwks_url)?;
        check_jwks_url("iap_jwks_url", &identity.iap_jwks_url)?;

        if self.profile == Profile::S01 {
            let stream = &self.corporate_actions;
            if let Err(error) = stream.endpoint() {
                return invalid(&format!("corporate_actions.stream_url: {error}"));
            }
            if stream.idle_timeout_secs == 0 {
                return invalid("corporate_actions.idle_timeout_secs must be above 0");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn is_disabled(&self, operation: Operation) -> bool {
        self.disabled_operations.contains(&operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = "api_key = \"key\"\napi_secret = \"secret\"";

    fn kms() -> String {
        "client_id = \"client\"\nkms_key_version = \
         \"projects/p/locations/europe-west3/keyRings/r/cryptoKeys/alpaca/cryptoKeyVersions/1\""
            .to_string()
    }

    fn local_pem() -> String {
        "client_id = \"client\"\nprivate_key_pem = \"-----BEGIN PRIVATE KEY-----\"".to_string()
    }

    /// [`sample`] in `environment` against the Broker API `mode`, signing
    /// with `credential`.
    fn deployment(environment: &str, mode: &str, credential: &str) -> String {
        sample("")
            .replace(
                "environment = \"staging\"",
                &format!("environment = \"{environment}\""),
            )
            .replace("mode = \"sandbox\"", &format!("mode = \"{mode}\""))
            .replace(BASIC, credential)
    }

    fn sample(extra: &str) -> String {
        format!(
            r#"
profile = "t0"
environment = "staging"
listen = "127.0.0.1:0"
expected_account_number = "T0-0001"
{extra}

[broker]
api_key = "key"
api_secret = "secret"
account_id = "904837e3-3b76-47ec-b432-046db621571b"
mode = "sandbox"

[identity]
bot_audience = "https://t0-alpaca.run.app"
bot_principals = ["111"]
read_audience = "/projects/1/global/backendServices/11"
write_audience = "/projects/1/global/backendServices/22"

[wallet]
bot_withdrawal_destinations = ["0x1111111111111111111111111111111111111111"]

[tokenization]
networks = ["base"]
mint_recipients = ["0x2222222222222222222222222222222222222222"]
"#
        )
    }

    #[test]
    fn unknown_fields_are_refused() {
        let error = GatewayConfig::parse(&sample("proxy_base_url = \"https://x\"")).unwrap_err();
        assert!(matches!(error, ConfigError::Parse(_)), "{error}");
    }

    #[test]
    fn equal_audiences_are_refused() {
        let text = sample("").replace(
            "write_audience = \"/projects/1/global/backendServices/22\"",
            "write_audience = \"/projects/1/global/backendServices/11\"",
        );
        let error = GatewayConfig::parse(&text).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
    }

    /// A config error is logged at startup and by `--validate-config`, so
    /// a malformed credential line must not reach its text.
    #[test]
    fn a_malformed_credential_line_stays_out_of_the_error() {
        const SECRET: &str = "7300419928";
        let malformed = [
            // An unterminated string and a bare word fail as TOML.
            format!("api_secret = \"{SECRET}"),
            format!("api_secret = {SECRET}x"),
            // A number where the credential takes a string fails the
            // credential shape.
            format!("api_secret = {SECRET}"),
        ];

        for line in malformed {
            let text = sample("").replace("api_secret = \"secret\"", &line);
            let error = GatewayConfig::parse(&text).unwrap_err();

            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(SECRET), "{line}: {shown}");
            assert!(matches!(error, ConfigError::Parse(_)), "{line}: {shown}");
        }
    }

    #[test]
    fn disabled_operations_parse_by_catalog_name() {
        let config =
            GatewayConfig::parse(&sample("disabled_operations = [\"wallet.withdraw\"]")).unwrap();
        assert!(config.is_disabled(Operation::WalletWithdraw));
        assert!(!config.is_disabled(Operation::OrdersCancel));
    }

    #[test]
    fn real_money_refuses_every_credential_but_the_kms_key() {
        // A production label or the production Broker API each demand KMS,
        // so neither the label nor the mode alone escapes the rule.
        for (environment, mode) in [
            ("production", "sandbox"),
            ("production", "production"),
            ("staging", "production"),
        ] {
            for credential in [BASIC.to_string(), local_pem()] {
                let error =
                    GatewayConfig::parse(&deployment(environment, mode, &credential)).unwrap_err();
                assert!(
                    matches!(&error, ConfigError::Invalid(message) if message.contains("Cloud KMS")),
                    "{environment} against {mode}: {error}"
                );
            }
        }
        GatewayConfig::parse(&deployment("production", "production", &kms())).unwrap();
        GatewayConfig::parse(&deployment("staging", "production", &kms())).unwrap();
    }

    /// The wallet and tokenization clients mint at the production token
    /// endpoint and refuse the local key, so a config either would break
    /// never validates; the sandbox runs on Basic keys.
    #[test]
    fn credentials_the_wallet_and_tokenization_clients_cannot_use_are_refused() {
        let without_mode = sample("")
            .replace("mode = \"sandbox\"\n", "")
            .replace(BASIC, &kms());
        for text in [
            deployment("staging", "sandbox", &local_pem()),
            deployment("staging", "sandbox", &kms()),
            deployment("production", "sandbox", &kms()),
            without_mode,
        ] {
            let error = GatewayConfig::parse(&text).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{text}: {error}");
        }
        GatewayConfig::parse(&deployment("staging", "sandbox", BASIC)).unwrap();
    }

    #[test]
    fn an_environment_other_than_production_or_staging_does_not_parse() {
        for environment in ["prod", "Production", "production ", "", "test"] {
            let error =
                GatewayConfig::parse(&deployment(environment, "sandbox", BASIC)).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse(_)),
                "{environment:?}: {error}"
            );
        }

        let config = GatewayConfig::parse(&deployment("staging", "sandbox", BASIC)).unwrap();
        assert_eq!(config.environment, Environment::Staging);
    }

    #[test]
    fn signing_key_urls_must_be_https_off_loopback() {
        for field in ["google_jwks_url", "iap_jwks_url"] {
            let with = |url: &str| {
                sample("").replace(
                    "[identity]\n",
                    &format!("[identity]\n{field} = \"{url}\"\n"),
                )
            };

            let error = GatewayConfig::parse(&with("http://keys.example.com/certs")).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{field}: {error}");

            GatewayConfig::parse(&with("http://127.0.0.1:8080/certs")).unwrap();
            GatewayConfig::parse(&with("https://keys.example.com/certs")).unwrap();
        }
    }

    /// `--validate-config` refuses a stream config the relay could not use.
    #[test]
    fn an_s01_stream_config_the_relay_cannot_use_is_refused() {
        let s01 = |corporate_actions: &str| {
            format!(
                "{}\n[corporate_actions]\n{corporate_actions}\n",
                sample("").replace("profile = \"t0\"", "profile = \"s01\"")
            )
        };

        for section in [
            "stream_url = \"not a url\"",
            "stream_url = \"http://stream.example.com/v2/events\"",
            "idle_timeout_secs = 0",
        ] {
            let error = GatewayConfig::parse(&s01(section)).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Invalid(message) if message.contains("corporate_actions")),
                "{section}: {error}"
            );
        }
        GatewayConfig::parse(&s01("")).unwrap();
    }
}
