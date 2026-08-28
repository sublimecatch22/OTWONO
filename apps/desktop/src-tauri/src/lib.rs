//! The OTWONO AI desktop shell.
//!
//! Its job is small and specific: start the local service inside this process,
//! hand the web view the address and token it needs, keep one window, and stop
//! cleanly. All the application's behaviour lives in the service.

use std::sync::Mutex;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

/// What the web view is told at start-up. The token is generated fresh on every
/// launch and is never written into the page's source.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeInfo {
    pub base_url: String,
    pub token: String,
    pub version: String,
}

#[derive(Default)]
struct ServiceState(Mutex<Option<RuntimeInfo>>);

/// The web view asks for this once, on load.
#[tauri::command]
fn runtime_info(state: tauri::State<'_, ServiceState>) -> Result<RuntimeInfo, String> {
    state
        .0
        .lock()
        .map_err(|_| "the service state was poisoned".to_string())?
        .clone()
        .ok_or_else(|| "the local service has not started yet".to_string())
}

/// Whether OTWONO starts when the user signs in. Off unless they ask.
#[tauri::command]
fn autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())?;
    } else {
        manager.disable().map_err(|e| e.to_string())?;
    }
    manager.is_enabled().map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,otwono=debug".into()),
        )
        .init();

    tauri::Builder::default()
        // A second launch focuses the window that is already open rather than
        // starting a second service against the same database.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(ServiceState::default())
        .invoke_handler(tauri::generate_handler![
            runtime_info,
            autostart_enabled,
            set_autostart
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Start the service before the window is shown, so the first
            // request the web view makes already has somewhere to go.
            tauri::async_runtime::spawn(async move {
                match otwono_local_service::start(0).await {
                    Ok(service) => {
                        let info = RuntimeInfo {
                            base_url: service.base_url(),
                            token: service.token.clone(),
                            version: env!("CARGO_PKG_VERSION").to_string(),
                        };
                        if let Some(state) = handle.try_state::<ServiceState>() {
                            if let Ok(mut slot) = state.0.lock() {
                                *slot = Some(info.clone());
                            }
                        }
                        // Tell the web view it can start talking to the service.
                        let _ = handle.emit("otwono://ready", info);
                        tracing::info!("local service ready");
                    }
                    Err(error) => {
                        tracing::error!(%error, "the local service could not start");
                        let _ = handle.emit(
                            "otwono://failed",
                            format!(
                                "OTWONO could not start its local service: {error}. Your data has \
                                 not been changed."
                            ),
                        );
                    }
                }
            });

            let show = MenuItem::with_id(app, "show", "Open OTWONO", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::with_id("otwono")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        cleanup();
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                if window.label() == "main" {
                    cleanup();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("the OTWONO desktop application failed to start");
}

/// Remove the handshake file so a stale token cannot be reused.
fn cleanup() {
    if let Ok(path) = otwono_local_service::runtime::handshake_path() {
        otwono_local_service::runtime::remove_handshake(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_information_carries_only_what_the_web_view_needs() {
        let info = RuntimeInfo {
            base_url: "http://127.0.0.1:51234".into(),
            token: "a-token".into(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_value(&info).unwrap();

        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(json["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1"));
        for absent in ["path", "database", "secret", "home"] {
            assert!(
                !json.to_string().to_lowercase().contains(absent),
                "runtime info should not mention {absent}"
            );
        }
    }
}
