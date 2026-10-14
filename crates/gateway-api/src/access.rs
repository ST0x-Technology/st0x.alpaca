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

    /// The tier's name in config and on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    #[must_use]
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Read | Self::Write)
    }
}

/// Which account the gateway serves. It picks the capability matrix, see
/// [`crate::Operation::tiers`], and names the account in audit records. One
/// profile can run as more than one deployment (`t0-liqbot` and `t0-humans`
/// both run `t0`); the audit record's `tier` tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// The T0 trading account.
    T0,
    /// The S01 issuer account.
    S01,
}

impl Profile {
    /// The account's name in the audit record's `deployment` field. It is
    /// kept stable for the audit stream, so it stays `t0-alpaca` although
    /// T0 runs as `t0-liqbot` and `t0-humans`.
    #[must_use]
    pub const fn deployment(self) -> &'static str {
        match self {
            Self::T0 => "t0-alpaca",
            Self::S01 => "s01-alpaca",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_names_are_their_wire_names() {
        for tier in Tier::ALL {
            assert_eq!(
                serde_json::to_value(tier).unwrap(),
                serde_json::json!(tier.as_str())
            );
        }
    }
}
