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

use serde::{Deserialize, Serialize};

/// Path parameter of every per symbol route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolPath {
    pub symbol: st0x_alpaca::st0x_finance::Symbol,
}
