//! Launcher-side settings. Deliberately separate from the distribution's own
//! `kernel_boot.json`: the launcher only records user choices, never runtime
//! wiring, and never stores credentials (see `credentials`).
//!
//! Schema 2 splits state by scope, because the two halves have different owners:
//!
//! * `shared` is machine-level — one weight library and one set of credentials for
//!   every installation on this machine.
//! * `installations` is per installed product. A distribution owns one root, so its
//!   data directory and its enabled-domain set belong to it.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

/// Bumped whenever the on-disk shape changes; `load` migrates anything older.
pub const SCHEMA: u32 = 3;

/// Values that are the same for every installation on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Shared {
    /// Absolute path for model weights. Empty means "the installation's default".
    pub models_dir: String,
    /// Last four characters of the stored Hugging Face token. Never the token.
    pub huggingface_token_hint: String,
    /// LLM service endpoint; the engines domain only reads this from its own
    /// settings store, so the launcher can only carry it and write it later.
    pub ollama_host: String,
    /// Same idea for the LLM credential.
    pub ollama_token_hint: String,
    /// Closing the window keeps the launcher resident in the notification area.
    pub close_to_tray: bool,
    /// Optional endpoint for the sellable-pipeline catalog. Empty disables it.
    pub catalog_url: String,
    /// Absolute install root (the whole distribution lives here). Empty means
    /// "the historical default under LOCALAPPDATA". Written by the wizard's
    /// install-location step or by a migration; not by hand.
    pub install_dir: String,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            models_dir: String::new(),
            huggingface_token_hint: String::new(),
            ollama_host: String::new(),
            ollama_token_hint: String::new(),
            close_to_tray: true,
            catalog_url: String::new(),
            install_dir: String::new(),
        }
    }
}

/// State that belongs to one installed distribution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Installation {
    /// Absolute path for project data. Empty means "the installation's default".
    pub data_dir: String,
    /// Set once this installation's setup page has been completed.
    pub setup_confirmed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub shared: Shared,
    pub installations: BTreeMap<String, Installation>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { schema: SCHEMA, shared: Shared::default(), installations: BTreeMap::new() }
    }
}

impl Settings {
    pub fn installation(&self, id: &str) -> Installation {
        self.installations.get(id).cloned().unwrap_or_default()
    }

    pub fn installation_mut(&mut self, id: &str) -> &mut Installation {
        self.installations.entry(id.to_string()).or_default()
    }

    /// Effective data directory for one installation. Falls back to `<root>/data`
    /// exactly like the distribution's own default.
    pub fn data_path(&self, root: &Path, install: &str) -> PathBuf {
        absolute(&self.installation(install).data_dir).unwrap_or_else(|| root.join(crate::contract::DATA))
    }

    /// Effective model directory. Shared across installations; empty falls back to
    /// `<root>/models`, the engines domain's own default.
    pub fn models_path(&self, root: &Path) -> PathBuf {
        absolute(&self.shared.models_dir).unwrap_or_else(|| root.join(crate::contract::MODELS))
    }
}

/// 修掉"数据目录指着已经不在、或者已经空掉的旧安装根"。
///
/// 安装根换过位置、或者用户手工删了旧目录之后，`<root>/data` 里明明有数据，
/// 设置却还指着旧路径 —— `ensure_data_link` 会拒绝启动（"发行包目录里已经有数据"）。
/// 判据：`<root>/data` 是真实目录且非空，而配置的那个目录不存在、是空的、或者不是目录。
pub fn heal_data_dir(settings: &mut Settings, root: &Path) -> bool {
    let Some(configured) = absolute(&settings.installation(crate::catalog::INSTALL_ID).data_dir) else {
        return false;
    };
    let fallback = root.join(crate::contract::DATA);
    if !fallback.is_dir() || crate::locations::is_link(&fallback) {
        return false;
    }
    let has_entries = |path: &Path| {
        std::fs::read_dir(path).map(|mut entries| entries.next().is_some()).unwrap_or(false)
    };
    if !has_entries(&fallback) {
        return false;
    }
    let same = configured
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(fallback.to_string_lossy().trim_end_matches(['\\', '/']));
    if same {
        return false;
    }
    // 配置的那个目录里确实有东西就尊重它，不替用户改主意。
    if configured.is_dir() && has_entries(&configured) {
        return false;
    }
    settings.installation_mut(crate::catalog::INSTALL_ID).data_dir = String::new();
    true
}

fn absolute(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.is_absolute().then_some(path)
}

pub fn state_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("Atelier"))
        .ok_or_else(|| "LOCALAPPDATA 未设置".to_string())
}

pub fn settings_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("launcher.json"))
}

/// UTF-8 BOM at the top of the file used to make the whole parse fail, which silently reset
/// every choice (install dir, model dir, token hint) and dropped the user back into the wizard
/// on what looked like a perfectly good file — PowerShell's `Out-File`/`Set-Content` add one.
fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

pub fn load() -> Settings {
    let Ok(path) = settings_path() else { return Settings::default() };
    let Ok(bytes) = fs::read(&path) else { return Settings::default() };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(strip_bom(&bytes)) else {
        // Never wipe silently: keep the broken file next to the real one.
        let _ = fs::copy(&path, path.with_extension("corrupt.json"));
        return Settings::default();
    };
    from_value(value)
}

fn from_value(value: serde_json::Value) -> Settings {
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(version) if version >= SCHEMA as u64 => {
            serde_json::from_value(value).unwrap_or_default()
        }
        // Schema 1 kept one flat set of choices for the single installation that
        // existed; move them into the scoped shape instead of dropping them, then
        // write once so the file stops being older than the code.
        _ => {
            let migrated = migrate_flat(&value);
            let _ = save(&migrated);
            migrated
        }
    }
}

fn migrate_flat(value: &serde_json::Value) -> Settings {
    let text = |key: &str| {
        value.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_string()
    };
    let mut settings = Settings::default();
    settings.shared.models_dir = text("models_dir");
    settings.shared.huggingface_token_hint = text("token_hint");
    settings.shared.close_to_tray = value
        .get("close_to_tray")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let installation = Installation {
        data_dir: text("data_dir"),
        setup_confirmed: value
            .get("setup_complete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    };
    settings.installations.insert(crate::catalog::INSTALL_ID.to_string(), installation);
    settings
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let path = settings_path()?;
    let parent = path.parent().ok_or("设置路径无效")?;
    fs::create_dir_all(parent).map_err(|e| format!("无法创建设置目录：{e}"))?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    // Write then rename so an interrupted save cannot truncate the previous file.
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, bytes).map_err(|e| format!("无法写入设置：{e}"))?;
    fs::rename(&temp, &path).map_err(|e| format!("无法保存设置：{e}"))
}

/// Launcher-owned settings, shared with the Tauri commands.
#[derive(Default)]
pub struct SettingsState(pub Mutex<Settings>);

impl SettingsState {
    pub fn snapshot(&self) -> Settings {
        self.0.lock().map(|value| value.clone()).unwrap_or_else(|_| Settings::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_order_mark_does_not_reset_the_settings() {
        let text = r#"{
            "schema": 3,
            "shared": { "models_dir": "E:\\Weights", "install_dir": "F:\\tmp" },
            "installations": { "free": { "data_dir": "F:\\tmp\\data", "setup_confirmed": true } }
        }"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(text.as_bytes());
        let value = serde_json::from_slice::<serde_json::Value>(strip_bom(&bytes)).unwrap();
        let settings = from_value(value);
        assert_eq!(settings.shared.install_dir, r"F:\tmp");
        assert_eq!(settings.shared.models_dir, r"E:\Weights");
        assert!(settings.installation("free").setup_confirmed);
    }

    #[test]
    fn empty_choices_fall_back_to_the_root_defaults() {
        let settings = Settings::default();
        let root = Path::new(r"C:\AtelierFree");
        assert_eq!(settings.data_path(root, "free"), root.join("data"));
        assert_eq!(settings.models_path(root), root.join("models"));
    }

    /// 安装根搬走/旧目录被删之后，data_dir 悬空会让启动被拒（"发行包目录里已经有数据"）。
    #[test]
    fn a_data_dir_pointing_at_a_deleted_root_falls_back_to_the_new_root() {
        let base = std::env::temp_dir().join(format!("atelier-heal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("new");
        fs::create_dir_all(root.join("data")).expect("data dir");
        fs::write(root.join("data").join("atelier_kernel.db"), b"x").expect("db");

        let mut settings = Settings::default();
        let set = |settings: &mut Settings, path: &Path| {
            settings.installation_mut(crate::catalog::INSTALL_ID).data_dir =
                path.display().to_string();
        };

        // 1) 旧路径已经不存在
        set(&mut settings, &base.join("gone").join("data"));
        assert!(heal_data_dir(&mut settings, &root), "悬空的旧路径要被清掉");
        assert_eq!(settings.installation(crate::catalog::INSTALL_ID).data_dir, "");

        // 2) 旧路径还在，但已经被删空了（用户手工清理到一半就是这样）
        let empty = base.join("emptied");
        fs::create_dir_all(&empty).expect("empty dir");
        set(&mut settings, &empty);
        assert!(heal_data_dir(&mut settings, &root), "空目录同样要退回去");
        assert_eq!(settings.installation(crate::catalog::INSTALL_ID).data_dir, "");

        // 3) 那个目录里确实有数据：尊重用户的选择
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("elsewhere");
        fs::write(elsewhere.join("atelier_kernel.db"), b"y").expect("db");
        set(&mut settings, &elsewhere);
        assert!(!heal_data_dir(&mut settings, &root), "有数据的目录不能动");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn choices_are_scoped_to_their_owner() {
        let mut settings = Settings::default();
        settings.shared.models_dir = r"E:\Weights".into();
        settings.installation_mut("free").data_dir = r"D:\Atelier\Data".into();
        let root = Path::new(r"C:\AtelierFree");
        assert_eq!(settings.data_path(root, "free"), PathBuf::from(r"D:\Atelier\Data"));
        assert_eq!(settings.data_path(root, "auto"), root.join("data"));
        assert_eq!(settings.models_path(root), PathBuf::from(r"E:\Weights"));
    }

    #[test]
    fn a_relative_choice_never_overrides_the_root() {
        let mut settings = Settings::default();
        settings.shared.models_dir = "weights".into();
        settings.installation_mut("free").data_dir = "data".into();
        let root = Path::new(r"C:\AtelierFree");
        assert_eq!(settings.data_path(root, "free"), root.join("data"));
        assert_eq!(settings.models_path(root), root.join("models"));
    }

    #[test]
    fn a_flat_schema_one_file_is_migrated_not_dropped() {
        let legacy = serde_json::json!({
            "setup_complete": true,
            "data_dir": r"D:\Atelier\Data",
            "models_dir": r"E:\Weights",
            "token_hint": "hf_••••1234",
            "close_to_tray": false
        });
        let settings = migrate_flat(&legacy);
        assert_eq!(settings.schema, SCHEMA);
        assert_eq!(settings.shared.models_dir, r"E:\Weights");
        assert_eq!(settings.shared.huggingface_token_hint, "hf_••••1234");
        assert!(!settings.shared.close_to_tray);
        let installation = settings.installation(crate::catalog::INSTALL_ID);
        assert_eq!(installation.data_dir, r"D:\Atelier\Data");
        assert!(installation.setup_confirmed);
    }
}
