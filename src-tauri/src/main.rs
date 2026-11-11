#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod capabilities;
mod catalog;
mod contract;
mod credentials;
mod installer;
mod locations;
mod self_update;
mod settings;
mod setup;
mod variant_runtime;

use std::fs;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WindowEvent,
};
use variant_runtime::{install_dir, kernel_running, StatusWatch};

/// Surface the window from anywhere: second launch, tray click, tray menu.
fn reveal(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Resident in the notification area so closing the window does not kill a running
/// workbench, and so a second launch has something to hand control back to.
fn install_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("open", "打开 Atelier").build(app)?;
    let setup = MenuItemBuilder::with_id("setup", "重新运行初始设置").build(app)?;
    let data = MenuItemBuilder::with_id("data", "打开数据目录").build(app)?;
    let models = MenuItemBuilder::with_id("models", "打开模型目录").build(app)?;
    let stop = MenuItemBuilder::with_id("stop", "停止 Atelier Free").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 Atelier").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&open, &setup, &data, &models, &stop, &quit]).build()?;

    let mut builder = TrayIconBuilder::with_id("atelier")
        .tooltip("Atelier")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => reveal(app),
            "setup" => {
                // Cheap way back to the three-step page without editing state files.
                let _ = setup::reopen(app);
            }
            "data" => {
                let _ = setup::reveal(app, "data");
            }
            "models" => {
                let _ = setup::reveal(app, "models");
            }
            "stop" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    // The status watcher reports the shutdown once the kernel exits.
                    let _ = variant_runtime::stop_sync();
                    reveal(&app);
                });
            }
            "quit" => {
                // The handoff contract says leaving the launcher stops the kernel.
                let app = app.clone();
                std::thread::spawn(move || {
                    let _ = variant_runtime::stop_sync();
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                reveal(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}


/// Remove the program files while keeping user data. A junction is a reparse point,
/// so it is removed with `remove_dir`; recursing would delete the user's real data.
#[tauri::command]
fn uninstall_variant() -> Result<(), String> {
    let dir = install_dir()?;
    if kernel_running(&dir)? {
        return Err("请先关闭 Atelier Free，再卸载。".into());
    }
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if contract::PRESERVED.iter().any(|name| entry.file_name().to_string_lossy().eq_ignore_ascii_case(name)) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            fs::remove_dir(&path).map_err(|e| e.to_string())?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
        } else {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // Must be registered first: it owns the instance mutex before any other plugin.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| reveal(app)))
        .plugin(tauri_plugin_dialog::init())
        .manage(settings::SettingsState::default())
        .manage(StatusWatch::default())
        .setup(|app| {
            let handle = app.handle();
            if let Some(state) = handle.try_state::<settings::SettingsState>() {
                if let Ok(mut settings) = state.0.lock() {
                    *settings = settings::load();
                }
            }
            install_tray(handle)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let close_to_tray = window
                    .try_state::<settings::SettingsState>()
                    .map(|state| state.snapshot().shared.close_to_tray)
                    .unwrap_or(true);
                if close_to_tray {
                    // Stay resident: hiding keeps the embedded workbench and the
                    // kernel it supervises alive.
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            variant_runtime::variant_info,
            installer::install_variant,
            uninstall_variant,
            variant_runtime::open_workbench,
            variant_runtime::foreign_installs,
            variant_runtime::stop_variant,
            variant_runtime::watch_variant,
            setup::setup_state,
            setup::choose_directory,
            capabilities::capabilities,
            capabilities::shared_settings,
            capabilities::save_shared,
            setup::inspect_directory,
            setup::test_token,
            setup::complete_setup,
            setup::reopen_setup,
            setup::open_runtime_path,
            setup::show_window,
            self_update::check_self_update,
            self_update::apply_self_update
        ])
        .run(tauri::generate_context!())
        .expect("Atelier 启动失败");
}
