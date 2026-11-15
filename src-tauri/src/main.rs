#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod bootspec;
mod capabilities;
mod catalog;
mod contract;
mod credentials;
mod installer;
mod locations;
// 迁移安装还没接到 UI（「⋯ → 迁移安装」）：先把这套原语留着，接上命令后去掉这个 allow。
#[allow(dead_code)]
mod migrate;
mod net;
mod relocate;
mod self_update;
mod settings;
mod setup;
mod storage;
mod variant_runtime;
#[cfg(windows)]
mod win;

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
    // 「重新运行初始设置」只在开发构建里出现:发布版不该给用户一条回到向导的入口。
    #[cfg(debug_assertions)]
    let setup = Some(MenuItemBuilder::with_id("setup", "重新运行初始设置").build(app)?);
    let data = MenuItemBuilder::with_id("data", "打开数据目录").build(app)?;
    let models = MenuItemBuilder::with_id("models", "打开模型目录").build(app)?;
    let stop = MenuItemBuilder::with_id("stop", "停止 Atelier Free").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出 Atelier").build(app)?;
    #[cfg(debug_assertions)]
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open, setup.as_ref().unwrap(), &data, &models, &stop, &quit];
    #[cfg(not(debug_assertions))]
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open, &data, &models, &stop, &quit];
    let menu = MenuBuilder::new(app).items(items.as_mut_slice()).build()?;

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
                // The handoff contract says leaving the launcher stops the kernel. That is done
                // once, in the RunEvent::ExitRequested hook below, so every exit path gets it.
                app.exit(0);
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
///
/// This deletes tens of thousands of files (`runtime-site-packages`, the hosts, `dist`),
/// which is why it runs on a blocking worker: doing it on the UI thread froze the window
/// into "未响应" for the whole deletion.
#[tauri::command]
async fn uninstall_variant(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || uninstall_sync(&app))
        .await
        .map_err(|e| e.to_string())?
}

fn uninstall_sync(app: &AppHandle) -> Result<(), String> {
    use std::io::ErrorKind;
    let dir = install_dir()?;
    if kernel_running(&dir)? {
        return Err("请先关闭 Atelier Free，再卸载。".into());
    }
    let entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let total = entries.len().max(1);
    let mut removed = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        let name = entry.file_name();
        let label = name.to_string_lossy().to_string();
        if contract::PRESERVED.iter().any(|kept| label.eq_ignore_ascii_case(kept)) {
            continue;
        }
        installer::report(
            app,
            "uninstall",
            format!("正在删除 {label}"),
            Some(((index as f32) / total as f32) * 100.0),
            Some(label.clone()),
        );
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        let outcome = if metadata.file_type().is_symlink() {
            fs::remove_dir(&path)
        } else if metadata.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match outcome {
            Ok(()) => { removed += 1; }
            // 用户目录里的连接/正在被占用的文件不该拦下整个卸载。
            Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {}
            Err(error) => return Err(format!("无法删除 {label}：{error}")),
        }
    }
    installer::report(app, "done", format!("已删除 {removed} 项程序文件"), Some(100.0), None);
    // 程序文件删掉了,占用条要跟着变。
    storage::refresh(app.clone());
    Ok(())
}

/// 构建属性：界面用它决定要不要露出只在开发构建里存在的入口。
#[tauri::command]
fn build_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "debug": cfg!(debug_assertions),
    })
}

fn main() {
    let app = tauri::Builder::default()
        // Must be registered first: it owns the instance mutex before any other plugin.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| reveal(app)))
        .plugin(tauri_plugin_dialog::init())
        .manage(settings::SettingsState::default())
        .manage(StatusWatch::default())
        .manage(storage::StorageState::default())
        .setup(|app| {
            let handle = app.handle();
            if let Some(state) = handle.try_state::<settings::SettingsState>() {
                if let Ok(mut settings) = state.0.lock() {
                    *settings = settings::load();
                }
            }
            install_tray(handle)?;
            // 占用统计:先把上次的账本摆出来,再在后台扫一遍校准(70 GB 要几十秒)。
            storage::prime(handle);
            storage::refresh(handle.clone());
            // 排障钩子(仅开发构建):设了 ATELIER_TRACE_PREWARM 就在启动 3 秒后自动跑一遍预热。
            #[cfg(debug_assertions)]
            if std::env::var_os("ATELIER_TRACE_PREWARM").is_some() {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    installer::trace("hook: auto prewarm");
                    let _ = installer::prewarm_blocking(&handle);
                });
            }
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
            installer::install_only,
            installer::prewarm_only,
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
            relocate::locations_view,
            relocate::relocate_install,
            relocate::relocate_data,
            relocate::relocate_models,
            setup::inspect_directory,
            setup::test_token,
            setup::complete_setup,
            setup::reopen_setup,
            setup::open_runtime_path,
            setup::show_window,
            setup::open_url,
            build_info,
            storage::storage_usage,
            self_update::check_self_update,
            self_update::apply_self_update
        ])
        .build(tauri::generate_context!())
        .expect("Atelier 启动失败");

    // 退出就是退出:实例不许留在后台。以前只有托盘「退出」会停 kernel,窗口关掉(没开
    // 关到托盘)、系统关机、Tauri 自己退出这些路径都会把 kernel 和它管着的域留在后台,
    // 而启动器已经不在了 —— 没有窗口、没有托盘图标,没人能停它们。
    let stopped = std::sync::atomic::AtomicBool::new(false);
    app.run(move |_handle, event| {
        if matches!(event, tauri::RunEvent::ExitRequested { .. })
            && !stopped.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _ = variant_runtime::stop_sync();
        }
    });
}
