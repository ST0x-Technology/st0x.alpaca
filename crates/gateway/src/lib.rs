//! The account bound Alpaca gateway (`t0-alpaca`).
//!
//! Holds the deployment's Alpaca credential and serves the operation catalog
//! of `st0x-alpaca-gateway-api` to bots (Google ID tokens) and operators
//! (IAP). Every operation runs one bounded `st0x-alpaca` method against the
//! account fixed in config; no request can name an account or an Alpaca URL.
//! The gateway signs nothing onchain and keeps no state.

pub mod answer;
pub mod audit;
pub mod auth;
pub mod budget;
pub mod config;
pub mod extract;
mod handlers;
pub mod routes;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::audit::AuditSink;
use crate::config::GatewayConfig;
use crate::routes::Verifiers;
use crate::state::AppState;

/// How long shutdown waits for detached mutations after the listener stops.
/// Below Cloud Run's default ten second termination grace.
const DRAIN_GRACE: Duration = Duration::from_secs(8);

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Startup(#[from] state::StartupError),
    #[error("identity verifier HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("listener: {0}")]
    Io(#[from] std::io::Error),
}

/// Checks the account, then serves until `shutdown` resolves, then waits for
/// detached mutations within the grace period.
///
/// # Errors
///
/// Returns [`ServeError`] when the startup account check fails or the
/// listener cannot bind.
pub async fn serve(
    config: GatewayConfig,
    audit: Arc<dyn AuditSink>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let listen = config.listen;
    let state = AppState::connect(config, audit, env!("CARGO_PKG_VERSION").to_string()).await?;
    let verifiers = Verifiers::from_state(&state)?;
    let app = routes::app(state.clone(), &verifiers);

    let listener = TcpListener::bind(listen).await?;
    info!(address = %listener.local_addr()?, "Gateway listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    state.tasks.close();
    if tokio::time::timeout(DRAIN_GRACE, state.tasks.wait())
        .await
        .is_err()
    {
        warn!(
            pending = state.tasks.len(),
            "Shutdown grace ended with mutations still running; callers reconcile from reads"
        );
    }
    Ok(())
}
