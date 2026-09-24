//! Alpaca corporate-action stream client used by st0x.issuance.
//!
//! Covers the Alpaca side of the `v1beta1/events/corporate-actions` SSE
//! stream: endpoint validation ([`CorporateActionStreamEndpoint`]), the
//! authenticated request with its replay position
//! ([`CorporateActionStreamClient::connect`], [`CorporateActionReplay`]),
//! the bounded SSE decoder ([`CorporateActionSseDecoder`]), and the wire
//! identities it produces. Cursor persistence, the projection, reconnect
//! policy, and alerting stay in the consumer. The surface emits no log
//! events.

mod client;
mod endpoint;
mod event;
mod replay;
mod sse;

pub use client::{
    CorporateActionStream, CorporateActionStreamBuildError, CorporateActionStreamClient,
    CorporateActionStreamError,
};
pub use endpoint::{
    CorporateActionEndpointError, CorporateActionStreamEndpoint, CorporateActionStreamTransport,
    DEFAULT_CORPORATE_ACTIONS_STREAM_URL, DevelopmentLoopback,
};
pub use event::{
    CorporateActionEventId, CorporateActionId, CorporateActionMutation,
    CorporateActionMutationKind, CorporateActionSymbol, DividendCorporateAction,
};
pub use replay::{
    CorporateActionBootstrapSince, CorporateActionBootstrapSinceError, CorporateActionReplay,
    CorporateActionReplayUntil,
};
pub use sse::{
    CorporateActionDecodeBatch, CorporateActionDecodeError, CorporateActionSseDecoder,
    CorporateActionStreamDecodeError,
};
