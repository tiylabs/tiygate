mod commands;
mod config;
mod sidecar;

#[cfg(all(target_os = "macos", debug_assertions))]
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

#[cfg(all(target_os = "macos", debug_assertions))]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
};

use crate::config::ClientConfig;
use crate::sidecar::SidecarManager;

#[cfg(all(target_os = "macos", debug_assertions))]
const TRAY_TEST_SOCKET_NAME: &str = "tiygate-tray-test.sock";

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

    // Record startup before the event loop begins. The tray watchdog uses
    // this to avoid rebuilding a status item while macOS is still laying
    // out the menu bar during launch.
    #[cfg(target_os = "macos")]
    app_start_time();

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

            // Debug builds expose a local Unix socket that can deliberately
            // hide the tray item, allowing deterministic recovery tests
            // without relying on a SystemUIServer or display transition.
            #[cfg(all(target_os = "macos", debug_assertions))]
            start_tray_test_server(handle.clone());

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
        tauri::RunEvent::Resumed => {
            // Only repair the tray after startup: during the first five
            // seconds macOS may still be laying out its status item.
            if app_start_time().elapsed() >= std::time::Duration::from_secs(5) {
                schedule_tray_repair(app_handle.clone());
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => {
            if app_start_time().elapsed() >= std::time::Duration::from_secs(5) {
                schedule_tray_repair(app_handle.clone());
            }

            // A menu-bar accessory app has no Dock or Cmd+Tab entry. Treat a
            // Finder or Launchpad reopen as an explicit request to bring the
            // panel forward. `has_visible_windows` is only AppKit's native
            // visibility snapshot; it can be true while the panel is hidden
            // behind another app or on a different Space.
            tracing::info!("macOS requested TiyGate reopen; showing main window");
            show_main_window(app_handle);
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

    // Load the dedicated tray icon (a monochrome template PNG derived from
    // webui/public/icon-round.svg). On macOS it is registered as a template
    // image so the system automatically adapts it to dark/light
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
                        show_main_window(app);
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
            show_main_window(app);
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

/// Make the primary window visible and active, logging failures rather than
/// silently leaving a menu-bar-only app inaccessible.
fn show_main_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("main window is unavailable while attempting to show it");
        return;
    };

    if let Err(e) = window.show() {
        tracing::warn!("failed to show main window: {e}");
        return;
    }

    if let Err(e) = window.set_focus() {
        tracing::warn!("failed to focus main window: {e}");
    }
}

fn load_tray_icon() -> tauri::Result<tauri::image::Image<'static>> {
    tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon-template.png"))
}

#[cfg(target_os = "macos")]
fn repair_main_tray(app: &tauri::AppHandle) {
    if tray_needs_rebuild(app) {
        tracing::warn!("main tray icon is missing; recreating macOS status item");

        if let Some(tray) = app.tray_by_id("main-tray") {
            // Do not remove the Tauri resource here. `remove_tray_by_id`
            // returns a native NSStatusItem wrapper whose final drop can run
            // on a Tokio worker, but AppKit requires that destruction on the
            // main thread. Tauri's visibility methods synchronously marshal
            // to the main thread and recreate the underlying status item.
            if let Err(e) = tray.set_visible(false).and_then(|_| tray.set_visible(true)) {
                tracing::warn!("failed to recreate main tray icon: {e}");
            }
        } else if let Err(e) = build_main_tray(app) {
            tracing::warn!("failed to rebuild unregistered main tray icon: {e}");
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
fn schedule_tray_repair(app: tauri::AppHandle) {
    // `TrayIcon::rect` and `TrayIconBuilder::build` synchronously dispatch to
    // the AppKit main thread. Run the health check from a worker so a macOS
    // RunEvent handler never waits on its own event loop.
    tauri::async_runtime::spawn(async move {
        repair_main_tray(&app);
    });
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

/// Start the debug-only local command server used to test tray recovery.
///
/// The Unix socket is owned by the current user and is not present in release
/// builds. See `scripts/inject-desktop-tray-loss.sh` for the client command.
#[cfg(all(target_os = "macos", debug_assertions))]
fn start_tray_test_server(app: tauri::AppHandle) {
    let socket_path = match app.path().app_local_data_dir() {
        Ok(data_dir) => data_dir.join(TRAY_TEST_SOCKET_NAME),
        Err(e) => {
            tracing::warn!("failed to resolve desktop tray test socket path: {e}");
            return;
        }
    };

    tauri::async_runtime::spawn(async move {
        match std::fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("failed to remove stale desktop tray test socket: {e}");
                return;
            }
        }

        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("failed to bind desktop tray test socket: {e}");
                return;
            }
        };

        if let Err(e) =
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("failed to restrict desktop tray test socket permissions: {e}");
            return;
        }

        tracing::info!(
            socket = %socket_path.display(),
            "desktop tray test socket is ready"
        );

        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(e) => {
                    tracing::warn!("desktop tray test socket accept failed: {e}");
                    continue;
                }
            };

            let mut buffer = [0_u8; 64];
            let command = match stream.read(&mut buffer).await {
                Ok(read) if read > 0 => String::from_utf8_lossy(&buffer[..read]).trim().to_owned(),
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("desktop tray test socket read failed: {e}");
                    continue;
                }
            };

            let response = match command.as_str() {
                "simulate-loss" => simulate_tray_loss(&app),
                "status" => tray_test_status(&app),
                _ => "error: supported commands are simulate-loss and status\n".to_owned(),
            };

            if let Err(e) = stream.write_all(response.as_bytes()).await {
                tracing::warn!("desktop tray test socket write failed: {e}");
            }
        }
    });
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn simulate_tray_loss(app: &tauri::AppHandle) -> String {
    let Some(tray) = app.tray_by_id("main-tray") else {
        return "ok: tray item was already absent\n".to_owned();
    };

    // This is an intentional, safe loss simulation. `set_visible` is
    // dispatched by Tauri to the AppKit main thread, unlike dropping a
    // `TrayIcon` resource from this Tokio socket task.
    match tray.set_visible(false) {
        Ok(()) => {
            tracing::warn!(
                "tray test command hid the status item; watchdog should recreate it within 30 seconds"
            );
            "ok: tray item hidden; wait up to 30 seconds for recovery\n".to_owned()
        }
        Err(e) => format!("error: failed to hide tray item: {e}\n"),
    }
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn tray_test_status(app: &tauri::AppHandle) -> String {
    match app.tray_by_id("main-tray") {
        Some(tray) => match tray.rect() {
            Ok(Some(rect)) => format!("status: tray registered; rect={rect:?}\n"),
            Ok(None) => "status: tray registered; macOS returned no rect\n".to_owned(),
            Err(e) => format!("status: tray registered; rect unavailable: {e}\n"),
        },
        None => "status: no tray item registered\n".to_owned(),
    }
}

/// Track the app start time so the tray watchdog can skip rebuilds
/// during the initial startup grace period (first 5 seconds).
#[cfg(target_os = "macos")]
fn app_start_time() -> std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrayStatus {
    Registered,
    Missing,
    Unreadable,
}

#[cfg(target_os = "macos")]
fn should_rebuild_tray(startup_complete: bool, status: TrayStatus) -> bool {
    startup_complete && status != TrayStatus::Registered
}

#[cfg(target_os = "macos")]
fn tray_needs_rebuild(app: &tauri::AppHandle) -> bool {
    let status = match app.tray_by_id("main-tray") {
        None => TrayStatus::Missing,
        Some(tray) => match tray.rect() {
            // A non-empty rectangle proves that AppKit registered the status
            // item, but not that the item is unobscured. On a notched display,
            // menu-bar overflow can leave this rect underneath the camera
            // housing. The debug verifier compares it with NSScreen's usable
            // menu-bar area; rebuilding cannot repair system-level overflow.
            Ok(Some(rect)) if !tray_rect_is_empty(rect) => TrayStatus::Registered,
            // After startup, a missing rectangle means macOS no longer has a
            // visible NSStatusItem. This is the condition the watchdog is
            // designed to repair.
            Ok(Some(_)) | Ok(None) => TrayStatus::Missing,
            Err(e) => {
                tracing::warn!("failed to read main tray icon rect; rebuilding it: {e}");
                TrayStatus::Unreadable
            }
        },
    };

    should_rebuild_tray(
        app_start_time().elapsed() >= std::time::Duration::from_secs(5),
        status,
    )
}

#[cfg(target_os = "macos")]
fn tray_rect_is_empty(rect: tauri::Rect) -> bool {
    let size = rect.size.to_physical::<u32>(1.0);
    size.width == 0 || size.height == 0
}

#[cfg(all(test, target_os = "macos"))]
mod tray_tests {
    use super::{should_rebuild_tray, TrayStatus};

    #[test]
    fn tray_recovery_waits_for_startup_grace_period() {
        for status in [
            TrayStatus::Registered,
            TrayStatus::Missing,
            TrayStatus::Unreadable,
        ] {
            assert!(!should_rebuild_tray(false, status));
        }
    }

    #[test]
    fn tray_recovery_rebuilds_missing_or_unreadable_status_items() {
        assert!(!should_rebuild_tray(true, TrayStatus::Registered));
        assert!(should_rebuild_tray(true, TrayStatus::Missing));
        assert!(should_rebuild_tray(true, TrayStatus::Unreadable));
    }
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
