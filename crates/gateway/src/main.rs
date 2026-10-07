//! `st0x-alpaca-gateway`: reads its config path from
//! `ST0X_ALPACA_GATEWAY_CONFIG` and serves until SIGTERM or Ctrl-C.
//!
//! `st0x-alpaca-gateway --validate-config <path>` only parses and validates a
//! config file and exits; the release workflow runs it inside the image
//! before publishing a config.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use st0x_alpaca_gateway::audit::StdoutSink;
use st0x_alpaca_gateway::config::GatewayConfig;
use tracing::error;
use tracing_subscriber::EnvFilter;

const CONFIG_ENV: &str = "ST0X_ALPACA_GATEWAY_CONFIG";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !args.is_empty() {
        if let [flag, path] = args.as_slice()
            && flag == "--validate-config"
        {
            return validate(&PathBuf::from(path));
        }
        error!("usage: st0x-alpaca-gateway [--validate-config <path>]");
        return ExitCode::FAILURE;
    }

    let Some(path) = std::env::var_os(CONFIG_ENV).map(PathBuf::from) else {
        error!("{CONFIG_ENV} is not set");
        return ExitCode::FAILURE;
    };

    let config = match GatewayConfig::load(&path) {
        Ok(config) => config,
        Err(load_error) => {
            error!(%load_error, "Refusing to start");
            return ExitCode::FAILURE;
        }
    };

    match st0x_alpaca_gateway::serve(config, Arc::new(StdoutSink), shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(serve_error) => {
            error!(%serve_error, "Gateway stopped");
            ExitCode::FAILURE
        }
    }
}

fn validate(path: &std::path::Path) -> ExitCode {
    match GatewayConfig::load(path) {
        Ok(config) => {
            tracing::info!(
                environment = %config.environment,
                account_id = %config.broker.account_id,
                "Config is valid"
            );
            ExitCode::SUCCESS
        }
        Err(load_error) => {
            error!(%load_error, "Config is invalid");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
