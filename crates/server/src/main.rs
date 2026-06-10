//! TiyGate Server — Assembles the gateway and starts the HTTP server.
//!
//! Supports deployment modes: `all` (single-process), `proxy` (data plane only),
//! and `admin` (control plane only).
//!
//! ## Graceful drain (§3.8)
//!
//! On `SIGTERM` / `SIGINT` (or programmatic trigger) the server enters
//! `draining` state, `/readyz` flips to 503 so load balancers remove the
//! pod, and `axum::serve(...).with_graceful_shutdown(...)` lets in-flight
//! requests finish. Once the drain task completes (or the configurable
//! `drain_timeout` elapses) the process exits.

mod app;
mod config;
mod drain;
mod ingress;
mod telemetry;

// Ensure provider-bedrock is linked for Executor discovery (only when the
// `bedrock` feature is enabled, otherwise the crate is not even compiled in).
#[cfg(feature = "bedrock")]
use tiygate_provider_bedrock as _;

use std::time::Duration;

#[cfg(feature = "tracing")]
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() {
    // Initialize tracing (no-op when the `tracing` feature is disabled).
    #[cfg(feature = "tracing")]
    {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    }

    tracing::info!("TiyGate AI Gateway v{}", env!("CARGO_PKG_VERSION"));

    // Load .env if present (no-op when the `dotenv` feature is disabled).
    #[cfg(feature = "dotenv")]
    let _ = dotenvy::dotenv();

    // Run the rest of startup, mapping any error to the appropriate
    // return type for the enabled feature set.
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = run().await;

    if let Err(e) = result {
        tracing::error!(error = %e, "server exited with error");
        #[cfg(feature = "anyhow")]
        {
            // Re-raise so the process exits with a non-zero status when
            // `anyhow` is in scope. We swallow the error otherwise because
            // we cannot construct an `anyhow::Error` without the dep.
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Build the application
    let app = app::App::new().await?;

    // Build the server config
    let server_config = config::ServerConfig::from_env();
    tracing::info!(
        "Starting in {:?} mode on {}",
        server_config.mode,
        server_config.listen_addr
    );

    // Start the HTTP server
    let listener = tokio::net::TcpListener::bind(&server_config.listen_addr).await?;
    tracing::info!("Listening on {}", server_config.listen_addr);

    // Shared draining flag — `/readyz` polls this; the graceful-shutdown
    // future also waits on it so the two are synchronised.
    let drain_state = drain::DrainState::new(Duration::from_secs(server_config.drain_timeout_secs));
    let drain_signal = drain_state.clone();

    // Spawn a task that listens for SIGTERM/SIGINT and flips the state.
    drain::spawn_signal_listener(drain_signal.clone());
    // Also forward the per-instance signal into the process-global flag
    // used by `/readyz`.
    let drain_signal_for_global = drain_signal.clone();
    tokio::spawn(async move {
        drain_signal_for_global.wait_for_signal().await;
        drain::set_global_drain_signalled();
    });

    let app_router = app.router();
    let drain_for_server = drain_state.clone();
    let server = axum::serve(listener, app_router).with_graceful_shutdown(async move {
        // Wait until the drain state is signalled.
        drain_for_server.wait_for_signal().await;
        tracing::info!("graceful shutdown: in-flight requests will be allowed to finish");
    });

    // Bind a /readyz handler that returns 503 once draining starts.
    // (The router inside `app.router()` already wires up `/readyz`; we
    // additionally broadcast the drain signal to background tasks here.)

    if let Err(e) = server.await {
        tracing::error!(error = %e, "server exited with error");
        return Err(Box::new(e));
    }

    // After graceful_shutdown completes, run the bounded drain:
    // - wait up to drain_timeout for telemetry channel to flush
    // - exit
    tracing::info!(
        "waiting for drain to complete (timeout = {}s)",
        server_config.drain_timeout_secs
    );
    let _ = tokio::time::timeout(
        Duration::from_secs(server_config.drain_timeout_secs),
        drain_state.wait_for_drain_complete(),
    )
    .await;

    tracing::info!("TiyGate shutdown complete");
    Ok(())
}
