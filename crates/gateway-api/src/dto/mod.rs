//! Request and response bodies, one module per surface.
//!
//! Requests reject unknown fields, so no stray `accountId` (or any other field
//! the contract does not define) is ever silently accepted. Responses are
//! additive: clients ignore fields they do not know.

pub mod account;
pub mod market;
pub mod orders;
pub mod tokenization;
pub mod wallet;

use serde::{Deserialize, Deserializer, Serialize};
use st0x_alpaca::st0x_finance::Symbol;

/// Path parameter of every per symbol route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolPath {
    #[serde(deserialize_with = "symbol")]
    pub symbol: Symbol,
}

/// Reads a symbol a caller sent, path or body, refusing anything that could
/// steer the URL the library builds from it: only ASCII letters, digits,
/// `.`, `/` and `-`, at least one letter or digit, and no `/` separated part
/// that is empty, `.` or `..`. `BRK.B` and `BRK/B` pass; `../x`, `/AAPL`,
/// `A//B` and `AAPL?x` do not.
///
/// # Errors
///
/// A deserialization error naming the refused symbol.
pub fn symbol<'de, D>(deserializer: D) -> Result<Symbol, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    if !is_safe_symbol(&raw) {
        return Err(serde::de::Error::custom(format_args!(
            "symbol {raw:?} must be ASCII letters, digits, '.', '/' or '-', without empty or dot \
             parts"
        )));
    }
    Symbol::new(raw).map_err(serde::de::Error::custom)
}

fn is_safe_symbol(raw: &str) -> bool {
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-'))
        && raw.chars().any(|c| c.is_ascii_alphanumeric())
        && raw
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Sends a value over the wire and reads it back, as the gateway and its
/// client do.
#[cfg(test)]
fn through_wire<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn symbols_with_a_traversal_or_url_syntax_are_refused() {
        for accepted in ["AAPL", "BRK.B", "BRK/B", "BF-B", "USDC/USD"] {
            let path: SymbolPath = serde_json::from_value(json!({ "symbol": accepted })).unwrap();
            assert_eq!(path.symbol.as_str(), accepted);
        }

        for refused in [
            "",
            " ",
            " AAPL",
            "..",
            ".",
            "-",
            "../trading/accounts/x/account",
            "AAPL/..",
            "/AAPL",
            "AAPL/",
            "A//B",
            "./A",
            "AAPL?x=1",
            "AAPL#x",
            "AAPL%2F",
            "AAPL&x",
            "ÄAPL",
        ] {
            assert!(
                serde_json::from_value::<SymbolPath>(json!({ "symbol": refused })).is_err(),
                "{refused:?} was accepted"
            );
        }
    }
}
