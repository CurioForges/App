//! First-run setup: two directory choices and the weight-download token.
//!
//! The wizard is the only place the launcher writes user paths, and it writes them
//! exactly once into the distribution's own contract: a junction for `data/` and an
//! environment override for `models/`. Nothing here touches domain configuration.

use std::{
    path::PathBuf,
    time::Duration,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::{
    contract,
    credentials,
    locations::{self, DirectoryCheck},
    settings::{self, SettingsState},
    variant_runtime::install_dir,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Defaults {
    pub data_dir: String,
    pub models_dir: String,
    pub install_dir: String,
    pub system_drive: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    pub needs_setup: bool,
    pub data_dir: String,
    pub models_dir: String,
    pub token_hint: Option<String>,
    pub defaults: Defaults,
}

fn resolved(settings: &settings::Settings, root: &std::path::Path) -> (String, String) {
    (
        settings.data_path(root, crate::catalog::INSTALL_ID).display().to_string(),
        settings.models_path(root).display().to_string(),
    )
}

#[tauri::command]
pub async fn setup_state(state: State<'_, SettingsState>) -> Result<SetupState, String> {
    let root = install_dir()?;
    let settings = state.snapshot();
    let (data_dir, models_dir) = resolved(&settings, &root);
    Ok(SetupState {
        needs_setup: !settings.installation(crate::catalog::INSTALL_ID).setup_confirmed,
        data_dir,
        models_dir,
        token_hint: credentials::hint(),
        defaults: Defaults {
            data_dir: root.join(contract::DATA).display().to_string(),
            models_dir: root.join(contract::MODELS).display().to_string(),
            install_dir: root.display().to_string(),
            system_drive: std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()),
        },
    })
}

fn title_for(kind: &str) -> &'static str {
    match kind {
        "models" => "选择模型权重目录",
        _ => "选择项目数据目录",
    }
}

/// Native folder picker. The dialog answers on the UI thread; this waits for that
/// answer on a blocking worker so the async runtime is never parked.
#[tauri::command]
pub async fn choose_directory(app: AppHandle, kind: String, current: Option<String>) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut builder = app.dialog().file().set_title(title_for(&kind)).set_can_create_directories(true);
        if let Some(directory) = current.filter(|value| !value.trim().is_empty()) {
            builder = builder.set_directory(PathBuf::from(directory));
        }
        builder.pick_folder(move |picked| {
            let _ = sender.send(picked);
        });
        match receiver.recv_timeout(Duration::from_secs(900)) {
            Ok(Some(picked)) => Ok(picked.into_path().ok().map(|path| path.display().to_string())),
            Ok(None) => Ok(None),
            Err(_) => Err("选择目录超时".into()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryInspection {
    pub kind: String,
    /// Where this choice will physically land once applied.
    pub resolved: String,
    pub check: DirectoryCheck,
}

#[tauri::command]
pub async fn inspect_directory(kind: String, path: String) -> Result<DirectoryInspection, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let chosen = PathBuf::from(path.trim());
        if !chosen.is_absolute() {
            return Err("请选择一个绝对路径。".into());
        }
        Ok(DirectoryInspection {
            resolved: chosen.display().to_string(),
            check: locations::inspect(&kind, &chosen),
            kind,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCheck {
    pub valid: bool,
    pub account: Option<String>,
    pub role: Option<String>,
    pub message: String,
}

/// Ask Hugging Face who the token belongs to. The token is only ever sent to the
/// service that issued it, and it is never written into the message either.
fn verify(token: &str) -> Result<TokenCheck, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("无法创建网络连接：{e}"))?;
    let response = client
        .get("https://huggingface.co/api/whoami-v2")
        .bearer_auth(token)
        .send()
        .map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                "无法连接 Hugging Face。请检查网络或代理设置，然后再试一次。".to_string()
            } else {
                format!("请求 Hugging Face 失败：{e}")
            }
        })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(TokenCheck { valid: false, account: None, role: None, message: "Token 无效或已被撤销。".into() });
    }
    if !response.status().is_success() {
        return Ok(TokenCheck {
            valid: false,
            account: None,
            role: None,
            message: format!("Hugging Face 返回 {}", response.status().as_u16()),
        });
    }
    let body: serde_json::Value = response.json().map_err(|e| format!("响应无法解析：{e}"))?;
    let account = body.get("name").and_then(serde_json::Value::as_str).map(str::to_string);
    let role = body
        .pointer("/auth/accessToken/role")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let scope_hint = match role.as_deref() {
        Some("read") => "这个 Token 是只读权限，下载公开权重与已获许可的受限权重都够用。",
        Some("fineGrained") | Some("fine-grained") => "这是一个细粒度 Token，请确认它带有读取已获许可仓库的权限。",
        Some("write") => "这个 Token 带写入权限，权限比下载权重所需更大。",
        Some(_) => "已确认可用。",
        None => "已确认可用。",
    };
    Ok(TokenCheck {
        valid: true,
        account: account.clone(),
        role,
        message: match account {
            Some(name) => format!("已连接到 {name}。{scope_hint}"),
            None => scope_hint.to_string(),
        },
    })
}

#[tauri::command]
pub async fn test_token(token: String) -> Result<TokenCheck, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("请先粘贴 Token。".into());
    }
    tauri::async_runtime::spawn_blocking(move || verify(&trimmed)).await.map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupResult {
    pub data_dir: String,
    pub models_dir: String,
    pub linked: bool,
    pub token_hint: Option<String>,
}

/// Apply the wizard. All three answers are validated again here so a stale UI can
/// never write a link to a directory that has since become unwritable.
#[tauri::command]
pub async fn complete_setup(
    state: State<'_, SettingsState>,
    data_dir: String,
    models_dir: String,
    token: Option<String>,
) -> Result<SetupResult, String> {
    let root = install_dir()?;
    let data = PathBuf::from(data_dir.trim());
    let models = PathBuf::from(models_dir.trim());
    if !data.is_absolute() {
        return Err("数据目录必须是绝对路径。".into());
    }
    if !models.is_absolute() {
        return Err("模型权重目录必须是绝对路径。".into());
    }
    if models.starts_with(&data) || data.starts_with(&models) {
        return Err("数据目录和模型目录不能互相包含。".into());
    }
    for (kind, path) in [("data", &data), ("models", &models)] {
        let check = locations::inspect(kind, path);
        if let Some(blocker) = check.blockers.first() {
            return Err(format!("{} 目录不可用：{blocker}", if kind == "data" { "数据" } else { "模型" }));
        }
    }

    let root_data = root.join(contract::DATA);
    let outcome = locations::ensure_data_link(&root_data, &data)?;
    std::fs::create_dir_all(&models).map_err(|e| format!("无法创建模型目录：{e}"))?;

    let mut settings = state.snapshot();
    settings.shared.models_dir = models.display().to_string();
    if let Some(token) = token {
        credentials::store(&token)?;
        settings.shared.huggingface_token_hint = if token.trim().is_empty() {
            String::new()
        } else {
            credentials::hint().unwrap_or_default()
        };
    }
    {
        let installation = settings.installation_mut(crate::catalog::INSTALL_ID);
        installation.data_dir = data.display().to_string();
        installation.setup_confirmed = true;
    }
    settings::save(&settings)?;
    *state.0.lock().map_err(|e| e.to_string())? = settings;

    Ok(SetupResult {
        data_dir: data.display().to_string(),
        models_dir: models.display().to_string(),
        linked: outcome.linked,
        token_hint: credentials::hint(),
    })
}

/// Reveal a runtime directory in Explorer. `logs` opens the data directory, because
/// kernel and domain logs are stored in the kernel database there.
pub fn reveal(app: &AppHandle, kind: &str) -> Result<(), String> {
    let root = install_dir()?;
    let settings = app.try_state::<SettingsState>().map(|state| state.snapshot()).unwrap_or_default();
    let target = match kind {
        "models" => settings.models_path(&root),
        "install" => root.clone(),
        _ => settings.data_path(&root, crate::catalog::INSTALL_ID),
    };
    if !target.is_dir() {
        std::fs::create_dir_all(&target).map_err(|e| format!("目录还不存在：{e}"))?;
    }
    std::process::Command::new("explorer.exe")
        .arg(&target)
        .spawn()
        .map_err(|e| format!("无法打开目录：{e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn open_runtime_path(app: AppHandle, kind: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || reveal(&app, &kind)).await.map_err(|e| e.to_string())?
}

/// Put the launcher back into first-run setup.
///
/// Only the `setup_complete` flag is cleared: the saved directories and the stored
/// credential stay, so the wizard reopens with the current answers pre-filled and
/// nothing on disk is touched. Re-applying is idempotent — the data junction is only
/// created when the choice actually differs from where it already points.
pub fn reopen(app: &AppHandle) -> Result<(), String> {
    let state = app.try_state::<SettingsState>().ok_or("设置状态不可用")?;
    let mut settings = state.snapshot();
    settings.installation_mut(crate::catalog::INSTALL_ID).setup_confirmed = false;
    settings::save(&settings)?;
    *state.0.lock().map_err(|e| e.to_string())? = settings;
    app.emit("atelier://setup", ()).map_err(|e| e.to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    Ok(())
}

/// Command twin of the tray entry, so the launcher UI can offer it too.
#[tauri::command]
pub fn reopen_setup(app: AppHandle) -> Result<(), String> {
    reopen(&app)
}

/// Tray/menu entry that re-surfaces the launcher window.
#[tauri::command]
pub fn show_window(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    Ok(())
}
