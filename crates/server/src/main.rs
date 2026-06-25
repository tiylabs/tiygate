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
//! ## Subcommands (§8)
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
mod models;
mod oauth_manager;
mod telemetry;
mod trace;
#[cfg(feature = "webui")]
mod webui;

// Ensure provider-bedrock is linked for Executor discovery (only when the
// `bedrock` feature is enabled, otherwise the crate is not even compiled in).
#[cfg(feature = "bedrock")]
use tiygate_provider_bedrock as _;

use bytes::Bytes;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;
use tiygate_store::archive::PayloadArchiveClient;

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
        cli::Command::ArchiveCheck(args) => match run_archive_check(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %e, "archive check failed");
                ExitCode::FAILURE
            }
        },
    }
}

async fn run_archive_check(args: cli::ArchiveCheckArgs) -> anyhow::Result<()> {
    let archive_cfg = config::PayloadArchiveConfig::from_env();
    if !archive_cfg.is_complete() {
        return Err(anyhow::anyhow!(
            "TIYGATE_PAYLOAD_ARCHIVE_* configuration is incomplete"
        ));
    }
    let endpoint = archive_cfg
        .s3_endpoint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_PAYLOAD_ARCHIVE_S3_ENDPOINT is required"))?;
    let bucket = archive_cfg
        .s3_bucket
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_PAYLOAD_ARCHIVE_S3_BUCKET is required"))?;
    let access_key_id = archive_cfg
        .s3_access_key_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_PAYLOAD_ARCHIVE_S3_ACCESS_KEY_ID is required"))?;
    let secret_access_key = archive_cfg.s3_secret_access_key.clone().ok_or_else(|| {
        anyhow::anyhow!("TIYGATE_PAYLOAD_ARCHIVE_S3_SECRET_ACCESS_KEY is required")
    })?;
    let client = tiygate_store::archive::S3ArchiveClient::new(
        endpoint,
        archive_cfg.s3_region.clone(),
        bucket.clone(),
        tiygate_store::archive::normalize_prefix(&archive_cfg.s3_prefix),
        archive_cfg.s3_force_path_style,
        access_key_id,
        secret_access_key,
        archive_cfg.timeout_secs,
    )?;

    let key = if archive_cfg.s3_prefix.trim().is_empty() {
        args.key.trim_start_matches('/').to_string()
    } else {
        format!(
            "{}/{}",
            tiygate_store::archive::normalize_prefix(&archive_cfg.s3_prefix),
            args.key.trim_start_matches('/')
        )
    };
    let body = Bytes::from(args.content.into_bytes());
    client
        .put_object(
            &key,
            body.clone(),
            "text/plain; charset=utf-8",
            "identity",
            vec![],
        )
        .await
        .map_err(|e| anyhow::anyhow!("PUT {key} failed: {e}"))?;
    let roundtrip = client
        .get_object(&key)
        .await
        .map_err(|e| anyhow::anyhow!("GET {key} failed: {e}"))?;
    if roundtrip != body {
        return Err(anyhow::anyhow!("round-trip content mismatch for {key}"));
    }
    println!("archive connectivity OK: s3://{bucket}/{key}");
    Ok(())
}

async fn run_migrate() -> anyhow::Result<()> {
    let cfg = config::ServerConfig::from_env();
    let database_url = cfg
        .database_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIYGATE_DATABASE_URL is required for `migrate`"))?;
    let pool = tiygate_store::db::open_pool(&database_url).await?;
    tiygate_store::db::run_migrations(&pool).await?;
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
    let rows = tiygate_store::db::list_applied(&pool).await?;
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
    // `into_make_service_with_connect_info` makes
    // `ConnectInfo<SocketAddr>` available in request extensions,
    // which the admin brute-force middleware uses to extract the
    // client IP when `X-Forwarded-For` is absent.
    let server = axum::serve(
        listener,
        app_router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        drain_for_server.wait_for_signal().await;
        tracing::info!("graceful shutdown: in-flight requests will be allowed to finish");
    });

    // `with_graceful_shutdown` makes `server` resolve only after the drain
    // signal fires *and* all in-flight requests have finished. That means
    // when there is no live traffic the server returns almost immediately
    // — no fixed wait. The `drain_timeout` is only a safety bound for the
    // case where some request is stuck; we start counting it from the
    // moment the drain signal arrives, not from process start.
    let drain_deadline = drain_state.clone();
    let timeout_guard = async move {
        drain_deadline.wait_for_signal().await;
        tokio::time::sleep(Duration::from_secs(server_config.drain_timeout_secs)).await;
    };

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!(error = %e, "server exited with error");
                return Err(e.into());
            }
            tracing::info!("drain complete: all in-flight requests finished");
        }
        _ = timeout_guard => {
            tracing::warn!(
                "drain timeout ({}s) elapsed with requests still in flight — forcing shutdown",
                server_config.drain_timeout_secs
            );
        }
    }

    tracing::info!("TiyGate shutdown complete");
    Ok(())
}
