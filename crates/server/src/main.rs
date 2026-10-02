use std::process::ExitCode;

use clap::Parser;
use mw_server::{Config, Server};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::parse();
    let server = match Server::new(config).bind().await {
        Ok(server) => server,
        Err(err) => {
            error!(%err, "could not bind");
            return ExitCode::FAILURE;
        }
    };
    if let (Ok(tcp), Ok(udp)) = (server.tcp_addr(), server.udp_addr()) {
        info!(%tcp, %udp, "listening");
    }

    match server.run(shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "server failed");
            ExitCode::FAILURE
        }
    }
}

/// Ctrl+C everywhere, plus SIGTERM on Unix (docker stop, systemd).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown requested");
}
