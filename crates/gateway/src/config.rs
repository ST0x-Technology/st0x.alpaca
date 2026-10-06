//! Deployment config. The Alpaca account, its credential and every pinned
//! destination come from here and nowhere else.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use alloy_primitives::Address;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::broker::{AlpacaAccountId, AlpacaBrokerApiCtx};
use st0x_alpaca::core::{AlpacaAuth, Network};
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
    #[error("cannot parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole deployment config, loaded once at startup.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub profile: Profile,
    /// Written into every audit record. A production deployment must sign
    /// with its Cloud KMS key.
    pub environment: Environment,
    pub listen: SocketAddr,
    /// The account number Alpaca must report for `broker.account_id`.
    /// Startup refuses to serve on a mismatch.
    pub expected_account_number: String,
    /// Credential, account and mode. The only place an account id appears.
    /// Unknown keys are refused here too, though the library type accepts
    /// them.
    #[serde(deserialize_with = "strict_broker")]
    pub broker: AlpacaBrokerApiCtx,
    pub identity: IdentityConfig,
    /// Operations switched off in this deployment. They answer
    /// `403 capability_disabled`.
    #[serde(default)]
    pub disabled_operations: Vec<Operation>,
    /// Shared budget of the read and write tiers, in cost units per minute.
    /// Must cover the largest admission cost of an operation the tiers
    /// serve, or that operation could never be admitted.
    #[serde(default = "default_human_budget")]
    pub human_budget_per_minute: u32,
    #[serde(default)]
    pub journal: JournalConfig,
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub tokenization: TokenizationConfig,
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

/// Reads `[broker]` with the library type, then refuses every key the
/// parsed value does not account for: the library flattens an untagged
/// credential, so serde alone would ignore a misspelled `mode` (falling back
/// to its default) or a second credential beside the one it picked.
fn strict_broker<'de, D>(deserializer: D) -> Result<AlpacaBrokerApiCtx, D::Error>
where
    D: Deserializer<'de>,
{
    let table = toml::Table::deserialize(deserializer)?;
    let keys: Vec<String> = table.keys().cloned().collect();
    let broker: AlpacaBrokerApiCtx = toml::Value::Table(table)
        .try_into()
        .map_err(D::Error::custom)?;

    let accepted = broker_keys(&broker);
    if let Some(unknown) = keys.iter().find(|key| !accepted.contains(&key.as_str())) {
        return Err(D::Error::custom(format!(
            "unknown key `{unknown}` in [broker], which takes {}",
            accepted.join(", ")
        )));
    }
    Ok(broker)
}

/// The `[broker]` keys `broker` is read from: every field of the library
/// type and the keys of its credential. The patterns are exhaustive, so a
/// field or credential added to the library fails to compile here instead
/// of being refused at startup.
fn broker_keys(broker: &AlpacaBrokerApiCtx) -> [&'static str; 6] {
    let AlpacaBrokerApiCtx {
        auth,
        account_id: _,
        mode: _,
        asset_cache_ttl: _,
        time_in_force: _,
    } = broker;
    let [first, second] = match auth {
        AlpacaAuth::Basic {
            api_key: _,
            api_secret: _,
        } => ["api_key", "api_secret"],
        AlpacaAuth::KmsJwt {
            client_id: _,
            kms_key_version: _,
        } => ["client_id", "kms_key_version"],
        AlpacaAuth::PrivateKeyJwt {
            client_id: _,
            private_key_pem: _,
        } => ["client_id", "private_key_pem"],
    };
    [
        "account_id",
        "mode",
        "asset_cache_ttl",
        "time_in_force",
        first,
        second,
    ]
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
    /// withdraw to any approved whitelist entry except these.
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
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError::Invalid(message.to_string()));

        if self.environment == Environment::Production
            && !matches!(self.broker.auth, AlpacaAuth::KmsJwt { .. })
        {
            return invalid(
                "a production deployment signs with its Cloud KMS key: [broker] takes \
                 client_id and kms_key_version",
            );
        }
        if self.expected_account_number.trim().is_empty() {
            return invalid("expected_account_number must not be blank");
        }

        let identity = &self.identity;
        let audiences = [
            &identity.bot_audience,
            &identity.read_audience,
            &identity.write_audience,
        ];
        if audiences.iter().any(|audience| audience.trim().is_empty()) {
            return invalid("identity audiences must not be blank");
        }
        if audiences.iter().collect::<HashSet<_>>().len() != audiences.len() {
            return invalid("bot, read and write audiences must differ");
        }
        if identity.bot_principals.is_empty()
            || identity
                .bot_principals
                .iter()
                .any(|principal| principal.trim().is_empty())
        {
            return invalid("identity.bot_principals must list at least one service account id");
        }
        check_jwks_url("google_jwks_url", &identity.google_jwks_url)?;
        check_jwks_url("iap_jwks_url", &identity.iap_jwks_url)?;

        if self
            .journal
            .counterparties
            .values()
            .any(|account| *account == self.broker.account_id)
        {
            return invalid("a journal counterparty cannot be the deployment's own account");
        }

        if let Some(operation) = self
            .disabled_operations
            .iter()
            .find(|operation| operation.tiers(self.profile).is_empty())
        {
            return Err(ConfigError::Invalid(format!(
                "disabled operation {operation} is not served by this profile"
            )));
        }

        if let Some(operation) = self.unaffordable_operation() {
            return Err(ConfigError::Invalid(format!(
                "human_budget_per_minute = {} cannot admit {operation}, which reserves {} units \
                 per call; raise the budget or disable the operation",
                self.human_budget_per_minute,
                operation.human_budget_cost()
            )));
        }

        Ok(())
    }

    /// The costliest operation a human tier may call in this deployment
    /// whose admission cost is more than the whole human budget, if any.
    fn unaffordable_operation(&self) -> Option<Operation> {
        Operation::ALL
            .into_iter()
            .filter(|operation| {
                !self.is_disabled(*operation)
                    && operation
                        .tiers(self.profile)
                        .iter()
                        .any(|tier| tier.is_human())
            })
            .max_by_key(|operation| operation.human_budget_cost())
            .filter(|operation| operation.human_budget_cost() > self.human_budget_per_minute)
    }

    #[must_use]
    pub fn is_disabled(&self, operation: Operation) -> bool {
        self.disabled_operations.contains(&operation)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use st0x_alpaca::broker::{AlpacaBrokerApiMode, TimeInForce};

    use super::*;

    pub(crate) const ACCOUNT_ID: &str = "904837e3-3b76-47ec-b432-046db621571b";
    const BASIC: &str = "api_key = \"key\"\napi_secret = \"secret\"";
    const KMS_KEY_VERSION: &str =
        "projects/p/locations/europe-west3/keyRings/r/cryptoKeys/alpaca/cryptoKeyVersions/1";
    const PEM: &str = "-----BEGIN PRIVATE KEY-----";

    fn kms() -> String {
        format!("client_id = \"client\"\nkms_key_version = \"{KMS_KEY_VERSION}\"")
    }

    fn local_pem() -> String {
        format!("client_id = \"client\"\nprivate_key_pem = \"{PEM}\"")
    }

    /// [`sample`] in `environment`, signing with `credential`.
    fn deployment(environment: &str, credential: &str) -> String {
        sample("")
            .replace(
                "environment = \"staging\"",
                &format!("environment = \"{environment}\""),
            )
            .replace(BASIC, credential)
    }

    pub(crate) fn sample(extra: &str) -> String {
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
account_id = "{ACCOUNT_ID}"
mode = "sandbox"

[identity]
bot_audience = "https://t0-alpaca.run.app"
bot_principals = ["111"]
read_audience = "/projects/1/global/backendServices/11"
write_audience = "/projects/1/global/backendServices/22"
"#
        )
    }

    #[test]
    fn a_minimal_config_loads() {
        let config = GatewayConfig::parse(&sample("")).unwrap();
        assert_eq!(config.profile, Profile::T0);
        assert_eq!(config.human_budget_per_minute, 60);
        assert!(config.journal.counterparties.is_empty());
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

    #[test]
    fn the_own_account_cannot_be_a_journal_counterparty() {
        let text = format!(
            "{}\n[journal.counterparties]\nself = \"{ACCOUNT_ID}\"\n",
            sample("")
        );
        let error = GatewayConfig::parse(&text).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
    }

    #[test]
    fn disabled_operations_parse_by_catalog_name() {
        let config =
            GatewayConfig::parse(&sample("disabled_operations = [\"wallet.withdraw\"]")).unwrap();
        assert!(config.is_disabled(Operation::WalletWithdraw));
        assert!(!config.is_disabled(Operation::OrdersCancel));
    }

    #[test]
    fn a_misspelled_broker_key_is_refused() {
        let text = sample("").replace("mode = \"sandbox\"", "mdoe = \"production\"");
        let error = GatewayConfig::parse(&text).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Parse(parse) if parse.to_string().contains("mdoe")),
            "{error}"
        );
    }

    #[test]
    fn mixed_broker_credentials_are_refused() {
        let text = sample("").replace(
            "api_secret = \"secret\"",
            "api_secret = \"secret\"\nclient_id = \"id\"",
        );
        let error = GatewayConfig::parse(&text).unwrap_err();
        assert!(matches!(error, ConfigError::Parse(_)), "{error}");
    }

    #[test]
    fn every_credential_shape_parses_to_its_variant() {
        let config = GatewayConfig::parse(&deployment("staging", BASIC)).unwrap();
        assert!(
            matches!(
                &config.broker.auth,
                AlpacaAuth::Basic { api_key, api_secret } if api_key == "key" && api_secret == "secret"
            ),
            "{:?}",
            config.broker.auth
        );

        let config = GatewayConfig::parse(&deployment("staging", &kms())).unwrap();
        assert!(
            matches!(
                &config.broker.auth,
                AlpacaAuth::KmsJwt { client_id, kms_key_version }
                    if client_id == "client" && kms_key_version == KMS_KEY_VERSION
            ),
            "{:?}",
            config.broker.auth
        );

        let config = GatewayConfig::parse(&deployment("staging", &local_pem())).unwrap();
        assert!(
            matches!(
                &config.broker.auth,
                AlpacaAuth::PrivateKeyJwt { client_id, private_key_pem }
                    if client_id == "client" && private_key_pem == PEM
            ),
            "{:?}",
            config.broker.auth
        );
    }

    #[test]
    fn a_broker_table_setting_every_library_field_parses() {
        let text = deployment("production", &kms()).replace(
            "mode = \"sandbox\"",
            "mode = \"production\"\nasset_cache_ttl = 30\ntime_in_force = \"market_on_close\"",
        );
        let config = GatewayConfig::parse(&text).unwrap();

        // Exhaustive, so a field added to the library type fails to compile
        // here until this config sets it.
        let AlpacaBrokerApiCtx {
            auth,
            account_id,
            mode,
            asset_cache_ttl,
            time_in_force,
        } = config.broker;
        assert!(matches!(auth, AlpacaAuth::KmsJwt { .. }), "{auth:?}");
        assert_eq!(account_id.to_string(), ACCOUNT_ID);
        assert_eq!(mode, Some(AlpacaBrokerApiMode::Production));
        assert_eq!(asset_cache_ttl, Duration::from_secs(30));
        assert_eq!(time_in_force, TimeInForce::MarketOnClose);
    }

    #[test]
    fn production_refuses_every_credential_but_the_kms_key() {
        for credential in [BASIC.to_string(), local_pem()] {
            let error = GatewayConfig::parse(&deployment("production", &credential)).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
        }

        GatewayConfig::parse(&deployment("production", &kms())).unwrap();
    }

    #[test]
    fn an_environment_other_than_production_or_staging_does_not_parse() {
        for environment in ["prod", "Production", "production ", "", "test"] {
            let error = GatewayConfig::parse(&deployment(environment, BASIC)).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse(_)),
                "{environment:?}: {error}"
            );
        }

        let config = GatewayConfig::parse(&deployment("staging", BASIC)).unwrap();
        assert_eq!(config.environment, Environment::Staging);
    }

    #[test]
    fn a_human_budget_below_an_operation_cost_is_refused_naming_the_operation() {
        let error = GatewayConfig::parse(&sample("human_budget_per_minute = 9")).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid(message) if message.contains("activities.list")),
            "{error}"
        );

        GatewayConfig::parse(&sample("human_budget_per_minute = 10")).unwrap();
        // A disabled operation is never admitted, so its cost does not count.
        GatewayConfig::parse(&sample(
            "human_budget_per_minute = 4\ndisabled_operations = [\"activities.list\"]",
        ))
        .unwrap();
    }

    #[test]
    fn signing_key_urls_must_be_https_off_loopback() {
        for field in ["google_jwks_url", "iap_jwks_url"] {
            let line = |url: &str| format!("{field} = \"{url}\"\n");
            let with = |url: &str| {
                sample("").replace("[identity]\n", &format!("[identity]\n{}", line(url)))
            };

            let error = GatewayConfig::parse(&with("http://keys.example.com/certs")).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{field}: {error}");

            GatewayConfig::parse(&with("http://127.0.0.1:8080/certs")).unwrap();
            GatewayConfig::parse(&with("https://keys.example.com/certs")).unwrap();
        }
    }
}
