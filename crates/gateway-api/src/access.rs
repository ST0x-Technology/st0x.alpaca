//! Who may call the gateway, and which account a deployment serves.

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

/// The account a deployment is bound to. Chosen once at startup from config;
/// no request can name another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// `t0-alpaca`: the T0 hedging account used by st0x.liquidity.
    T0,
}

impl Profile {
    /// Deployment name used in audit records.
    #[must_use]
    pub const fn deployment(self) -> &'static str {
        match self {
            Self::T0 => "t0-alpaca",
        }
    }
}
