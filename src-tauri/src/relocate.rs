//! 安装位置 / 数据目录的迁移。
//!
//! 换位置的难点不在复制，而在**绝对路径**。发行包里至少三处写死了根路径，搬完不
//! 重写就是一堆坏掉的环境：
//!
//! 1. `.venvs/<env>/pyvenv.cfg` 的 `home = <root>\.venvs\.python\...`（uv 建的 venv）
//! 2. `.venvs/engine-environments.json` 里每个环境的 `python` / `project` / `tool_paths`
//! 3. `.venvs/<env>/Scripts/*.exe`：控制台脚本把解释器路径编进了 PE 文件 —— 改不了，
//!    只能把该环境从账本里删掉，等下次用到时重建
//!
//! 另外 `<root>/data` 可能是指向别的盘的目录联接：复制时必须跳过 reparse point，
//! 否则会顺着联接把几十 GB 权重也复制一遍。

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{
    installer::Progress,
    locations,
    settings::{self, SettingsState},
    variant_runtime,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 当前生效的位置，给设置页展示用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationsView {
    pub install: String,
    pub install_default: String,
    pub data: String,
    pub data_linked: bool,
    pub data_target: Option<String>,
    pub models: String,
    pub free_gb: Option<f64>,
}

/// 迁移结果，字段都是给界面看的人话。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelocateReport {
    pub target: String,
    pub copied_files: u64,
    pub copied_bytes: u64,
    /// 被跳过、随后原样重建的目录联接（通常是 `data/`）。
    pub kept_links: Vec<String>,
    /// 重写了 `home =` 的 venv 数量。
    pub rewritten_venvs: u64,
    /// 自检不过、已从账本移除的环境：下次用到时会重建。
    pub rebuild: Vec<String>,
    pub removed_old: bool,
    pub message: String,
}

pub fn view(app: &AppHandle) -> Result<LocationsView, String> {
    let root = variant_runtime::install_dir()?;
    let settings = app
        .try_state::<SettingsState>()
        .map(|state| state.snapshot())
        .unwrap_or_else(settings::load);
    let data = settings.data_path(&root, crate::catalog::INSTALL_ID);
    let root_data = root.join(crate::contract::DATA);
    let linked = locations::is_link(&root_data);
    let target = fs::canonicalize(&root_data).ok().map(|path| path.display().to_string());
    Ok(LocationsView {
        install: root.display().to_string(),
        install_default: variant_runtime::default_install_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        data: data.display().to_string(),
        data_linked: linked,
        data_target: if linked { target } else { None },
        models: settings.models_path(&root).display().to_string(),
        free_gb: locations::disk_space(&root).map(|space| (space.free_gb * 10.0).round() / 10.0),
    })
}

#[tauri::command]
pub async fn locations_view(app: AppHandle) -> Result<LocationsView, String> {
    tauri::async_runtime::spawn_blocking(move || view(&app))
        .await
        .map_err(|error| error.to_string())?
}

/// 换安装位置。`mode`：`move` 搬过去，`reinstall` 在新位置重新下载装配。
#[tauri::command]
pub async fn relocate_install(
    app: AppHandle,
    target: String,
    mode: String,
) -> Result<RelocateReport, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || match mode.as_str() {
        "move" => move_install(&app_for_task, Path::new(target.trim())),
        "reinstall" => reinstall_install(&app_for_task, Path::new(target.trim())),
        other => Err(format!("未知的迁移方式：{other}")),
    })
    .await
    .map_err(|error| error.to_string())?
    .inspect_err(|error| finish_failed(&app, error))
    .map(|report| {
        finish_done(&app, &report.message);
        report
    })
}

/// 换数据目录：把数据搬到新位置，再把 `<root>/data` 指过去。
#[tauri::command]
pub async fn relocate_data(app: AppHandle, target: String) -> Result<RelocateReport, String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || move_data(&app_for_task, Path::new(target.trim())))
        .await
        .map_err(|error| error.to_string())?
        .inspect_err(|error| finish_failed(&app, error))
        .map(|report| {
            finish_done(&app, &report.message);
            report
        })
}

fn emit(app: &AppHandle, phase: &'static str, label: impl Into<String>, detail: impl Into<String>) {
    let detail = detail.into();
    let mut progress = Progress::phase(phase, label);
    progress = progress.with_detail(detail.clone());
    progress = progress.log(detail);
    crate::installer::emit_progress(app, progress);
}

fn finish_done(app: &AppHandle, message: &str) {
    let mut progress = Progress::phase("done", message.to_string());
    progress = progress.with_detail(message.to_string());
    crate::installer::emit_progress(app, progress);
}

fn finish_failed(app: &AppHandle, message: &str) {
    let mut progress = Progress::phase("fail", message.to_string());
    progress = progress.with_detail(message.to_string());
    crate::installer::emit_progress(app, progress);
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |path: &Path| {
        std::path::absolute(path)
            .map(|path| {
                path.as_os_str()
                    .to_string_lossy()
                    .replace('/', "\\")
                    .trim_end_matches('\\')
                    .to_lowercase()
            })
            .unwrap_or_else(|_| path.as_os_str().to_string_lossy().to_lowercase())
    };
    norm(a) == norm(b)
}

/// 目标目录必须：绝对路径、不和当前根重叠、互不包含、可写。
fn check_target(kind: &str, current: &Path, target: &Path) -> Result<(), String> {
    if !target.is_absolute() {
        return Err(format!("{kind}必须是绝对路径。"));
    }
    if same_path(current, target) {
        return Err(format!("{kind}和现在的位置一样，不用改。"));
    }
    if target.starts_with(current) || current.starts_with(target) {
        return Err(format!("{kind}不能和现在的位置互相包含，请选一个独立的目录。"));
    }
    let check = locations::inspect(if kind == "数据目录" { "data" } else { "install" }, target);
    if let Some(blocker) = check.blockers.first() {
        return Err(format!("{kind}不可用：{blocker}"));
    }
    fs::create_dir_all(target).map_err(|error| format!("无法创建{kind}：{error}"))?;
    // 迁移是整体搬家：目标位置混着别的东西会两边都乱。
    if fs::read_dir(target).map(|mut entries| entries.next().is_some()).unwrap_or(false) {
        return Err(format!("{kind}不是空目录，请选一个空目录或新建一个。"));
    }
    Ok(())
}

/// 复制整棵树。目录联接只记录不跟随，返回 `(相对路径, 指向)` 供事后原样重建。
fn copy_tree(
    src: &Path,
    dst: &Path,
    base: &Path,
    report: &mut RelocateReport,
    links: &mut Vec<(PathBuf, PathBuf)>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|error| format!("无法创建 {}：{error}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|error| format!("无法读取 {}：{error}", src.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
        if file_type.is_symlink() || locations::is_link(&path) {
            // 认不出来的联接也按联接处理：跳过比顺着它复制安全。
            let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            links.push((relative, fs::read_link(&path).unwrap_or_default()));
            report.kept_links.push(entry.file_name().to_string_lossy().to_string());
            continue;
        }
        if file_type.is_dir() {
            copy_tree(&path, &target, base, report, links, progress)?;
            continue;
        }
        fs::copy(&path, &target).map_err(|error| format!("无法复制 {}：{error}", path.display()))?;
        report.copied_files += 1;
        report.copied_bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        progress(report.copied_files, report.copied_bytes);
    }
    Ok(())
}

fn recreate_links(new_root: &Path, old_root: &Path, links: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    for (relative, target) in links {
        // 指向旧根内部就跟着搬；指向外面（另一块盘的数据目录）保持原样。
        let target = match target.strip_prefix(old_root) {
            Ok(rest) => new_root.join(rest),
            Err(_) => target.clone(),
        };
        let link = new_root.join(relative);
        if locations::is_link(&link) {
            continue;
        }
        locations::create_junction(&link, &target)?;
    }
    Ok(())
}

/// 引擎环境的 `.venvs`：发布包在 `engine-root/` 下，开发树直接在根下。
fn venv_roots(root: &Path) -> Vec<PathBuf> {
    ["engine-root\\.venvs", ".venvs"]
        .iter()
        .map(|relative| root.join(relative))
        .filter(|path| path.is_dir())
        .collect()
}

fn rewrite_pyvenv(cfg: &Path, old_root: &Path, new_root: &Path) -> Result<bool, String> {
    let Ok(text) = fs::read_to_string(cfg) else { return Ok(false) };
    let mut changed = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("home = ") {
            if let Ok(rest) = PathBuf::from(value.trim()).strip_prefix(old_root) {
                let rewritten = format!("home = {}", new_root.join(rest).display());
                changed |= rewritten != line;
                lines.push(rewritten);
                continue;
            }
        }
        lines.push(line.to_string());
    }
    if changed {
        let mut out = lines.join("\n");
        out.push('\n');
        fs::write(cfg, out).map_err(|error| format!("无法重写 {}：{error}", cfg.display()))?;
    }
    Ok(changed)
}

fn rewrite_ledger(ledger: &Path, old_root: &Path, new_root: &Path) -> Result<bool, String> {
    let Ok(bytes) = fs::read(ledger) else { return Ok(false) };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(false);
    };
    let old = old_root.display().to_string();
    let new = new_root.display().to_string();
    if !replace_in_value(&mut value, &old, &new) {
        return Ok(false);
    }
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    fs::write(ledger, text).map_err(|error| format!("无法重写 {}：{error}", ledger.display()))?;
    Ok(true)
}

/// 目录里所有 `pyvenv.cfg` 与账本里的绝对路径一起换前缀。
fn rewrite_environment_paths(root: &Path, old_root: &Path, report: &mut RelocateReport) -> Result<(), String> {
    for venvs in venv_roots(root) {
        let Ok(entries) = fs::read_dir(&venvs) else { continue };
        for entry in entries.flatten() {
            let directory = entry.path();
            if !directory.is_dir() {
                continue;
            }
            let cfg = directory.join("pyvenv.cfg");
            if cfg.is_file() && rewrite_pyvenv(&cfg, old_root, root)? {
                report.rewritten_venvs += 1;
            }
            // `.engine-builds/<env>-<platform>-<hash>/` 这一层
            if let Ok(inner) = fs::read_dir(&directory) {
                for child in inner.flatten() {
                    let cfg = child.path().join("pyvenv.cfg");
                    if cfg.is_file() && rewrite_pyvenv(&cfg, old_root, root)? {
                        report.rewritten_venvs += 1;
                    }
                }
            }
        }
        let ledger = venvs.join("engine-environments.json");
        if ledger.is_file() {
            rewrite_ledger(&ledger, old_root, root)?;
        }
    }
    Ok(())
}

fn replace_in_value(value: &mut serde_json::Value, old: &str, new: &str) -> bool {
    match value {
        serde_json::Value::String(text) => match replace_ignore_ascii_case(text, old, new) {
            Some(replaced) => {
                *text = replaced;
                true
            }
            None => false,
        },
        serde_json::Value::Array(items) => items.iter_mut().fold(false, |hit, item| {
            let changed = replace_in_value(item, old, new);
            hit || changed
        }),
        serde_json::Value::Object(map) => map.values_mut().fold(false, |hit, item| {
            let changed = replace_in_value(item, old, new);
            hit || changed
        }),
        _ => false,
    }
}

/// 只在两边都是 ASCII 时做大小写不敏感替换；带中文的路径退回精确替换，不做编码冒险。
fn replace_ignore_ascii_case(text: &str, needle: &str, replacement: &str) -> Option<String> {
    if needle.is_empty() {
        return None;
    }
    if !(text.is_ascii() && needle.is_ascii()) {
        return text.contains(needle).then(|| text.replace(needle, replacement));
    }
    let hay = text.to_ascii_lowercase();
    let low = needle.to_ascii_lowercase();
    if !hay.contains(&low) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut start = 0usize;
    while let Some(index) = hay[start..].find(&low) {
        let at = start + index;
        out.push_str(&text[start..at]);
        out.push_str(replacement);
        start = at + needle.len();
    }
    out.push_str(&text[start..]);
    Some(out)
}

/// 自检每个已构建环境：改过 `pyvenv.cfg` 之后 python 还能不能认出自己、控制台脚本里
/// 还有没有残留的旧路径。不过关的从账本里删掉 —— 下次用到时按锁文件重建。
fn verify_environments(roots: &[PathBuf], old_root: &Path) -> Result<Vec<String>, String> {
    let mut rebuild = Vec::new();
    for directory in roots {
        let ledger = directory.join("engine-environments.json");
        let Ok(bytes) = fs::read(&ledger) else { continue };
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        let Some(entries) = value.as_object_mut() else { continue };
        let keys: Vec<String> = entries.keys().cloned().collect();
        let mut changed = false;
        for key in keys {
            let Some(entry) = entries.get(&key) else { continue };
            let python = entry
                .get("python")
                .and_then(|item| item.as_str())
                .unwrap_or_default()
                .to_string();
            let python = PathBuf::from(python);
            let healthy = python.is_file()
                && prefix_of(&python).is_some()
                && !shim_has_old_root(&python, old_root);
            if !healthy {
                entries.remove(&key);
                rebuild.push(key);
                changed = true;
            }
        }
        if changed {
            let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
            fs::write(&ledger, text).map_err(|error| format!("无法更新 {}：{error}", ledger.display()))?;
        }
    }
    Ok(rebuild)
}

/// `Scripts/*.exe` 里是否编着旧的解释器路径（uv/pip 的控制台脚本就是这样）。
fn shim_has_old_root(python: &Path, old_root: &Path) -> bool {
    let Some(env) = python.parent().and_then(|scripts| scripts.parent()) else { return false };
    let scripts = env.join("Scripts");
    let Ok(entries) = fs::read_dir(&scripts) else { return false };
    let old = old_root.display().to_string();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().map(|ext| ext.eq_ignore_ascii_case("exe")).unwrap_or(false) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        if contains_ascii(&bytes, old.as_bytes()) || contains_utf16(&bytes, &old) {
            return true;
        }
    }
    false
}

fn contains_ascii(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn contains_utf16(haystack: &[u8], needle: &str) -> bool {
    let encoded: Vec<u8> = needle.encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect();
    contains_ascii(haystack, &encoded)
}

/// 解释器自报的 `sys.prefix`；失败说明 venv 已经坏掉。
fn prefix_of(python: &Path) -> Option<String> {
    let mut command = Command::new(python);
    command.args(["-c", "import sys;print(sys.prefix)"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 只删真实文件与目录；遇到联接只删链接本身，绝不顺着它往下删。
fn remove_tree(path: &Path) -> Result<(), String> {
    if !path.exists() && !locations::is_link(path) {
        return Ok(());
    }
    if locations::is_link(path) {
        return fs::remove_dir(path).map_err(|error| format!("无法删除联接 {}：{error}", path.display()));
    }
    for entry in fs::read_dir(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?
        .flatten()
    {
        let child = entry.path();
        if locations::is_link(&child) {
            fs::remove_dir(&child).map_err(|error| format!("无法删除联接 {}：{error}", child.display()))?;
        } else if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            remove_tree(&child)?;
        } else {
            fs::remove_file(&child).map_err(|error| format!("无法删除 {}：{error}", child.display()))?;
        }
    }
    fs::remove_dir(path).map_err(|error| format!("无法删除 {}：{error}", path.display()))
}

fn store_install_dir(app: &AppHandle, target: &Path) -> Result<(), String> {
    let mut settings = app
        .try_state::<SettingsState>()
        .map(|state| state.snapshot())
        .unwrap_or_else(settings::load);
    settings.shared.install_dir = target.display().to_string();
    settings::save(&settings)?;
    if let Some(state) = app.try_state::<SettingsState>() {
        *state.0.lock().map_err(|error| error.to_string())? = settings;
    }
    Ok(())
}

/// 一次真正的搬家：复制 → 补联接 → 重写环境路径 → 自检 → 确认新位置能跑。
///
/// 只碰文件系统，不改设置、不删旧目录 —— 调用方在它成功之后才动旧位置，
/// 这样任何一步失败都能原地退回。测试也直接打这一层。
fn relocate_tree(root: &Path, target: &Path, emit_progress: &mut dyn FnMut(&'static str, String, String)) -> Result<RelocateReport, String> {
    let mut report = RelocateReport { target: target.display().to_string(), ..Default::default() };
    let mut links = Vec::new();
    emit_progress("copy", "正在迁移安装目录".to_string(),
        format!("{} → {}", root.display(), target.display()));
    let mut progress = |files: u64, bytes: u64| {
        if files % 500 == 0 {
            emit_progress("copy", "正在迁移安装目录".to_string(),
                format!("{files} 个文件 · {:.1} GB", bytes as f64 / 1e9));
        }
    };
    if let Err(error) = copy_tree(root, target, root, &mut report, &mut links, &mut progress) {
        let _ = remove_tree(target);
        return Err(format!("迁移中断，旧安装没有改动：{error}"));
    }

    recreate_links(target, root, &links)?;
    rewrite_environment_paths(target, root, &mut report)?;
    report.rebuild = verify_environments(&venv_roots(target), root)?;

    // 新位置至少要有一个可执行的内核，否则宁可回滚也不动旧目录。
    if !target.join("atelier-kernel.exe").is_file() {
        let _ = remove_tree(target);
        return Err("迁移后的目录里没有 atelier-kernel.exe，已放弃本次迁移（旧安装未改动）。".into());
    }
    Ok(report)
}

fn move_install(app: &AppHandle, target: &Path) -> Result<RelocateReport, String> {
    let root = variant_runtime::install_dir()?;
    check_target("安装位置", &root, target)?;
    if std::env::current_exe().map(|exe| exe.starts_with(&root)).unwrap_or(false) {
        return Err("启动器自己就在安装目录里，先把它挪出去再迁移。".into());
    }
    emit(app, "relocate", "正在停止实例", root.display().to_string());
    let _ = variant_runtime::stop_sync();

    let mut report = relocate_tree(&root, target, &mut |phase, label, detail| {
        emit(app, phase, label, detail);
    })?;

    store_install_dir(app, target)?;
    emit(app, "relocate", "正在清理旧位置", root.display().to_string());
    report.removed_old = remove_tree(&root).is_ok();
    report.message = format!(
        "已迁移到 {}：{} 个文件 / {:.1} GB{}{}",
        target.display(),
        report.copied_files,
        report.copied_bytes as f64 / 1e9,
        if report.rebuild.is_empty() {
            String::new()
        } else {
            format!("；{} 个环境下次用到时会重建", report.rebuild.len())
        },
        if report.removed_old { "，旧目录已清空" } else { "，旧目录没删干净" });
    Ok(report)
}

fn reinstall_install(app: &AppHandle, target: &Path) -> Result<RelocateReport, String> {
    let root = variant_runtime::install_dir()?;
    check_target("安装位置", &root, target)?;
    emit(app, "relocate", "正在停止实例", root.display().to_string());
    let _ = variant_runtime::stop_sync();
    store_install_dir(app, target)?;
    emit(app, "relocate", "正在新位置重新安装", target.display().to_string());
    let mut report = RelocateReport { target: target.display().to_string(), ..Default::default() };
    if let Err(error) = crate::installer::run_with(app, false) {
        // 装失败就退回原位置：设置已经改过去，不回去会打不开。
        let _ = store_install_dir(app, &root);
        return Err(format!("新位置安装失败，已退回原位置：{error}"));
    }
    report.removed_old = remove_tree(&root).is_ok();
    report.message = format!(
        "已在 {} 重新安装{}",
        target.display(),
        if report.removed_old { "，旧目录已清空" } else { "，旧目录没删干净" });
    Ok(report)
}

fn move_data(app: &AppHandle, target: &Path) -> Result<RelocateReport, String> {
    let root = variant_runtime::install_dir()?;
    let settings_now = app
        .try_state::<SettingsState>()
        .map(|state| state.snapshot())
        .unwrap_or_else(settings::load);
    let current = settings_now.data_path(&root, crate::catalog::INSTALL_ID);
    check_target("数据目录", &current, target)?;
    emit(app, "relocate", "正在停止实例", root.display().to_string());
    let _ = variant_runtime::stop_sync();

    let root_data = root.join(crate::contract::DATA);
    let mut report = RelocateReport { target: target.display().to_string(), ..Default::default() };
    let mut links = Vec::new();
    emit(app, "copy", "正在迁移数据目录", format!("{} → {}", current.display(), target.display()));
    let mut progress = |files: u64, bytes: u64| {
        if files % 500 == 0 {
            emit(app, "copy", "正在迁移数据目录",
                format!("{files} 个文件 · {:.1} GB", bytes as f64 / 1e9));
        }
    };
    if let Err(error) = copy_tree(&current, target, &current, &mut report, &mut links, &mut progress) {
        let _ = remove_tree(target);
        return Err(format!("迁移中断，数据没有改动：{error}"));
    }

    // 先把旧目录挪开（同盘改名，可回滚），再建联接。
    let backup = root.join(".data-previous");
    let _ = remove_tree(&backup);
    let moved_aside = if locations::is_link(&root_data) {
        fs::remove_dir(&root_data).map_err(|error| format!("无法先断开旧的数据联接：{error}"))?;
        false
    } else if root_data.is_dir() {
        fs::rename(&root_data, &backup).map_err(|error| format!("无法暂存旧数据目录：{error}"))?;
        true
    } else {
        false
    };
    if let Err(error) = locations::create_junction(&root_data, target) {
        // 联接建不出来就退回旧目录，数据一个字节都没动。
        if moved_aside {
            let _ = fs::rename(&backup, &root_data);
        }
        let _ = remove_tree(target);
        return Err(format!("无法把数据目录指向新位置：{error}"));
    }

    let mut settings = settings_now.clone();
    settings.installation_mut(crate::catalog::INSTALL_ID).data_dir = target.display().to_string();
    settings::save(&settings)?;
    if let Some(state) = app.try_state::<SettingsState>() {
        *state.0.lock().map_err(|error| error.to_string())? = settings;
    }
    let cleaned = remove_tree(&backup).is_ok() && !backup.exists();
    report.removed_old = cleaned;
    report.message = format!(
        "数据已迁到 {}：{} 个文件 / {:.1} GB{}",
        target.display(),
        report.copied_files,
        report.copied_bytes as f64 / 1e9,
        if report.removed_old { "，旧目录已清空" } else { "，旧目录没删干净" });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("atelier-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("scratch");
        base
    }

    fn copy_without_progress(
        src: &Path,
        dst: &Path,
        report: &mut RelocateReport,
        links: &mut Vec<(PathBuf, PathBuf)>,
    ) -> Result<(), String> {
        let mut idle = |_: u64, _: u64| {};
        copy_tree(src, dst, src, report, links, &mut idle)
    }

    #[test]
    fn path_replacement_is_case_insensitive_for_ascii() {
        assert_eq!(
            replace_ignore_ascii_case(r"G:\Atelier\app", r"g:\atelier\app", r"E:\Atelier\app").as_deref(),
            Some(r"E:\Atelier\app"));
        assert_eq!(replace_ignore_ascii_case("no path here", r"G:\Atelier", "x"), None);
        assert_eq!(
            replace_ignore_ascii_case(r"G:\Atelier\权重", r"G:\Atelier", r"E:\Atelier").as_deref(),
            Some(r"E:\Atelier\权重"));
    }

    #[test]
    fn pyvenv_home_follows_the_new_root() {
        let base = scratch("pyvenv");
        let env = base.join("env");
        fs::create_dir_all(&env).expect("env dir");
        let cfg = env.join("pyvenv.cfg");
        fs::write(&cfg, "home = G:\\Old\\engine-root\\.venvs\\.python\\cpython-3.11.15\nuv = 0.11.19\n")
            .expect("write cfg");
        assert!(rewrite_pyvenv(&cfg, Path::new(r"G:\Old"), Path::new(r"E:\New")).expect("rewrite"));
        let text = fs::read_to_string(&cfg).expect("read");
        assert!(text.contains(r"home = E:\New\engine-root\.venvs\.python\cpython-3.11.15"), "{text}");
        assert!(text.contains("uv = 0.11.19"));
        assert!(!rewrite_pyvenv(&cfg, Path::new(r"G:\Old"), Path::new(r"E:\New")).expect("rewrite"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ledger_entries_move_with_the_root() {
        let base = scratch("ledger");
        let ledger = base.join("engine-environments.json");
        fs::write(&ledger, serde_json::json!({
            "hf4-media/windows-x86_64": {
                "python": r"G:\Old\engine-root\.venvs\hf4-media\Scripts\python.exe",
                "project": r"G:\Old\engine-root\domains\engines\environments\hf4-media\windows-x86_64",
                "tool_paths": [r"G:\Old\engine-root\.venvs\.assets\ffmpeg"]
            }
        }).to_string()).expect("write ledger");
        assert!(rewrite_ledger(&ledger, Path::new(r"G:\Old"), Path::new(r"E:\New")).expect("rewrite"));
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&ledger).expect("read")).expect("json");
        let entry = &value["hf4-media/windows-x86_64"];
        assert_eq!(
            entry["python"].as_str(),
            Some(r"E:\New\engine-root\.venvs\hf4-media\Scripts\python.exe"));
        assert_eq!(
            entry["project"].as_str(),
            Some(r"E:\New\engine-root\domains\engines\environments\hf4-media\windows-x86_64"));
        assert_eq!(entry["tool_paths"][0].as_str(), Some(r"E:\New\engine-root\.venvs\.assets\ffmpeg"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn copying_skips_junctions_instead_of_following_them() {
        let base = scratch("copy");
        let source = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(source.join("atelier-kernel.exe"), b"kernel").expect("file");
        fs::write(outside.join("big.bin"), vec![7u8; 4096]).expect("big file");
        locations::create_junction(&source.join("data"), &outside).expect("junction");

        let target = base.join("target");
        let mut report = RelocateReport::default();
        let mut links = Vec::new();
        copy_without_progress(&source, &target, &mut report, &mut links).expect("copy");

        assert_eq!(report.copied_files, 1, "联接里的内容不该被复制");
        assert!(target.join("atelier-kernel.exe").is_file());
        assert!(report.kept_links.contains(&"data".to_string()));
        assert_eq!(links, vec![(PathBuf::from("data"), outside.clone())]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn removing_a_tree_never_deletes_through_a_junction() {
        let base = scratch("remove");
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("keep.bin"), b"keep").expect("keep file");
        locations::create_junction(&root.join("data"), &outside).expect("junction");

        remove_tree(&root).expect("remove");
        assert!(!root.exists());
        assert!(outside.join("keep.bin").is_file(), "联接目标必须活着");
        let _ = fs::remove_dir_all(&base);
    }

    /// 整棵假安装搬一次：内核文件、venv 的 pyvenv.cfg、账本、`data` 联接都要对。
    #[test]
    fn relocating_a_tree_rewrites_paths_and_keeps_links() {
        let base = scratch("relocate-tree");
        let root = base.join("root");
        let target = base.join("target");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("engine-root").join(".venvs")).expect("venvs");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(root.join("atelier-kernel.exe"), b"kernel").expect("kernel");
        fs::write(root.join("kernel_boot.json"), b"{}").expect("boot");
        fs::write(outside.join("atelier_kernel.db"), b"db").expect("db");
        locations::create_junction(&root.join("data"), &outside).expect("data junction");

        let env = root.join("engine-root").join(".venvs").join("hf4-media");
        fs::create_dir_all(env.join("Scripts")).expect("env");
        fs::write(env.join("pyvenv.cfg"),
            format!("home = {}\nuv = 0.11.19\n",
                root.join("engine-root").join(".venvs").join(".python").join("cpython-3.11.15").display()))
            .expect("pyvenv");
        // 账本里的解释器指向不存在的文件：搬家后必须进「需要重建」，不能报错。
        let python = env.join("Scripts").join("python.exe");
        let ledger = root.join("engine-root").join(".venvs").join("engine-environments.json");
        fs::write(&ledger, serde_json::json!({
            "hf4-media/windows-x86_64": {"python": python.display().to_string()}
        }).to_string()).expect("ledger");

        let mut phases = Vec::new();
        let report = relocate_tree(&root, &target, &mut |phase, label, _| {
            phases.push((phase, label));
        }).expect("relocate");

        assert!(target.join("atelier-kernel.exe").is_file());
        assert!(report.kept_links.contains(&"data".to_string()));
        assert!(locations::is_link(&target.join("data")), "联接要在新位置重建");
        let rebuilt_pyvenv = fs::read_to_string(
            target.join("engine-root").join(".venvs").join("hf4-media").join("pyvenv.cfg"))
            .expect("pyvenv read");
        assert!(!rebuilt_pyvenv.contains(&root.display().to_string()), "旧根不该留在 pyvenv.cfg 里：{rebuilt_pyvenv}");
        assert!(rebuilt_pyvenv.contains(&target.display().to_string()));
        let moved: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(target.join("engine-root").join(".venvs").join("engine-environments.json"))
                .expect("ledger read")).expect("json");
        assert!(moved.get("hf4-media/windows-x86_64").is_none(), "坏掉的环境必须从账本移除");
        assert_eq!(report.rebuild, vec!["hf4-media/windows-x86_64".to_string()]);
        assert!(phases.iter().any(|(phase, _)| *phase == "copy"));
        let _ = fs::remove_dir_all(&base);
    }

    /// 没有内核的目录不算「装好」：宁可失败也不把旧安装删掉。
    #[test]
    fn relocating_refuses_a_tree_without_a_kernel() {
        let base = scratch("relocate-nokernel");
        let root = base.join("root");
        let target = base.join("target");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("random.txt"), b"x").expect("file");
        let error = relocate_tree(&root, &target, &mut |_, _, _| {}).expect_err("must fail");
        assert!(error.contains("atelier-kernel.exe"), "{error}");
        assert!(!target.exists(), "失败时不该留下半个新目录");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn shim_scan_finds_the_old_root_in_a_console_script() {
        let base = scratch("shim");
        let env = base.join("hf4-media");
        fs::create_dir_all(env.join("Scripts")).expect("scripts");
        let old = r"G:\Old\engine-root\.venvs\hf4-media\Scripts\python.exe";
        let mut payload = b"MZ".to_vec();
        payload.extend_from_slice(old.as_bytes());
        fs::write(env.join("Scripts").join("indextts2.exe"), &payload).expect("shim");
        let python = env.join("Scripts").join("python.exe");
        assert!(shim_has_old_root(&python, Path::new(r"G:\Old")));
        assert!(!shim_has_old_root(&python, Path::new(r"E:\New")));
        let _ = fs::remove_dir_all(&base);
    }
}
