//! Shared Alpaca client library for st0x services.
//!
//! Both st0x bots integrate with Alpaca; this crate is the single home for
//! that integration so client code stops being copy-pasted across repos.
//! Each consumer enables only the surfaces it needs via feature flags
//! (purely additive):
//!
//! - `issuer` — the ITN issuer-callback surface (st0x.issuance): mint
//!   callback, redeem initiation, tokenization-request polling.
//! - `broker` — Broker API trading: orders, positions, journals, account,
//!   market clock (st0x.liquidity).
//! - `wallet` — crypto funding wallets, transfers, whitelists
//!   (st0x.liquidity).
//! - `market-data` — quotes and snapshots (st0x.liquidity).
//!
//! The always-on [`core`] module holds the shared transport: the HTTP client
//! builder, Alpaca's dual authentication (HTTP Basic + `APCA-API-KEY`
//! headers), base-URL/account configuration, retry classification, and the
//! error taxonomy.
//!
//! The crate is telemetry-free by design: consumers wrap calls with their own
//! instrumentation. Wire types are neutral (strings, decimals, addresses);
//! consumers convert to their domain newtypes at the boundary.

pub mod core;

#[cfg(feature = "broker")]
pub mod broker;
#[cfg(feature = "issuer")]
pub mod issuer;
#[cfg(feature = "market-data")]
pub mod market_data;
#[cfg(feature = "wallet")]
pub mod wallet;

pub use core::{AlpacaAuth, AlpacaClient, AlpacaError};
