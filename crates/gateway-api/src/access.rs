//! Who may call the gateway, and which deployment it is.

use serde::{Deserialize, Serialize};

/// The caller class a request arrives as. The path prefix decides it, and the
/// gateway verifies the matching identity on every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A bot runtime service account, authenticated with a Google ID token.
    Bot,
    /// A human reader, authenticated by IAP.
    Read,
    /// A human writer, authenticated by IAP.
    Write,
}

impl Tier {
    pub const ALL: [Self; 3] = [Self::Bot, Self::Read, Self::Write];

    /// Path prefix the tier is served under.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Bot => "/bot/v1",
            Self::Read => "/alpaca-read/v1",
            Self::Write => "/alpaca-write/v1",
        }
    }

    #[must_use]
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Read | Self::Write)
    }
}

/// Deployment name used in audit records.
pub const DEPLOYMENT: &str = "t0-alpaca";
