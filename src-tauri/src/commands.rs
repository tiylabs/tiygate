//! Tauri commands exposed to the frontend.
//!
//! These commands let the Setup wizard and AuthContext interact with
//! the local client configuration and sidecar process:
//!
//! - `is_first_run` — whether the setup wizard should be shown.
//! - `get_admin_token` — retrieve the stored token (for auto-login).
//! - `set_admin_token` — set a user-chosen token and restart the sidecar.
//! - `enable_passwordless` — keep the auto-generated token and mark
//!   setup as done.
//! - `get_server_port` — the port the sidecar is listening on.

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::config::InstanceEntry;
use crate::sidecar;
use crate::AppState;

/// Returns `true` when the setup wizard has not been completed yet.
/// On lock failure returns `true` (conservative: show setup).
#[tauri::command]
pub fn is_first_run(state: State<'_, AppState>) -> bool {
    match state.config.lock() {
        Ok(cfg) => !cfg.first_run_completed,
        Err(_) => true,
    }
}

/// Returns the stored admin token so the frontend can auto-login
/// without showing the Login page. Returns `None` when no token is
/// configured or the lock is poisoned.
#[tauri::command]
pub fn get_admin_token(state: State<'_, AppState>) -> Option<String> {
    state.config.lock().ok().map(|cfg| cfg.admin_token.clone())
}

/// Returns the port the sidecar is listening on. Returns 0 on failure.
#[tauri::command]
pub fn get_server_port(state: State<'_, AppState>) -> u16 {
    state.server_port.lock().map(|p| *p).unwrap_or(0)
}

/// Returns the master key used to encrypt provider API keys and other
/// secrets at rest. The setup wizard displays this to the user after
/// first-run so they can save it for future data migration / restore.
/// Returns `None` on lock failure or in non-Tauri environments.
#[tauri::command]
pub fn get_master_key(state: State<'_, AppState>) -> Option<String> {
    state.config.lock().ok().map(|cfg| cfg.master_key.clone())
}

/// Apply a master key (e.g. generated or rotated on the frontend),
/// persist it to config, and restart the sidecar so the new
/// `TIYGATE_MASTER_KEY` takes effect.
#[tauri::command]
pub async fn apply_master_key(
    app: AppHandle,
    state: State<'_, AppState>,
    key: String,
) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("master key cannot be empty".into());
    }
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        let data_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
        cfg.master_key = key.trim().to_string();
        cfg.save(&data_dir)
            .map_err(|e| format!("failed to save config: {e}"))?;
    }
    restart_with_current_config(&app, &state).await
}

/// Set a user-chosen admin token, persist it, and restart the sidecar
/// so the new `TIYGATE_ADMIN_TOKEN` takes effect. Marks first-run as
/// complete. After this call, the frontend should redirect the user to
/// the Login page so they can authenticate with the token they chose.
#[tauri::command]
pub async fn set_admin_token(
    app: AppHandle,
    state: State<'_, AppState>,
    token: String,
) -> Result<(), String> {
    if token.trim().is_empty() {
        return Err("token cannot be empty".into());
    }
    let token = token.trim().to_string();

    // Update config and persist.
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        let data_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
        cfg.update_admin_token(token.clone(), &data_dir)
            .map_err(|e| format!("failed to save config: {e}"))?;
        cfg.mark_first_run_done(&data_dir)
            .map_err(|e| format!("failed to save config: {e}"))?;
    }

    restart_with_current_config(&app, &state).await
}

/// Enable passwordless mode: keep the auto-generated token, mark
/// first-run as complete, and return the token so the frontend can
/// auto-login. No sidecar restart is needed because the token was
/// already injected at startup.
#[tauri::command]
pub async fn enable_passwordless(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let token = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        let data_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
        cfg.mark_first_run_done(&data_dir)
            .map_err(|e| format!("failed to save config: {e}"))?;
        cfg.admin_token.clone()
    };
    Ok(token)
}

// ---------------------------------------------------------------------------
// File save (backup export)
// ---------------------------------------------------------------------------

/// Show a native save-file dialog and write the supplied contents to
/// the chosen path. Used by the config export page inside Tauri's
/// macOS WKWebView, where the standard `<a download>` blob pattern
/// does not work (WKWebView cancels the navigation with
/// `NSURLErrorCancelled`).
///
/// Returns the path the file was written to, or `None` when the user
/// cancels the dialog.
#[tauri::command]
pub async fn save_backup_file(
    filename: String,
    contents: String,
) -> Result<Option<String>, String> {
    let path = rfd::AsyncFileDialog::new()
        .set_file_name(&filename)
        .add_filter("JSON", &["json"])
        .save_file()
        .await;

    let Some(path) = path else {
        // User cancelled.
        return Ok(None);
    };

    let path_str = path.path().to_string_lossy().to_string();
    std::fs::write(&path_str, contents.into_bytes())
        .map_err(|e| format!("failed to write file: {e}"))?;
    Ok(Some(path_str))
}

// ---------------------------------------------------------------------------
// Remote instance management commands
// ---------------------------------------------------------------------------

/// Serializable view of the currently active instance, returned by
/// `get_active_instance`. In Tauri, `serde` rename flattens `kind` to
/// a lowercase string the frontend can switch on.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveInstance {
    /// `"local"` for the sidecar, `"remote"` for a user-added instance.
    pub kind: String,
    /// Instance id. `None` for the local sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Human-friendly label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Base URL (without `/admin/v1`). For local, this is the
    /// `http://127.0.0.1:{port}` origin. For remote, the user-entered
    /// URL. The frontend appends `/admin/v1` itself.
    pub url: Option<String>,
}

/// Categorical health status for the instance indicator.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// 2xx from `/healthz`.
    Ok,
    /// Non-2xx but reachable (e.g. 503 unconfigured, 401).
    Warning,
    /// 4xx/5xx server error.
    Error,
    /// Connection failed entirely.
    Unreachable,
}

/// List all user-added (remote) instances. The local sidecar is not
/// included — it is always available implicitly.
#[tauri::command]
pub fn list_instances(state: State<'_, AppState>) -> Vec<InstanceEntry> {
    state
        .config
        .lock()
        .map(|cfg| cfg.instances.clone())
        .unwrap_or_default()
}

/// Add a new remote instance and persist the config. Returns the
/// created entry (with generated id and normalized URL).
#[tauri::command]
pub fn add_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    label: String,
    url: String,
    skip_tls_verify: bool,
) -> Result<InstanceEntry, String> {
    let label = label.trim().to_string();
    let url = url.trim().to_string();
    if label.is_empty() {
        return Err("label cannot be empty".into());
    }
    if url.is_empty() {
        return Err("url cannot be empty".into());
    }
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
    let entry = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        let new_entry = InstanceEntry {
            id: String::new(),
            label,
            url,
            skip_tls_verify,
        };
        let added = cfg.add_instance(new_entry);
        let clone = added.clone();
        cfg.save(&data_dir)
            .map_err(|e| format!("failed to save config: {e}"))?;
        clone
    };
    Ok(entry)
}

/// Update an existing remote instance by id.
#[tauri::command]
pub fn update_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    label: String,
    url: String,
    skip_tls_verify: bool,
) -> Result<(), String> {
    let label = label.trim().to_string();
    let url = url.trim().to_string();
    if label.is_empty() {
        return Err("label cannot be empty".into());
    }
    if url.is_empty() {
        return Err("url cannot be empty".into());
    }
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    if !cfg.update_instance(&id, label, url, skip_tls_verify) {
        return Err("instance not found".into());
    }
    cfg.save(&data_dir)
        .map_err(|e| format!("failed to save config: {e}"))
}

/// Remove a remote instance by id. If the removed instance was active,
/// the active instance falls back to local.
#[tauri::command]
pub fn remove_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    if !cfg.remove_instance(&id) {
        return Err("instance not found".into());
    }
    cfg.save(&data_dir)
        .map_err(|e| format!("failed to save config: {e}"))
}

/// Return information about the currently active instance. The
/// frontend uses this to decide the API base URL.
#[tauri::command]
pub fn get_active_instance(state: State<'_, AppState>) -> ActiveInstance {
    let (active_id, instances) = {
        let cfg = state.config.lock();
        match cfg {
            Ok(c) => (c.active_instance_id.clone(), c.instances.clone()),
            Err(_) => (None, Vec::new()),
        }
    };
    let port = state.server_port.lock().map(|p| *p).unwrap_or(0);

    // If a remote instance is active, return its info.
    if let Some(ref id) = active_id {
        if let Some(inst) = instances.iter().find(|i| &i.id == id) {
            return ActiveInstance {
                kind: "remote".into(),
                id: Some(inst.id.clone()),
                label: Some(inst.label.clone()),
                url: Some(inst.url.clone()),
            };
        }
    }
    // Fall back to local sidecar.
    ActiveInstance {
        kind: "local".into(),
        id: None,
        label: None,
        url: Some(format!("http://127.0.0.1:{port}")),
    }
}

/// Switch the active instance. `id = None` selects the local sidecar.
/// Updates both `active_instance_id` and `last_instance_id`, then
/// persists. Does NOT restart the sidecar — it keeps running.
#[tauri::command]
pub fn switch_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    id: Option<String>,
) -> Result<(), String> {
    // Validate that the id refers to an existing remote instance.
    if let Some(ref inst_id) = id {
        let exists = state
            .config
            .lock()
            .map(|cfg| cfg.instances.iter().any(|i| &i.id == inst_id))
            .map_err(|e| e.to_string())?;
        if !exists {
            return Err("instance not found".into());
        }
    }
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg.set_active_instance(id.clone());
    cfg.set_last_instance(id);
    cfg.save(&data_dir)
        .map_err(|e| format!("failed to save config: {e}"))
}

/// Return the last-selected instance id so the Setup wizard can
/// default to it. `None` means local.
#[tauri::command]
pub fn get_last_instance_id(state: State<'_, AppState>) -> Option<String> {
    state
        .config
        .lock()
        .ok()
        .and_then(|cfg| cfg.last_instance_id.clone())
}

/// Probe a remote instance's `/healthz` endpoint and return a
/// categorical health status. Uses a permissive TLS client when
/// `skip_tls_verify` is set so self-signed instances still work.
#[tauri::command]
pub async fn check_instance_health(
    url: String,
    skip_tls_verify: bool,
) -> Result<HealthStatus, String> {
    let base = url.trim_end_matches('/');
    if base.is_empty() {
        return Err("url cannot be empty".into());
    }
    let health_url = format!("{base}/healthz");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let resp = client.get(&health_url).send().await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            if (200..300).contains(&status) {
                Ok(HealthStatus::Ok)
            } else if status == 401 || status == 403 || status == 503 {
                Ok(HealthStatus::Warning)
            } else {
                Ok(HealthStatus::Error)
            }
        }
        Err(_) => Ok(HealthStatus::Unreachable),
    }
}

/// Restart the sidecar using the current configuration values. This is
/// called after `set_admin_token` changes the token, since
/// `TIYGATE_ADMIN_TOKEN` is read at sidecar startup.
async fn restart_with_current_config(
    app: &AppHandle,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    let (port, admin_token, master_key, db_url) = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        let port = *state.server_port.lock().map_err(|e| e.to_string())?;
        let data_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("failed to resolve app_local_data_dir: {e}"))?;
        let db_path = data_dir.join("tiygate.db");
        let db_url = format!(
            "sqlite://{}?mode=rwc",
            db_path.to_string_lossy().replace('\\', "/")
        );
        (
            port,
            cfg.admin_token.clone(),
            cfg.master_key.clone(),
            db_url,
        )
    };

    // Take the old sidecar manager out of state, then release the lock
    // before awaiting (MutexGuard is not Send).
    let old_mgr = {
        let mut guard = state.sidecar.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    if let Some(mut old) = old_mgr {
        old.shutdown().await;
    }

    // Brief pause to let the old process release the port.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let new_mgr = sidecar::spawn_sidecar(app, port, &admin_token, &master_key, &db_url)
        .await
        .map_err(|e| format!("failed to restart sidecar: {e}"))?;

    let mut guard = state.sidecar.lock().map_err(|e| e.to_string())?;
    *guard = Some(new_mgr);
    Ok(())
}
