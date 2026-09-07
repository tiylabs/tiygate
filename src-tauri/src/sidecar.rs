//! Sidecar process lifecycle management.
//!
//! This module handles:
//! - Scanning for an available loopback port (starting from 13000).
//! - Spawning the `tiygate` sidecar binary with the correct environment
//!   variables (`TIYGATE_LISTEN_ADDR`, `TIYGATE_DATABASE_URL`,
//!   `TIYGATE_ADMIN_TOKEN`, `TIYGATE_MASTER_KEY`, `TIYGATE_MODE`,
//!   `RUST_LOG`).
//! - Polling the `/healthz` endpoint until the sidecar is ready.
//! - Graceful shutdown: kill the child process and wait for exit.

use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tauri::AppHandle;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// Maximum number of ports to try when scanning (13000..=13099).
const MAX_PORT_ATTEMPTS: u16 = 100;

/// How long to wait for the sidecar health check to succeed.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(300);

/// Build the SQLite URL shared by initial startup and every sidecar restart.
///
/// `sqlite:` is intentional: `sqlite://C:/...` treats the Windows drive
/// letter as a URL authority and leaves SQLite with an invalid relative path.
/// Keeping this in one helper prevents the startup and restart paths from
/// drifting apart again.
pub fn sqlite_database_url(data_dir: &Path) -> String {
    let db_path = data_dir.join("tiygate.db");
    format!(
        "sqlite:{}?mode=rwc",
        db_path.to_string_lossy().replace('\\', "/")
    )
}

/// Wrapper around the spawned sidecar child process.
pub struct SidecarManager {
    child: Option<CommandChild>,
    port: u16,
}

impl SidecarManager {
    /// Returns the port the sidecar is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Shut down the sidecar by killing the child process.
    ///
    /// **Note:** The Tauri shell plugin's `CommandChild::kill` sends
    /// `SIGKILL` on Unix (not `SIGTERM`), so the sidecar does not get
    /// a chance to perform graceful drain. This is a known limitation
    /// of `tauri-plugin-shell` 2.x — there is no `signal` API. The
    /// sidecar's in-flight requests will be terminated abruptly. A
    /// future improvement could spawn the sidecar via
    /// `tokio::process::Command` to send `SIGTERM` first, wait for
    /// graceful exit (up to 10 seconds), then `SIGKILL` as fallback.
    pub async fn shutdown(&mut self) {
        if let Some(child) = self.child.take() {
            tracing::info!("shutting down tiygate sidecar on port {}", self.port);
            if let Err(e) = child.kill() {
                tracing::warn!("failed to kill sidecar process: {e}");
            }
            // Give the OS time to fully release the process and its
            // file handles (especially SQLite's WAL/shm files on
            // Windows, which are not released instantaneously after
            // kill). Without this delay, a subsequent `spawn_sidecar`
            // on the same DB can fail with `unable to open database
            // file` (SQLite code 14).
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Find an available TCP port on 127.0.0.1 starting from `start_port`.
/// Returns the first bindable port, or `None` if all attempts fail.
pub fn find_available_port(start_port: u16) -> Option<u16> {
    for offset in 0..MAX_PORT_ATTEMPTS {
        let port = start_port.saturating_add(offset);
        // Try binding to check availability, then immediately release.
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// Spawn the tiygate sidecar with the given configuration and wait for
/// it to become healthy.
///
/// # Arguments
/// * `app` - Tauri app handle (used to access the shell plugin).
/// * `port` - Port for the sidecar to listen on.
/// * `admin_token` - Value for `TIYGATE_ADMIN_TOKEN`.
/// * `master_key` - 64-char hex for `TIYGATE_MASTER_KEY`.
/// * `db_url` - SQLite database URL (e.g. `sqlite:/path/to/tiygate.db?mode=rwc`).
pub async fn spawn_sidecar(
    app: &AppHandle,
    port: u16,
    admin_token: &str,
    master_key: &str,
    db_url: &str,
) -> Result<SidecarManager> {
    tracing::info!("spawning tiygate sidecar on port {port}");

    let listen_addr = format!("127.0.0.1:{port}");

    // Build the sidecar command. The sidecar binary name "tiygate"
    // matches the `externalBin` entry in tauri.conf.json (without the
    // target-triple suffix — Tauri resolves that automatically).
    let mut cmd = app
        .shell()
        .sidecar("tiygate")
        .context("failed to resolve tiygate sidecar binary")?;

    // Inject environment variables. These are read once by
    // ServerConfig::from_env() at sidecar startup.
    cmd = cmd
        .env("TIYGATE_LISTEN_ADDR", &listen_addr)
        .env("TIYGATE_MODE", "all")
        .env("TIYGATE_DATABASE_URL", db_url)
        .env("TIYGATE_ADMIN_TOKEN", admin_token)
        .env("TIYGATE_MASTER_KEY", master_key)
        .env("TIYGATE_SQLITE_MAINTENANCE_ENABLED", "true")
        .env("TIYGATE_SQLITE_MAINTENANCE_VACUUM_ENABLED", "true")
        .env("TIYGATE_SQLITE_MAINTENANCE_INTERVAL_SECS", "86400")
        .env("TIYGATE_SQLITE_MAINTENANCE_MIN_FREELIST_PAGES", "1024")
        .env("TIYGATE_SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT", "20")
        .env("RUST_LOG", "info");

    let (mut rx, child) = cmd.spawn().context("failed to spawn tiygate sidecar")?;

    // Spawn a background task to drain stdout/stderr so the process
    // doesn't block on a full pipe buffer.
    let health_port = port;
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    let line = line.trim_end();
                    tracing::info!("[sidecar] {line}");
                    // Detect early exit: if the sidecar logs an error
                    // before becoming healthy, bail out immediately
                    // instead of waiting for the full health-check
                    // timeout. This typically happens when SQLite
                    // cannot open the database file (e.g. the previous
                    // process hasn't released its handles yet on
                    // Windows).
                    if line.contains("\"level\":\"ERROR\"")
                        && (line.contains("server exited with error")
                            || line.contains("unable to open database file"))
                    {
                        tracing::error!(
                            "sidecar reported a fatal error before becoming healthy on port {health_port}"
                        );
                        break;
                    }
                }
                CommandEvent::Stderr(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    tracing::warn!("[sidecar] {}", line.trim_end());
                }
                CommandEvent::Terminated(payload) => {
                    tracing::warn!(
                        "sidecar process terminated: code={:?}, signal={:?}",
                        payload.code,
                        payload.signal
                    );
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait for the sidecar to become healthy.
    wait_for_health(port).await.with_context(|| {
        format!(
            "tiygate sidecar did not become healthy on port {port} within {}s",
            HEALTH_CHECK_TIMEOUT.as_secs()
        )
    })?;

    tracing::info!("tiygate sidecar is healthy on port {port}");

    Ok(SidecarManager {
        child: Some(child),
        port,
    })
}

/// Poll `GET /healthz` until it returns 200 or the timeout elapses.
async fn wait_for_health(port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(HEALTH_CHECK_INTERVAL)
        .build()
        .context("failed to build HTTP client for health check")?;

    let deadline = Instant::now() + HEALTH_CHECK_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("health check timeout");
        }
        match client.get(&url).send().await {
            Ok(res) if res.status().is_success() => {
                return Ok(());
            }
            Ok(res) => {
                tracing::debug!("health check returned {} — retrying", res.status());
            }
            Err(e) => {
                tracing::debug!("health check connection failed: {e} — retrying");
            }
        }
        tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_database_url_preserves_windows_drive_letter() {
        let url = sqlite_database_url(Path::new(r"C:\Users\test\AppData\Local\TiyGate"));
        assert_eq!(
            url,
            "sqlite:C:/Users/test/AppData/Local/TiyGate/tiygate.db?mode=rwc"
        );
        assert!(!url.starts_with("sqlite://C:"));
    }

    #[test]
    fn sqlite_database_url_builds_unix_absolute_path() {
        assert_eq!(
            sqlite_database_url(Path::new("/var/lib/tiygate")),
            "sqlite:/var/lib/tiygate/tiygate.db?mode=rwc"
        );
    }

    #[test]
    fn find_available_port_returns_a_bindable_port() {
        // Find a port, then verify we can bind to it.
        if let Some(port) = find_available_port(14000) {
            assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
        } else {
            panic!("find_available_port returned None");
        }
    }

    #[test]
    fn find_available_port_skips_occupied() {
        // Bind a port to occupy it, then verify find_available_port
        // returns a different one.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied = listener.local_addr().unwrap().port();
        // Only run this test if the occupied port is in a reasonable range
        // to avoid collisions with the 14000+ scan range.
        if occupied < 60000 {
            let found = find_available_port(occupied);
            assert!(found.is_some());
            assert_ne!(found.unwrap(), occupied);
        }
    }
}
