//! Shared Alpaca client library for st0x services.
//!
//! Shared Alpaca transport and the validated ITN issuer-callback surface.
//! The liquidity broker, wallet, and market-data surfaces are not part of
//! this release; they require a separate parity review before extraction.
//!
//! - `issuer` — the ITN issuer-callback surface (st0x.issuance): mint
//!   callback, redeem initiation, tokenization-request polling.
//!
//! The always-on [`core`] module holds the shared transport: the HTTP client
//! builder, Alpaca's dual authentication (HTTP Basic + `APCA-API-KEY`
//! headers), base-URL/account configuration, retry classification, and the
//! error taxonomy.
//!
//! The crate is telemetry-free by design: consumers wrap calls with their own
//! instrumentation. Wire types are neutral (strings, decimals, addresses);
//! consumers convert to their domain newtypes at the boundary.

mod auth;
pub mod core;
mod rate_limit;

#[cfg(feature = "issuer")]
pub mod issuer;

pub use auth::{ALPACA_SANDBOX_TOKEN_URL, ALPACA_TOKEN_URL, AuthRuntime, KmsJwtAuth, KmsJwtError};
pub use core::{AlpacaAuth, AlpacaClient, AlpacaError, Backpressure, Permanence};
pub use rate_limit::retry_after_from_response_headers;
