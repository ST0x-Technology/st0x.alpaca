//! Alpaca Instant Tokenization Network wire-format constants.
//!
//! Redeem (and mint) callback `network` values are defined by Alpaca's Broker
//! API `OpenAPI` schema `TokenizationNetwork`:
//! <https://docs.alpaca.markets/reference/posttokenizationredeem>
//!
//! The issuer guide lists the same values in prose:
//! <https://docs.alpaca.markets/us/docs/tokenization-guide-for-issuer>

/// `OpenAPI` reference for `POST .../tokenization/callback/redeem`.
pub const REDEEM_CALLBACK_OPENAPI_REFERENCE: &str =
    "https://docs.alpaca.markets/reference/posttokenizationredeem";

/// `TokenizationNetwork` enum values from Alpaca Broker API `OpenAPI`
/// (`components.schemas.TokenizationNetwork.enum`).
///
/// `"hyperevm"` and `"robinhood"` are not in that published enum yet: both
/// networks are issued by st0x and their wire names are pending Alpaca's
/// confirmation. They are listed here, as in st0x.issuance, so every issued
/// network passes the preflight; drop them again if Alpaca publishes a
/// different spelling.
pub const TOKENIZATION_NETWORK_WIRE_STRINGS: &[&str] = &[
    "solana",
    "arbitrum",
    "ethereum",
    "binance",
    "base",
    "ton",
    "tron",
    "mantle",
    "hyperevm",
    "robinhood",
];

/// Returns whether `wire` is a published Alpaca ITN `TokenizationNetwork` value.
#[must_use]
pub fn accepts_network_wire_string(wire: &str) -> bool {
    TOKENIZATION_NETWORK_WIRE_STRINGS.contains(&wire)
}

#[cfg(test)]
mod tests {
    use super::{REDEEM_CALLBACK_OPENAPI_REFERENCE, accepts_network_wire_string};
    use crate::core::Network;

    #[test]
    fn issued_network_wire_strings_are_alpaca_itn_values() {
        for network in [
            Network::Base,
            Network::Ethereum,
            Network::HyperEvm,
            Network::Robinhood,
            Network::BnbSmartChain,
        ] {
            let wire = network.as_str();
            assert!(
                accepts_network_wire_string(wire),
                "issued network {wire} must be listed in Alpaca TokenizationNetwork \
                 OpenAPI ({REDEEM_CALLBACK_OPENAPI_REFERENCE})"
            );
        }
    }

    #[test]
    fn unpublished_network_wire_strings_are_rejected() {
        for wire in ["polygon", "bsc", "Base", "", "hyper-evm"] {
            assert!(!accepts_network_wire_string(wire), "{wire}");
        }
    }
}
