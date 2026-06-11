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
//!
//! ## Subcommands (§8 stage 4)
//!
//! The binary now accepts `run` (default) / `migrate` / `migrate-status`
//! clap subcommands. `migrate` runs the schema migrations against the
//! configured database and exits; `migrate-status` reports the applied
//! versions. `run` is the legacy path.

mod app;
mod cli;
mod config;
mod drain;
mod ingress;
mod ingress_phase4;
mod telemetry;
mod trace;
#[cfg(feature = "webui")]
mod webui;

// Ensure provider-bedrock is linked for Executor discovery (only when the
// `bedrock` feature is enabled, otherwise the crate is not even compiled in).
#[cfg(feature = "bedrock")]
use tiygate_provider_bedrock as _;

use std::process::ExitCode;
use std::time::Duration;

#[cfg(feature = "tracing")]
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn init_tracing() {
    #[cfg(feature = "tracing")]
    {
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    }
}

fn load_dotenv() {
    #[cfg(feature = "dotenv")]
    let _ = dotenvy::dotenv();
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    load_dotenv();

    tracing::info!("TiyGate AI Gateway v{}", env!("CARGO_PKG_VERSION"));

    let args = cli::Args::parse_or_exit();
    let command = args
        .command
        .unwrap_or(cli::Command::Run(cli::RunArgs::default()));
    match command {
        cli::Command::Run(run_args) => match run(run_args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "server exited with error");
                ExitCode::FAILURE
            }
        },
        cli::Command::Migrate => match run_migrate().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "migrate failed");
                ExitCode::FAILURE
            }
        },
        cli::Command::MigrateStatus => match run_migrate_status().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "migrate status failed");
                ExitCode::FAILURE
            }
        },
    }
}

async fn run_migrate() -> anyhow::Result<()> {
    let cfg = config::ServerConfig::from_env();
    let database_url = cfg
        .database_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_DATABASE_URL is required for `migrate`"))?;
    let pool = tiygate_store::db::open_pool(&database_url).await?;
    tiygate_store::db::run_migrations(pool.sqlite()).await?;
    println!("migrations applied to {database_url}");
    Ok(())
}

async fn run_migrate_status() -> anyhow::Result<()> {
    let cfg = config::ServerConfig::from_env();
    let database_url = cfg
        .database_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_DATABASE_URL is required for `migrate status`"))?;
    let pool = tiygate_store::db::open_pool(&database_url).await?;
    let rows = tiygate_store::db::list_applied(pool.sqlite()).await?;
    if rows.is_empty() {
        println!("(no migrations applied yet)");
    } else {
        let header = format!("{:<10}  {:<22}  {}", "sequence", "version", "applied_at");
        println!("{header}");
        for (seq, version, applied_at) in rows {
            println!("{:<10}  {:<22}  {}", seq, version, applied_at);
        }
    }
    Ok(())
}

async fn run(_args: cli::RunArgs) -> anyhow::Result<()> {
    let app = app::App::new().await?;
    let server_config = config::ServerConfig::from_env();
    tracing::info!(
        "Starting in {:?} mode on {} (control_plane={})",
        server_config.mode,
        server_config.listen_addr,
        app.control_plane().is_some(),
    );

    let listener = tokio::net::TcpListener::bind(&server_config.listen_addr).await?;
    tracing::info!("Listening on {}", server_config.listen_addr);

    let drain_state = drain::DrainState::new(Duration::from_secs(server_config.drain_timeout_secs));
    let drain_signal = drain_state.clone();
    drain::spawn_signal_listener(drain_signal.clone());
    let drain_signal_for_global = drain_signal.clone();
    tokio::spawn(async move {
        drain_signal_for_global.wait_for_signal().await;
        drain::set_global_drain_signalled();
    });

    let app_router = app.router();
    let drain_for_server = drain_state.clone();
    let server = axum::serve(listener, app_router).with_graceful_shutdown(async move {
        drain_for_server.wait_for_signal().await;
        tracing::info!("graceful shutdown: in-flight requests will be allowed to finish");
    });

    if let Err(e) = server.await {
        tracing::error!(error = %e, "server exited with error");
        return Err(e.into());
    }

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
