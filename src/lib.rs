//! Shared Alpaca client library for st0x services.
//!
//! Each surface is an additive Cargo feature:
//!
//! - `issuer`: the ITN issuer-callback surface used by st0x.issuance (mint
//!   callback, redeem initiation with the ITN network preflight, keyed
//!   tokenization-request polling).
//! - `broker`: the Broker API and Market Data API surface used by
//!   st0x.liquidity (account, assets, equity orders, USD/USDC conversion,
//!   positions, journals, account activities, market hours, quotes).
//! - `wallet`: crypto wallet deposits, withdrawals, transfers, and
//!   whitelists.
//! - `tokenization`: the liquidity-side tokenization client (mint requests,
//!   request history, redemption detection, polling).
//! - `mock`: stateful httpmock broker, wallet, and tokenization servers for
//!   consumer end-to-end suites.
//!
//! The always-on [`core`] module holds the authentication modes and the
//! backpressure/permanence classification shared by every error type. Every
//! credential-bearing URL is validated (HTTPS, or HTTP on loopback only) and
//! every client refuses redirects. See `docs/parity.md` for the parity matrix
//! against the consumer implementations this crate replaces.

#[cfg(any(feature = "issuer", feature = "broker"))]
mod auth;
pub mod core;
#[cfg(any(feature = "issuer", feature = "broker"))]
mod endpoint;
#[cfg(any(feature = "issuer", feature = "broker"))]
mod rate_limit;

#[cfg(feature = "broker")]
pub mod broker;
#[cfg(feature = "issuer")]
pub mod issuer;
#[cfg(feature = "tokenization")]
pub mod tokenization;
#[cfg(feature = "mock")]
pub mod tokenization_mock;
#[cfg(feature = "wallet")]
pub mod wallet;

#[cfg(any(feature = "issuer", feature = "broker"))]
pub use auth::{ALPACA_SANDBOX_TOKEN_URL, ALPACA_TOKEN_URL, KmsJwtError};
pub use core::{AlpacaAuth, Backpressure, Permanence};
#[cfg(feature = "issuer")]
pub use core::{AlpacaClient, AlpacaError};
#[cfg(any(feature = "issuer", feature = "broker"))]
pub use endpoint::{EndpointError, EndpointRole};
/// The `st0x-finance` release every public amount, quantity, and symbol type
/// comes from; consumers must use the same version.
#[cfg(any(feature = "issuer", feature = "broker"))]
pub use st0x_finance;
