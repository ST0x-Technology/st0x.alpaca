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
use tokio::time::Instant;
use tracing::{info, warn};

use crate::audit::AuditSink;
use crate::config::GatewayConfig;
use crate::routes::Verifiers;
use crate::state::AppState;

/// The whole shutdown budget, measured from the signal: the connection
/// drain and the wait for detached mutations share it. Below Cloud Run's
/// default ten second termination grace.
const DRAIN_GRACE: Duration = Duration::from_secs(8);

/// The commit the binary was built from, which the flake passes at compile
/// time.
const BUILD_REV: Option<&str> = option_env!("ST0X_ALPACA_GATEWAY_REV");

/// The Cloud Run revision serving the process, set by Cloud Run.
const CLOUD_RUN_REVISION: &str = "K_REVISION";

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Startup(#[from] state::StartupError),
    #[error("identity verifier HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("listener: {0}")]
    Io(#[from] std::io::Error),
}

/// Checks the account, binds the configured address, then [`run`]s.
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
    let state = AppState::connect(config, audit, version()).await?;
    let listener = TcpListener::bind(listen).await?;
    run(state, listener, shutdown).await
}

/// The version every audit record and `/readyz` carry: the package version,
/// the commit the binary was built from (`unknown` outside the flake build)
/// and, under Cloud Run, the revision serving it, as
/// `0.1.0+<commit>@<revision>`.
fn version() -> String {
    let package = env!("CARGO_PKG_VERSION");
    let commit = BUILD_REV.filter(|rev| !rev.is_empty()).unwrap_or("unknown");
    match std::env::var(CLOUD_RUN_REVISION) {
        Ok(revision) if !revision.is_empty() => format!("{package}+{commit}@{revision}"),
        _ => format!("{package}+{commit}"),
    }
}

/// Serves `state` on `listener` until `shutdown` resolves. At the signal
/// every request in flight answers at once (a mutation `outcome_unknown`),
/// the connections drain, and detached mutations get what is left of
/// [`DRAIN_GRACE`] to finish.
///
/// # Errors
///
/// Returns [`ServeError`] when the identity HTTP client cannot be built or
/// the listener fails.
pub async fn run(
    state: AppState,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let verifiers = Verifiers::from_state(&state)?;
    let app = routes::app(state.clone(), &verifiers);
    info!(address = %listener.local_addr()?, "Gateway listening");

    let signal = state.shutdown.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let signal = signal.clone();
        async move {
            shutdown.await;
            signal.cancel();
        }
    });
    let mut serving = std::pin::pin!(server.into_future());

    // The server only ends on its own after the signal, so the signal
    // branch wins whenever both are ready.
    let drained = tokio::select! {
        biased;
        () = signal.cancelled() => false,
        outcome = &mut serving => {
            outcome?;
            true
        }
    };
    let budget_end = Instant::now() + DRAIN_GRACE;

    if !drained {
        if let Ok(outcome) = tokio::time::timeout_at(budget_end, &mut serving).await {
            outcome?;
        } else {
            warn!("Shutdown grace ended with connections still open");
        }
    }

    state.tasks.close();
    if tokio::time::timeout_at(budget_end, state.tasks.wait())
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
