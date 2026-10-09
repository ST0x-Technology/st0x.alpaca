//! Wire contract of the account bound Alpaca gateway (`t0-alpaca`,
//! `s01-alpaca`).
//!
//! The gateway runs `st0x-alpaca` methods on behalf of bots and operators so
//! that only the gateway holds Alpaca credentials. This crate is the contract
//! both sides compile against: the tiers, the operation catalog with its
//! capability matrix, the request and response bodies, the error body, and
//! the audit event. The `client` feature adds a typed HTTP client.

pub mod access;
#[cfg(feature = "client")]
pub mod client;
pub mod dto;
pub mod failure;
pub mod ops;
pub mod record;

pub use access::{Profile, Tier};
pub use failure::{ErrorBody, ErrorCode, Outcome, RejectionReason};
pub use ops::{Method, Operation};
pub use record::{AUDIT_TARGET, AuditEvent, AuditPhase};

/// Header a bot sets to the human it acts for. Audit only.
pub const ON_BEHALF_OF_HEADER: &str = "x-on-behalf-of";

/// Header carrying the gateway's request id on every answer.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
