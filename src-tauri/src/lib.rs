mod commands;
mod config;
mod sidecar;

use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

use crate::config::ClientConfig;
use crate::sidecar::SidecarManager;

/// Shared state managed by Tauri, holding the sidecar handle and the
/// resolved client configuration.
pub struct AppState {
    pub sidecar: Mutex<Option<SidecarManager>>,
    pub config: Mutex<ClientConfig>,
    pub server_port: Mutex<u16>,
}

/// Entry point for the Tauri client application.
pub fn run() {
    init_tracing();

    let app = match tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Run as a menu-bar (accessory) app on macOS: no Dock icon
            // and no Cmd+Tab entry. The window is still shown on launch
            // and can be toggled from the system-tray icon. This works
            // in both `cargo tauri dev` and bundled `.app` builds (the
            // static Info.plist LSUIElement key only takes effect in the
            // latter, so we set it at runtime as well).
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = handle.set_activation_policy(tauri::ActivationPolicy::Accessory) {
                    tracing::warn!("failed to set activation policy to Accessory: {e}");
                }
            }

            // Resolve the data directory and load (or create) the local
            // client configuration before spawning the sidecar.
            // Use app_local_data_dir (~/Library/Application Support/ on
            // macOS) to avoid triggering the macOS "Documents" TCC
            // permission prompt that app_data_dir can cause in unsigned
            // / debug builds.
            let data_dir = handle
                .path()
                .app_local_data_dir()
                .map_err(|e| anyhow::anyhow!("failed to resolve app_local_data_dir: {e}"))?;
            std::fs::create_dir_all(&data_dir)
                .map_err(|e| anyhow::anyhow!("failed to create data dir: {e}"))?;

            let mut client_config = ClientConfig::load_or_init(&data_dir)?;

            // Scan for an available port starting from 13000.
            let port = sidecar::find_available_port(13000)
                .ok_or_else(|| anyhow::anyhow!("no available port in range 13000-13099"))?;

            let db_path = data_dir.join("tiygate.db");
            // Use `sqlite:` (not `sqlite://`) so the URL parser does not
            // treat a Windows drive letter (e.g. `C:`) as the host part
            // of an authority. With `sqlite://C:/…` the `url` crate
            // parses `C` as host and strips it, leaving a relative path
            // that SQLite cannot open. `sqlite:` keeps the path verbatim.
            let db_url = format!(
                "sqlite:{}?mode=rwc",
                db_path.to_string_lossy().replace('\\', "/")
            );

            // Use the admin token already stored in the config (generated
            // during load_or_init when missing). The sidecar inherits it
            // through the TIYGATE_ADMIN_TOKEN environment variable.
            let admin_token = client_config.admin_token.clone();
            let master_key = client_config.master_key.clone();

            let sidecar_mgr = tauri::async_runtime::block_on(async {
                sidecar::spawn_sidecar(&handle, port, &admin_token, &master_key, &db_url).await
            })?;

            client_config.server_port = Some(port);
            client_config.reconcile_active_instance();
            client_config.save(&data_dir)?;

            app.manage(AppState {
                sidecar: Mutex::new(Some(sidecar_mgr)),
                config: Mutex::new(client_config),
                server_port: Mutex::new(port),
            });

            // Build the system tray icon with a context menu. The tray
            // allows the user to show/hide the window and quit the app
            // outright. Closing the window only hides it; the app keeps
            // running in the tray. Menu events are registered globally
            // once; the tray itself may be rebuilt later on macOS if the
            // system status item disappears after SystemUIServer restarts
            // or display/sleep transitions.
            // Register the tray menu handler once. Tauri menu events are
            // global, so this handler may also see future app/window menu
            // ids; unrecognized ids are ignored by handle_tray_menu_event.
            handle.on_menu_event(|app, event| {
                handle_tray_menu_event(app, event.id().as_ref());
            });

            build_main_tray(&handle)?;

            #[cfg(target_os = "macos")]
            start_tray_watchdog(handle.clone());

            // The webview loads frontendDist (tauri://localhost) which
            // has Tauri IPC. The frontend uses Tauri commands to get
            // the sidecar port and makes cross-origin fetch calls to
            // http://127.0.0.1:{port}/admin/v1/* for the API.
            // No window.eval redirect needed.

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::is_first_run,
            commands::get_admin_token,
            commands::set_admin_token,
            commands::enable_passwordless,
            commands::get_server_port,
            commands::open_external_url,
            commands::get_master_key,
            commands::apply_master_key,
            commands::save_backup_file,
            commands::list_instances,
            commands::add_instance,
            commands::update_instance,
            commands::remove_instance,
            commands::get_active_instance,
            commands::switch_instance,
            commands::get_last_instance_id,
            commands::check_instance_health,
        ])
        .on_window_event(|window, event| {
            // When the main window close is requested (e.g. clicking the
            // red traffic-light button on macOS or the X on Windows),
            // prevent the default close and hide the window instead so
            // the app keeps running in the system tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
    {
        Ok(app) => app,
        Err(e) => {
            tracing::error!("failed to build Tauri application: {e}");
            return;
        }
    };

    // Handle application-level exit events (Cmd+Q, dock quit, etc.).
    // These do NOT trigger WindowEvent::CloseRequested, so the sidecar
    // must be cleaned up here as well.
    app.run(|app_handle, event| match event {
        tauri::RunEvent::Exit => {
            shutdown_sidecar(app_handle);
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Resumed | tauri::RunEvent::Reopen { .. } => {
            repair_main_tray(app_handle);
        }
        _ => {}
    });
}

/// Create the main tray icon. This is intentionally reusable because
/// macOS can occasionally drop an `NSStatusItem` while the process keeps
/// running, for example after SystemUIServer restarts or display/sleep
/// transitions.
fn build_main_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "hide", "隐藏窗口", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出 TiyGate", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

    // Load the dedicated tray icon (a monochrome template PNG derived
    // from webui/public/icon-round.svg). On macOS it is registered as a
    // template image so the system automatically adapts it to dark/light
    // menu-bar appearance. The PNG is embedded at compile time via
    // `include_bytes!` so no filesystem access is needed at runtime.
    let tray_icon = load_tray_icon()?;

    TrayIconBuilder::with_id("main-tray")
        .icon(tray_icon)
        .icon_as_template(true)
        .tooltip("TiyGate")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            // Double-click (macOS) / left-click (Windows) toggles the
            // main window visibility.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.hide();
                    } else {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        })
        .build(app)?;

    Ok(())
}

fn handle_tray_menu_event(app: &tauri::AppHandle, id: &str) {
    match id {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "hide" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
        }
        "quit" => {
            shutdown_sidecar(app);
            app.exit(0);
        }
        _ => {}
    }
}

fn load_tray_icon() -> tauri::Result<tauri::image::Image<'static>> {
    tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon-template.png"))
}

#[cfg(target_os = "macos")]
fn repair_main_tray(app: &tauri::AppHandle) {
    if tray_needs_rebuild(app) {
        tracing::warn!("main tray icon is missing; rebuilding macOS status item");

        if let Some(old_tray) = app.remove_tray_by_id("main-tray") {
            // Force the stale NSStatusItem wrapper to be released before
            // creating a replacement with the same id.
            drop(old_tray);
        }

        if let Err(e) = build_main_tray(app) {
            tracing::warn!("failed to rebuild main tray icon: {e}");
        }
    } else if let Some(tray) = app.tray_by_id("main-tray") {
        if let Err(e) =
            load_tray_icon().and_then(|icon| tray.set_icon_with_as_template(Some(icon), true))
        {
            tracing::warn!("failed to refresh main tray icon: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
fn start_tray_watchdog(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            repair_main_tray(&app);
        }
    });
}

#[cfg(target_os = "macos")]
fn tray_needs_rebuild(app: &tauri::AppHandle) -> bool {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return true;
    };

    match tray.rect() {
        Ok(Some(rect)) => tray_rect_is_empty(rect),
        Ok(None) => true,
        Err(e) => {
            tracing::warn!("failed to read main tray icon rect: {e}");
            true
        }
    }
}

#[cfg(target_os = "macos")]
fn tray_rect_is_empty(rect: tauri::Rect) -> bool {
    let size = rect.size.to_physical::<u32>(1.0);
    size.width == 0 || size.height == 0
}

/// Shut down the sidecar process if it is still running. Safe to call
/// multiple times — the second call is a no-op because the manager is
/// `take()`n from the mutex on the first call.
fn shutdown_sidecar(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut guard) = state.sidecar.lock() {
            if let Some(mut mgr) = guard.take() {
                tracing::info!("shutting down sidecar on exit");
                tauri::async_runtime::block_on(async {
                    mgr.shutdown().await;
                });
            }
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
