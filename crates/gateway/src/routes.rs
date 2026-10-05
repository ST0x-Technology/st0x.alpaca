//! The router: one prefix per tier, each operation mounted only on the tiers
//! the profile's capability matrix allows, behind that tier's identity check.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router, middleware};
use serde_json::json;
use st0x_alpaca_gateway_api::{ErrorCode, Operation, Tier};

use crate::answer::Failure;
use crate::auth::{self, TokenVerifier};
use crate::handlers;
use crate::state::AppState;

/// Identity verifiers, one per tier.
pub struct Verifiers {
    pub bot: Arc<TokenVerifier>,
    pub read: Arc<TokenVerifier>,
    pub write: Arc<TokenVerifier>,
}

impl Verifiers {
    /// Builds the verifiers from config, sharing one HTTP client with
    /// timeouts for the key fetches.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client build error.
    pub fn from_state(state: &AppState) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let identity = &state.config.identity;
        Ok(Self {
            bot: Arc::new(TokenVerifier::bot(
                &identity.bot_audience,
                &identity.bot_principals,
                identity.google_jwks_url.clone(),
                http.clone(),
            )),
            read: Arc::new(TokenVerifier::iap(
                Tier::Read,
                &identity.read_audience,
                identity.iap_jwks_url.clone(),
                http.clone(),
            )),
            write: Arc::new(TokenVerifier::iap(
                Tier::Write,
                &identity.write_audience,
                identity.iap_jwks_url.clone(),
                http,
            )),
        })
    }

    fn for_tier(&self, tier: Tier) -> Arc<TokenVerifier> {
        match tier {
            Tier::Bot => Arc::clone(&self.bot),
            Tier::Read => Arc::clone(&self.read),
            Tier::Write => Arc::clone(&self.write),
        }
    }
}

/// The whole service: health routes plus every tier's operations.
pub fn app(state: AppState, verifiers: &Verifiers) -> Router {
    let profile = state.config.profile;
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz));

    for tier in Tier::ALL {
        let mut tier_router = Router::new();
        for operation in Operation::ALL {
            if !operation.allows(profile, tier) {
                continue;
            }
            let Some(handler) = handlers::route(operation) else {
                continue;
            };
            tier_router = tier_router.route(
                &format!("{}{}", tier.prefix(), operation.path()),
                handler.layer(Extension(operation)),
            );
        }
        router = router.merge(tier_router.route_layer(middleware::from_fn_with_state(
            verifiers.for_tier(tier),
            auth::require,
        )));
    }

    router
        .fallback(unknown_operation)
        .method_not_allowed_fallback(unknown_operation)
        .with_state(state)
}

async fn healthz() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

/// Serving at all means the startup account check passed: the state is only
/// built after it.
async fn readyz(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ready",
        "profile": state.config.profile,
        "environment": state.config.environment,
        "version": state.version,
    }))
    .into_response()
}

async fn unknown_operation() -> Response {
    Failure::new(
        ErrorCode::UnknownOperation,
        "no such operation for this deployment and tier",
    )
    .into_response()
}
