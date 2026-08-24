mod server;

use std::sync::Mutex;

use server::{ServerManager, StatusPayload, DEFAULT_PORT};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct AppState {
    server: Mutex<ServerManager>,
    /// "开机自启动" checkable tray menu item, kept in sync with the panel.
    autostart_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> StatusPayload {
    state.server.lock().unwrap().status()
}

#[tauri::command]
fn start_server(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.server.lock().unwrap().start(&app)
}

#[tauri::command]
fn restart_server(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.server.lock().unwrap().restart(&app)
}

#[tauri::command]
fn open_in_browser(state: State<'_, AppState>) -> Result<(), String> {
    let url = state.server.lock().unwrap().url();
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_log(state: State<'_, AppState>) -> String {
    state.server.lock().unwrap().tail_log(200)
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Set the OS autostart flag, sync the tray check mark and broadcast the new
/// state to every window (panel included).
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let currently = app.autolaunch().is_enabled().map_err(|e| e.to_string())?;
    if enabled != currently {
        let result = if enabled {
            app.autolaunch().enable()
        } else {
            app.autolaunch().disable()
        };
        result.map_err(|e| e.to_string())?;
    }
    let new_state = app.autolaunch().is_enabled().map_err(|e| e.to_string())?;
    sync_autostart(&app);
    let _ = app.emit("autostart-changed", new_state);
    Ok(new_state)
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Manual upgrade, triggered from the tray menu: stop the server, drop the
/// npx cache entry for @deepseek-ai/dsh so the next start re-fetches the
/// latest version, then restart the server. Progress is broadcast via the
/// "upgrade-status" event.
#[tauri::command]
fn upgrade_dsh(app: AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || upgrade_dsh_impl(&handle));
}

fn upgrade_dsh_impl(app: &AppHandle) {
    let _ = app.emit(
        "upgrade-status",
        serde_json::json!({ "state": "started" }),
    );

    // 1. stop the current server (if any).
    if let Some(state) = app.try_state::<AppState>() {
        state.server.lock().unwrap().stop();
    }
    // Give the killed process tree a moment to fully exit and release file
    // handles before we try to delete the npx cache.
    std::thread::sleep(std::time::Duration::from_millis(600));

    // 2. clear the npx cache entry for @deepseek-ai/dsh.
    let cleared = get_npm_cache_dir()
        .map(|d| remove_dsh_from_npx(d.join("_npx").as_path()))
        .unwrap_or(false);

    // 3. restart; npx will now download the latest version.
    let result: Result<Option<()>, String> = app
        .try_state::<AppState>()
        .map(|s| s.server.lock().unwrap().start(app))
        .transpose();
    match result {
        Ok(Some(())) => {
            let _ = app.emit(
                "upgrade-status",
                serde_json::json!({ "state": "done", "cacheCleared": cleared }),
            );
        }
        Ok(None) => {}
        Err(e) => {
            let _ = app.emit(
                "upgrade-status",
                serde_json::json!({ "state": "error", "message": e }),
            );
        }
    }
}

/// npm cache root (from `npm config get cache`), falling back to the default
/// Windows location.
fn get_npm_cache_dir() -> Option<PathBuf> {
    let out = Command::new("cmd")
        .args(["/C", "npm config get cache"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("npm-cache"))
}

/// Remove the `@deepseek-ai` scope inside the npx cache so the next `npx`
/// invocation re-resolves the latest version. Retries a few times in case
/// a dying process still holds file handles.
fn remove_dsh_from_npx(npx_dir: &Path) -> bool {
    for _ in 0..5 {
        if remove_dsh_from_npx_once(npx_dir) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    false
}

fn remove_dsh_from_npx_once(npx_dir: &Path) -> bool {
    let mut removed = false;
    if let Ok(entries) = std::fs::read_dir(npx_dir) {
        for entry in entries.flatten() {
            let scope = entry.path().join("node_modules").join("@deepseek-ai");
            if scope.exists() {
                removed |= std::fs::remove_dir_all(&scope).is_ok();
            }
        }
    }
    // Also handle the legacy top-level layout.
    let top = npx_dir.join("node_modules").join("@deepseek-ai");
    if top.exists() {
        removed |= std::fs::remove_dir_all(&top).is_ok();
    }
    removed
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            server: Mutex::new(ServerManager::new(DEFAULT_PORT)),
            autostart_item: Mutex::new(None),
        })
        .setup(|app| {
            build_tray(app.handle())?;
            sync_autostart(app.handle());
            // Kick off the server as soon as the app boots. The webview stays
            // on our shell page (index.html), which embeds the harness in an
            // <iframe> once the server reports ready.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                if let Some(state) = handle.try_state::<AppState>() {
                    if let Err(e) = state.server.lock().unwrap().start(&handle) {
                        let _ = handle.emit(
                            "server-status",
                            StatusPayload {
                                state: server::ServerState::Error,
                                port: DEFAULT_PORT,
                                url: format!("http://127.0.0.1:{DEFAULT_PORT}"),
                                error: Some(e),
                                elapsed_secs: None,
                            },
                        );
                    }
                }
            });
            Ok(())
        })
        // Closing the window hides it to the tray instead of quitting.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "restart" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let mut mgr = state.server.lock().unwrap();
                    let _ = mgr.restart(app);
                }
            }
            "browser" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let url = state.server.lock().unwrap().url();
                    let _ = tauri_plugin_opener::open_url(url, None::<&str>);
                }
            }
            "upgrade" => upgrade_dsh(app.clone()),
            "toggle-autostart" => {
                let currently = app.autolaunch().is_enabled().unwrap_or(false);
                let _ = set_autostart(app.clone(), !currently);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            start_server,
            restart_server,
            open_in_browser,
            get_log,
            get_autostart,
            set_autostart,
            quit_app,
            upgrade_dsh
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// System tray: 显示主窗口 / 重启服务 / 在浏览器中打开 / 开机自启动 / 退出
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let upgrade = MenuItem::with_id(app, "upgrade", "升级 dsh", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "重启服务", true, None::<&str>)?;
    let browser = MenuItem::with_id(app, "browser", "在浏览器中打开", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(app, "toggle-autostart", "开机自启动", true, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&show, &upgrade, &restart, &browser, &sep, &autostart, &sep, &quit],
    )?;

    let mut tray = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("DeepSeek Harness")
        .on_tray_icon_event(|tray, event| {
            // Left click on the icon brings the main window back.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;

    if let Some(state) = app.try_state::<AppState>() {
        *state.autostart_item.lock().unwrap() = Some(autostart);
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Sync the tray check mark and notify every window about the autostart state.
fn sync_autostart(app: &AppHandle) {
    let enabled = app.autolaunch().is_enabled().unwrap_or(false);
    if let Some(state) = app.try_state::<AppState>() {
        if let Some(item) = state.autostart_item.lock().unwrap().as_ref() {
            let _ = item.set_checked(enabled);
        }
    }
    let _ = app.emit("autostart-changed", enabled);
}
