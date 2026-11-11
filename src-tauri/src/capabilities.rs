//! The runtime picture the launcher's main page renders.
//!
//! One call answers "what can this installation do right now": which capabilities
//! exist, which are installed, which the user switched off, which are actually
//! running. Installed-ness comes from the distribution (`kernel_boot.json`);
//! running-ness comes from the kernel's own `/api/assembly`. Nothing is inferred
//! from the catalog except presentation and the paid flag.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    catalog,
    settings::{self, SettingsState},
    variant_runtime::{install_dir, live_port},
};
use tauri::State;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainView {
    pub id: String,
    pub label: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub paid: bool,
    pub always_on: bool,
    /// Declared by the installed distribution. A paid domain that is not installed
    /// is the purchasable one.
    pub installed: bool,
    /// Entitlement hook; free domains are always licensed.
    pub licensed: bool,
    /// `None` while the kernel is not running, so the UI can say "not running".
    pub running: Option<bool>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineView {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Present as a plugin package under `pipeline-plugins/`.
    pub installed: bool,
    pub paid: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub install_id: String,
    pub installed: bool,
    pub version: String,
    pub install_dir: String,
    pub port: Option<u16>,
    pub domains: Vec<DomainView>,
    pub pipelines: Vec<PipelineView>,
    /// Sellable entries that come from the remote catalog; empty when unavailable.
    pub catalog_reachable: bool,
}

fn declared_domains(config: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(config) else { return Vec::new() };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { return Vec::new() };
    value
        .get("domains")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("domain_id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Domain ids the running kernel reports, with their stopped flag.
fn live_states(port: u16) -> Vec<(String, bool)> {
    let Ok(client) = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
    else {
        return Vec::new();
    };
    let url = format!("http://127.0.0.1:{port}{}", crate::contract::assembly_path());
    let Ok(response) = client.get(&url).send() else { return Vec::new() };
    let Ok(body) = response.json::<Value>() else { return Vec::new() };
    body.get("domains")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("domain_id").and_then(Value::as_str)?;
                    let status = row.get("status").and_then(Value::as_str).unwrap_or_default();
                    let stopped = row
                        .pointer("/process/is_stopped")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let running = !stopped && status.eq_ignore_ascii_case("active");
                    Some((id.to_string(), running))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pipeline plugin ids present under `<root>/pipeline-plugins`.
fn installed_pipelines(root: &Path) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(root.join("pipeline-plugins")) else { return BTreeSet::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let manifest = entry.path().join("manifest.json");
            let bytes = fs::read(manifest).ok()?;
            let value = serde_json::from_slice::<Value>(&bytes).ok()?;
            value.get("id").and_then(Value::as_str).map(str::to_string)
        })
        .collect()
}

fn version_of(root: &Path) -> String {
    fs::read_to_string(root.join("VERSION")).map(|text| text.trim().to_string()).unwrap_or_default()
}

pub fn gather(settings: &settings::Settings) -> Result<Capabilities, String> {
    let root = install_dir()?;
    let install_id = catalog::INSTALL_ID;
    let installed = root.join("atelier-kernel.exe").is_file() && root.join("start.ps1").is_file();
    let declared: BTreeSet<String> = declared_domains(&root.join("kernel_boot.json")).into_iter().collect();
    let port = if installed { live_port(&root)? } else { None };
    let live = port.map(|port| live_states(port)).unwrap_or_default();

    let domains = catalog::DOMAINS
        .iter()
        .map(|meta| {
            let is_installed = declared.iter().any(|id| id.eq_ignore_ascii_case(meta.id));
            let running = if !is_installed {
                None
            } else {
                match port {
                    Some(_) => Some(
                        live.iter()
                            .find(|(id, _)| id.eq_ignore_ascii_case(meta.id))
                            .map(|(_, running)| *running)
                            .unwrap_or(false),
                    ),
                    None => None,
                }
            };
            DomainView {
                id: meta.id.to_string(),
                label: meta.label.to_string(),
                title: meta.title.to_string(),
                description: meta.description.to_string(),
                icon: meta.icon.to_string(),
                paid: meta.paid,
                always_on: meta.always_on,
                installed: is_installed,
                licensed: catalog::licensed(meta.id),
                running,
            }
        })
        .collect();

    let present = installed_pipelines(&root);
    let pipelines = present
        .iter()
        .map(|id| {
            let (label, description) = catalog::pipeline_label(id)
                .map(|(label, description)| (label.to_string(), description.to_string()))
                .unwrap_or_else(|| (id.clone(), String::new()));
            PipelineView { id: id.clone(), label, description, installed: true, paid: false }
        })
        .collect();

    Ok(Capabilities {
        install_id: install_id.to_string(),
        installed,
        version: version_of(&root),
        install_dir: root.display().to_string(),
        port,
        domains,
        pipelines,
        catalog_reachable: !settings.shared.catalog_url.trim().is_empty(),
    })
}

#[tauri::command]
pub async fn capabilities(state: State<'_, SettingsState>) -> Result<Capabilities, String> {
    let settings = state.snapshot();
    tauri::async_runtime::spawn_blocking(move || gather(&settings))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedView {
    /// What the user chose; empty means "the installation default".
    pub models_dir: String,
    /// Where weights actually land right now.
    pub models_dir_effective: String,
    pub ollama_host: String,
    pub token_hint: Option<String>,
    /// True while an instance is running, so the UI can promise hot apply.
    pub live: bool,
}

#[tauri::command]
pub async fn shared_settings(state: State<'_, SettingsState>) -> Result<SharedView, String> {
    let root = install_dir()?;
    let settings = state.snapshot();
    Ok(SharedView {
        models_dir: settings.shared.models_dir.clone(),
        models_dir_effective: settings.models_path(&root).display().to_string(),
        ollama_host: settings.shared.ollama_host.clone(),
        token_hint: crate::credentials::hint(),
        live: live_port(&root)?.is_some(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    /// Whether the running instance already picked the values up.
    pub applied: bool,
    pub note: String,
}

/// Save the machine-level values and, when possible, push them into the running
/// instance.
///
/// The kernel's environment cannot change in place, but the engines domain keeps
/// its own settings and reads them on every call — that is the hot path, and it is
/// exactly how the distribution itself describes the behaviour ("保存后下一次调用
/// 生效").
#[tauri::command]
pub async fn save_shared(
    state: State<'_, SettingsState>,
    models_dir: String,
    ollama_host: String,
    token: Option<String>,
) -> Result<ApplyResult, String> {
    let root = install_dir()?;
    let models = models_dir.trim();
    if !models.is_empty() {
        let path = PathBuf::from(models);
        if !path.is_absolute() {
            return Err("权重目录必须是绝对路径。".into());
        }
        let check = crate::locations::inspect("models", &path);
        if let Some(blocker) = check.blockers.first() {
            return Err(format!("权重目录不可用：{blocker}"));
        }
        fs::create_dir_all(&path).map_err(|e| format!("无法创建权重目录：{e}"))?;
    }

    let mut settings = state.snapshot();
    settings.shared.models_dir = models.to_string();
    settings.shared.ollama_host = ollama_host.trim().to_string();
    if let Some(token) = token.filter(|value| !value.trim().is_empty()) {
        crate::credentials::store(&token)?;
        settings.shared.huggingface_token_hint = crate::credentials::hint().unwrap_or_default();
    }
    settings::save(&settings)?;
    *state.0.lock().map_err(|e| e.to_string())? = settings;

    let Some(port) = live_port(&root)? else {
        return Ok(ApplyResult { applied: false, note: "已保存。实例启动后生效。".into() });
    };
    let mut patch = serde_json::Map::new();
    if !models.is_empty() {
        patch.insert("models_dir".into(), Value::String(models.to_string()));
    }
    if !ollama_host.trim().is_empty() {
        patch.insert("ollama_host".into(), Value::String(ollama_host.trim().to_string()));
    }
    if let Some(token) = crate::credentials::load() {
        patch.insert("huggingface_token".into(), Value::String(token));
    }
    if patch.is_empty() {
        return Ok(ApplyResult { applied: true, note: "没有需要同步的改动。".into() });
    }
    match push_settings(port, Value::Object(patch)) {
        Ok(()) => Ok(ApplyResult { applied: true, note: "已同步到运行中的实例，下一次引擎调用生效。".into() }),
        Err(err) => Ok(ApplyResult { applied: false, note: format!("已保存，但同步失败：{err}。重启后生效。") }),
    }
}

fn push_settings(port: u16, patch: Value) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(20))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/settings"))
        .json(&serde_json::json!({ "domain": "engines", "section_id": "general", "patch": patch }))
        .send()
        .map_err(|e| format!("无法联系内核：{e}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    let detail = response.text().unwrap_or_default();
    Err(detail.trim().to_string())
}
