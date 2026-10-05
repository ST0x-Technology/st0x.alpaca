//! Deployment config. The Alpaca account, its credential and every pinned
//! destination come from here and nowhere else.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;

use alloy_primitives::Address;
use serde::Deserialize;
use st0x_alpaca::broker::{AlpacaAccountId, AlpacaBrokerApiCtx};
use st0x_alpaca::core::Network;
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
    /// `production` or `staging`, written into every audit record.
    pub environment: String,
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
    /// Shared budget of the read and write tiers, in cost units per minute.
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
    /// Overrides Google's ID token keys, for tests.
    #[serde(default = "default_google_jwks_url")]
    pub google_jwks_url: String,
    /// Overrides IAP's assertion keys, for tests.
    #[serde(default = "default_iap_jwks_url")]
    pub iap_jwks_url: String,
}

fn default_google_jwks_url() -> String {
    "https://www.googleapis.com/oauth2/v3/certs".to_string()
}

fn default_iap_jwks_url() -> String {
    "https://www.gstatic.com/iap/verify/public_key-jwk".to_string()
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

        if self.environment.trim().is_empty() {
            return invalid("environment must not be blank");
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

        Ok(())
    }

    #[must_use]
    pub fn is_disabled(&self, operation: Operation) -> bool {
        self.disabled_operations.contains(&operation)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const ACCOUNT_ID: &str = "904837e3-3b76-47ec-b432-046db621571b";

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
}
