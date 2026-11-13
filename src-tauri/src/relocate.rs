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
    /// 控制台脚本（`Scripts/*.exe`）里仍编着旧路径的环境。引擎只用 `python -m` 调用，
    /// 所以只提示、不重建 —— 真的重建一次就是几十 GB 下载。
    pub shim_warnings: Vec<String>,
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
    let mut progress = Progress::phase(phase, label)
        .in_job("relocate")
        .with_detail(detail.clone());
    progress = progress.log(detail);
    crate::installer::emit_progress(app, progress);
}

/// 带字节数的迁移进度：界面在弹窗里画横向进度条 + 实时速度。
///
/// 总数还在统计时（`tick.total_bytes == 0`）不说百分比，改说"已经数了多少个文件"，
/// 这样几十万个文件的目录也不会看起来像卡住。
fn emit_copy(app: &AppHandle, tick: CopyTick) {
    let total = (tick.total_bytes > 0).then_some(tick.total_bytes);
    let detail = match total {
        Some(total) => format!(
            "{:.1} / {:.1} GB · {} 个文件",
            tick.bytes as f64 / 1e9,
            total as f64 / 1e9,
            tick.files),
        None => format!(
            "已搬 {:.1} GB · {} 个文件（正在统计总数，已数 {} 个）",
            tick.bytes as f64 / 1e9,
            tick.files,
            tick.scanned_files),
    };
    let label = if total.is_some() { "正在迁移" } else { "正在统计并迁移" };
    let mut progress = Progress::phase("copy", label)
        .in_job("relocate")
        .with_bytes(tick.bytes, total, Some(tick.rate))
        .with_detail(detail.clone());
    progress = progress.log(detail);
    crate::installer::emit_progress(app, progress);
}

fn finish_done(app: &AppHandle, message: &str) {
    let mut progress = Progress::phase("done", message.to_string())
        .in_job("relocate")
        .with_detail(message.to_string());
    progress = progress.log(message.to_string());
    crate::installer::emit_progress(app, progress);
}

fn finish_failed(app: &AppHandle, message: &str) {
    let mut progress = Progress::phase("fail", message.to_string())
        .in_job("relocate")
        .with_detail(message.to_string());
    progress = progress.log(message.to_string());
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

/// 一次进度帧：界面拿它画条和读数。
#[derive(Clone, Copy, Default)]
struct CopyTick {
    bytes: u64,
    files: u64,
    /// 0 表示总数还在统计中。
    total_bytes: u64,
    scanned_files: u64,
    rate: u64,
}

/// 统计线程与复制线程共享的进度槽。
///
/// 先扫描再复制会白等好几分钟（几十万个文件、每个都要一次 stat），所以扫描和复制
/// **同时跑**：界面先看到"已搬多少、多快"，总数出来之后进度条自动从不确定态切成百分比。
#[derive(Default)]
struct ScanShare {
    total_bytes: std::sync::atomic::AtomicU64,
    total_files: std::sync::atomic::AtomicU64,
    scanned_files: std::sync::atomic::AtomicU64,
    scanned_bytes: std::sync::atomic::AtomicU64,
    done: std::sync::atomic::AtomicBool,
}

impl ScanShare {
    fn scanned_files(&self) -> u64 {
        self.scanned_files.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn scanned_bytes(&self) -> u64 {
        self.scanned_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn total_bytes(&self) -> u64 {
        self.total_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn tick(&self, bytes: u64, files: u64, rate: u64) -> CopyTick {
        CopyTick {
            bytes,
            files,
            total_bytes: self.total_bytes(),
            scanned_files: self.scanned_files(),
            rate,
        }
    }
}

/// 量一遍要搬多少字节。四个线程一起走目录：单个线程读目录树时瓶颈在 stat 上，
/// 并行能把几十万个文件从"好几分钟"压到"十几秒"。
///
/// 只读元数据、不打开文件；遇到联接不算它的内容（反正不会复制）。
fn measure_tree(root: &Path, share: &ScanShare) {
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    let queue = Mutex::new(vec![root.to_path_buf()]);
    let workers = 4.min(std::thread::available_parallelism().map(|count| count.get()).unwrap_or(4));
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| loop {
                let next = {
                    let Ok(mut queue) = queue.lock() else { return };
                    queue.pop()
                };
                let Some(current) = next else { return };
                let Ok(entries) = fs::read_dir(&current) else { continue };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(file_type) = entry.file_type() else { continue };
                    if locations::is_link(&path) {
                        continue;
                    }
                    if file_type.is_dir() {
                        if let Ok(mut queue) = queue.lock() {
                            queue.push(path);
                        }
                        continue;
                    }
                    if !file_type.is_file() {
                        continue;
                    }
                    let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                    share.scanned_files.fetch_add(1, Ordering::Relaxed);
                    share.scanned_bytes.fetch_add(size, Ordering::Relaxed);
                }
            });
        }
    });
    share.total_bytes.store(share.scanned_bytes(), Ordering::Relaxed);
    share.total_files.store(share.scanned_files(), Ordering::Relaxed);
    share.done.store(true, Ordering::Relaxed);
}

/// 在后台统计总数，立刻返回（复制不用等它）。
fn start_measure(root: &Path) -> (std::sync::Arc<ScanShare>, std::thread::JoinHandle<()>) {
    let share = std::sync::Arc::new(ScanShare::default());
    let owned = share.clone();
    let root = root.to_path_buf();
    let handle = std::thread::spawn(move || measure_tree(&root, &owned));
    (share, handle)
}

/// 复制线程数。几十万个小文件的跨盘复制全是等待，多开线程才吃得满盘。
const COPY_THREADS: usize = 16;

/// 并行复制的产物：调用方拿它回填报告。
struct CopyOutcome {
    files: u64,
    bytes: u64,
    links: Vec<LinkRecord>,
    kept_links: Vec<String>,
}

/// 树里的一个联接。`dir` 决定搬到新位置后怎么补：目录用联接，文件用硬链接。
#[derive(Debug, Clone, PartialEq)]
struct LinkRecord {
    relative: PathBuf,
    target: PathBuf,
    dir: bool,
}

/// 并行复制共享状态：16 个线程分摊目录，主线程每 200 毫秒报一次进度。
struct CopyState {
    queue: std::sync::Mutex<Vec<(PathBuf, PathBuf)>>,
    queued: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    files: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    stop: std::sync::atomic::AtomicBool,
    failure: std::sync::Mutex<Option<String>>,
    links: std::sync::Mutex<Vec<LinkRecord>>,
    kept: std::sync::Mutex<Vec<String>>,
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl CopyState {
    fn new(src: &Path, dst: &Path) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
        use std::sync::Mutex;
        Self {
            queue: Mutex::new(vec![(src.to_path_buf(), dst.to_path_buf())]),
            queued: AtomicUsize::new(1),
            active: AtomicUsize::new(COPY_THREADS),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            failure: Mutex::new(None),
            links: Mutex::new(Vec::new()),
            kept: Mutex::new(Vec::new()),
        }
    }
}

fn copy_worker(state: &CopyState, base: &Path) {
    use std::sync::atomic::Ordering;
    loop {
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        let next = { lock(&state.queue).pop() };
        let Some((current, current_target)) = next else {
            // 队列空不等于收工：别的线程可能还在往下推目录。
            if state.active.fetch_sub(1, Ordering::AcqRel) == 1 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            state.active.fetch_add(1, Ordering::AcqRel);
            continue;
        };
        state.queued.fetch_sub(1, Ordering::AcqRel);
        if let Err(error) = copy_directory(state, base, &current, &current_target) {
            *lock(&state.failure) = Some(error);
            state.stop.store(true, Ordering::Release);
            return;
        }
    }
}

fn copy_directory(state: &CopyState, base: &Path, current: &Path, target_dir: &Path) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if let Err(error) = fs::create_dir_all(target_dir) {
        return Err(format!("无法创建 {}：{error}", target_dir.display()));
    }
    let entries = fs::read_dir(current).map_err(|error| format!("无法读取 {}：{error}", current.display()))?;
    let mut pushed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let target = target_dir.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
        if file_type.is_symlink() || locations::is_link(&path) {
            lock(&state.links).push(LinkRecord {
                relative: path.strip_prefix(base).unwrap_or(&path).to_path_buf(),
                target: fs::read_link(&path).unwrap_or_default(),
                dir: entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
                    || fs::metadata(&path).map(|meta| meta.is_dir()).unwrap_or(false),
            });
            lock(&state.kept).push(entry.file_name().to_string_lossy().to_string());
            continue;
        }
        if file_type.is_dir() {
            lock(&state.queue).push((path, target));
            pushed += 1;
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        fs::copy(&path, &target).map_err(|error| format!("无法复制 {}：{error}", path.display()))?;
        state.files.fetch_add(1, Ordering::Relaxed);
        state.bytes.fetch_add(size, Ordering::Relaxed);
    }
    if pushed > 0 {
        state.queued.fetch_add(pushed, Ordering::AcqRel);
    }
    Ok(())
}

/// 复制整棵树。目录联接只记录不跟随（`(相对路径, 指向)` 供事后原样重建）；
/// 任何文件出错都会停下并返回第一个错误 —— 迁移宁可整体失败，也不搬出半个树。
fn copy_tree(
    src: &Path,
    dst: &Path,
    base: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<CopyOutcome, String> {
    use std::sync::atomic::Ordering;
    fs::create_dir_all(dst).map_err(|error| format!("无法创建 {}：{error}", dst.display()))?;
    let state = CopyState::new(src, dst);
    std::thread::scope(|scope| {
        for _ in 0..COPY_THREADS {
            scope.spawn(|| copy_worker(&state, base));
        }
        // 主线程当计时器：每 200 毫秒把累计值推给界面。
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            progress(state.files.load(Ordering::Relaxed), state.bytes.load(Ordering::Relaxed));
            if state.stop.load(Ordering::Relaxed) {
                break;
            }
            if state.active.load(Ordering::Acquire) == 0 && state.queued.load(Ordering::Acquire) == 0 {
                break;
            }
        }
    });
    if let Some(error) = lock(&state.failure).take() {
        return Err(error);
    }
    let links: Vec<LinkRecord> = lock(&state.links).drain(..).collect();
    let kept_links: Vec<String> = lock(&state.kept).drain(..).collect();
    Ok(CopyOutcome {
        files: state.files.load(Ordering::Relaxed),
        bytes: state.bytes.load(Ordering::Relaxed),
        links,
        kept_links,
    })
}

/// 把树里跳过的联接在新位置补回来。
///
/// 目录联接（`data/`、HF 的 shared-blobs）用 `mklink /J`，不需要管理员。
/// **文件**符号链接（HF 缓存的 `snapshots/<rev>/xxx` 指向 `../../blobs/<sha>`）不能建联接，
/// 而创建真符号链接需要管理员权限，所以改用硬链接：同一个新目录里，不占额外空间、
/// 也不需要提权；硬链接失败（跨卷等）再退化成复制内容。
fn recreate_links(new_root: &Path, old_root: &Path, links: &[LinkRecord]) -> Result<(), String> {
    for record in links {
        // 指向旧根内部就跟着搬；指向外面（另一块盘的数据目录）保持原样。
        let mapped = match record.target.strip_prefix(old_root) {
            Ok(rest) => new_root.join(rest),
            Err(_) => record.target.clone(),
        };
        // 相对链接（HF 缓存就是这样写的）按新位置的父目录解析。
        let source = if record.target.is_absolute() {
            mapped
        } else {
            let parent = new_root.join(&record.relative);
            let parent = parent.parent().unwrap_or(new_root);
            parent.join(&record.target)
        };
        let link = new_root.join(&record.relative);
        if locations::is_link(&link) || link.exists() {
            continue;
        }
        if record.dir {
            locations::create_junction(&link, &source)?;
            continue;
        }
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("无法创建 {}：{error}", parent.display()))?;
        }
        if fs::hard_link(&source, &link).is_err() {
            // 跨卷或者文件系统不支持硬链接：只能把内容复制一份，至少数据是全的。
            fs::copy(&source, &link).map_err(|error| {
                format!("无法补回 {}（指向 {}）：{error}", link.display(), source.display())
            })?;
        }
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

/// 自检每个已构建环境：改过 `pyvenv.cfg` 之后 `python.exe` 还能不能认出自己。
///
/// 只有解释器真的坏了才从账本里删掉（下次用到时按锁文件重建）。控制台脚本
/// （`Scripts/*.exe`）里编死的旧路径改不了，但引擎只走 `python -m`，所以只作为
/// 提示返回，不触发重建 —— 否则一次搬家就是几十 GB 的重新下载。
fn verify_environments(roots: &[PathBuf], old_root: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let mut rebuild = Vec::new();
    let mut warnings = Vec::new();
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
            if !(python.is_file() && prefix_of(&python).is_some()) {
                entries.remove(&key);
                rebuild.push(key);
                changed = true;
                continue;
            }
            if shim_has_old_root(&python, old_root) {
                warnings.push(key);
            }
        }
        if changed {
            let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
            fs::write(&ledger, text).map_err(|error| format!("无法更新 {}：{error}", ledger.display()))?;
        }
    }
    Ok((rebuild, warnings))
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
///
/// 和复制一样按文件并行：几十万个文件单线程逐个 `remove_file` 会让人以为卡死了。
/// 目录不能并发删（要等子目录空），所以分两段——先并行删文件并收集目录，再自底向上
/// 逐个 `remove_dir`（每次只是一个系统调用）。
fn remove_tree(path: &Path) -> Result<(), String> {
    remove_tree_with(path, &mut |_| {})
}

fn remove_tree_with(path: &Path, progress: &mut dyn FnMut(u64)) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    if !path.exists() && !locations::is_link(path) {
        return Ok(());
    }
    if locations::is_link(path) {
        fs::remove_dir(path).map_err(|error| format!("无法删除联接 {}：{error}", path.display()))?;
        progress(1);
        return Ok(());
    }

    let queue: Mutex<Vec<PathBuf>> = Mutex::new(vec![path.to_path_buf()]);
    let directories: Mutex<Vec<PathBuf>> = Mutex::new(vec![path.to_path_buf()]);
    let queued = AtomicUsize::new(1);
    let active = AtomicUsize::new(COPY_THREADS);
    let deleted = AtomicU64::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);

    std::thread::scope(|scope| {
        for _ in 0..COPY_THREADS {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let next = { lock(&queue).pop() };
                let Some(current) = next else {
                    if active.fetch_sub(1, Ordering::AcqRel) == 1 {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    active.fetch_add(1, Ordering::AcqRel);
                    continue;
                };
                queued.fetch_sub(1, Ordering::AcqRel);
                let entries = match fs::read_dir(&current) {
                    Ok(entries) => entries,
                    Err(error) => {
                        lock(&failures).push(format!("无法读取 {}：{error}", current.display()));
                        continue;
                    }
                };
                let mut pushed = 0usize;
                for entry in entries.flatten() {
                    let child = entry.path();
                    if locations::is_link(&child) {
                        // 联接只删链接本身；删不掉也不影响其它文件。
                        if let Err(error) = fs::remove_dir(&child) {
                            lock(&failures).push(format!("无法删除联接 {}：{error}", child.display()));
                        }
                        continue;
                    }
                    let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                    if is_dir {
                        lock(&queue).push(child.clone());
                        lock(&directories).push(child);
                        pushed += 1;
                        continue;
                    }
                    match fs::remove_file(&child) {
                        Ok(()) => {
                            deleted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            lock(&failures).push(format!("无法删除 {}：{error}", child.display()));
                        }
                    }
                }
                if pushed > 0 {
                    queued.fetch_add(pushed, Ordering::AcqRel);
                }
            });
        }
        // 主线程：报进度，等所有 worker 收工。
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            progress(deleted.load(Ordering::Relaxed));
            if active.load(Ordering::Acquire) == 0 && queued.load(Ordering::Acquire) == 0 {
                break;
            }
        }
    });

    // 自底向上删目录：长的路径一定比短的深。
    let mut dirs = lock(&directories).drain(..).collect::<Vec<_>>();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.as_os_str().len()));
    for dir in dirs {
        if let Err(error) = fs::remove_dir(&dir) {
            lock(&failures).push(format!("无法删除 {}：{error}", dir.display()));
        }
    }

    let failures = lock(&failures).drain(..).collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        // 只报前几条：真正的失败通常是同一个原因（文件被占用）。
        let mut message = failures.iter().take(3).cloned().collect::<Vec<_>>().join("；");
        if failures.len() > 3 {
            message.push_str(&format!("；另有 {} 处失败", failures.len() - 3));
        }
        Err(message)
    }
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

/// 删目录时的进度回调：每 200 毫秒说一次删了多少文件，别让界面停在"正在清理"。
fn clean_up_with_progress(app: &AppHandle, path: &Path, label: &str) -> Result<(), String> {
    let mut last = std::time::Instant::now();
    remove_tree_with(path, &mut |files| {
        if last.elapsed().as_secs_f64() >= 0.25 {
            last = std::time::Instant::now();
            emit(app, "clean", label, format!("已删除 {files} 个文件"));
        }
    })
}

/// 一次真正的搬家：复制 → 补联接 → 重写环境路径 → 自检 → 确认新位置能跑。
///
/// 只碰文件系统，不改设置、不删旧目录 —— 调用方在它成功之后才动旧位置，
/// 这样任何一步失败都能原地退回。测试也直接打这一层。
/// 速率平滑器。
///
/// 单帧差分（200 毫秒窗口）抖得厉害，界面上的速度和"剩余时间"会来回跳；
/// 这里对瞬时速率做指数平滑，再让界面用 `剩余字节 / 速率` 算 ETA。
struct RateMeter {
    last_at: std::time::Instant,
    last_bytes: u64,
    smoothed: f64,
}

impl RateMeter {
    fn new() -> Self {
        Self { last_at: std::time::Instant::now(), last_bytes: 0, smoothed: 0.0 }
    }

    fn rate(&mut self, bytes: u64) -> u64 {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_at).as_secs_f64();
        if elapsed <= 0.0 {
            return self.smoothed as u64;
        }
        let instant = bytes.saturating_sub(self.last_bytes) as f64 / elapsed;
        self.smoothed = if self.smoothed <= 0.0 {
            instant
        } else {
            // 新样本占三成：既跟得上变化，又不会被单个大文件带飞。
            self.smoothed * 0.7 + instant * 0.3
        };
        self.last_at = now;
        self.last_bytes = bytes;
        self.smoothed.max(0.0) as u64
    }
}

/// 一次真正的搬家：复制 → 补联接 → 重写环境路径 → 自检 → 确认新位置能跑。
///
/// 只碰文件系统，不改设置、不删旧目录 —— 调用方在它成功之后才动旧位置，
/// 这样任何一步失败都能原地退回。测试也直接打这一层。
fn relocate_tree(
    root: &Path,
    target: &Path,
    progress: &mut dyn FnMut(CopyTick),
) -> Result<RelocateReport, String> {
    let mut report = RelocateReport { target: target.display().to_string(), ..Default::default() };
    let (share, measure) = start_measure(root);
    progress(share.tick(0, 0, 0));
    let mut last_tick = std::time::Instant::now();
    let mut meter = RateMeter::new();
    let copied = copy_tree(root, target, root, |files, bytes| {
        let now = std::time::Instant::now();
        if now.duration_since(last_tick).as_secs_f64() < 0.25 {
            return;
        }
        last_tick = now;
        progress(share.tick(bytes, files, meter.rate(bytes)));
    });
    // 统计线程要么已经结束，要么马上结束；等它一下，进度条才能落到 100%。
    let _ = measure.join();
    let outcome = copied.map_err(|error| {
        let _ = remove_tree(target);
        format!("迁移中断，旧安装没有改动：{error}")
    })?;
    report.copied_files = outcome.files;
    report.copied_bytes = outcome.bytes;
    report.kept_links = outcome.kept_links;
    let total = share.total_bytes();
    progress(CopyTick {
        bytes: report.copied_bytes,
        files: report.copied_files,
        total_bytes: total,
        scanned_files: report.copied_files,
        rate: 0,
    });

    recreate_links(target, root, &outcome.links)?;
    rewrite_environment_paths(target, root, &mut report)?;
    let (rebuild, warnings) = verify_environments(&venv_roots(target), root)?;
    report.rebuild = rebuild;
    report.shim_warnings = warnings;

    // 新位置至少要有一个可执行的内核，否则宁可回滚也不动旧目录。
    if !target.join("atelier-kernel.exe").is_file() {
        let _ = remove_tree(target);
        return Err(format!(
            "{} 里没有 atelier-kernel.exe，看起来不是真正的安装目录；已放弃本次迁移（旧目录未改动）。",
            root.display()));
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

    emit(app, "copy", "正在统计要搬多少", root.display().to_string());
    let mut report = {
        let mut sink = |tick: CopyTick| emit_copy(app, tick);
        relocate_tree(&root, target, &mut sink)?
    };

    store_install_dir(app, target)?;
    emit(app, "clean", "正在清理旧位置", root.display().to_string());
    report.removed_old = clean_up_with_progress(app, &root, "正在清理旧位置").is_ok();
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
    emit(app, "clean", "正在清理旧位置", root.display().to_string());
    report.removed_old = clean_up_with_progress(app, &root, "正在清理旧位置").is_ok();
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
    emit(app, "copy", "正在统计要搬多少", current.display().to_string());
    let (share, measure) = start_measure(&current);
    let mut last_tick = std::time::Instant::now();
    let mut meter = RateMeter::new();
    let copied = copy_tree(&current, target, &current, |files, bytes| {
        let now = std::time::Instant::now();
        if now.duration_since(last_tick).as_secs_f64() < 0.25 {
            return;
        }
        last_tick = now;
        emit_copy(app, share.tick(bytes, files, meter.rate(bytes)));
    });
    let _ = measure.join();
    let outcome = copied.map_err(|error| {
        let _ = remove_tree(target);
        format!("迁移中断，数据没有改动：{error}")
    })?;
    report.copied_files = outcome.files;
    report.copied_bytes = outcome.bytes;
    report.kept_links = outcome.kept_links;
    emit_copy(app, CopyTick {
        bytes: report.copied_bytes,
        files: report.copied_files,
        total_bytes: share.total_bytes(),
        scanned_files: report.copied_files,
        rate: 0,
    });

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
    emit(app, "clean", "正在清理旧数据", backup.display().to_string());
    let cleaned = clean_up_with_progress(app, &backup, "正在清理旧数据").is_ok() && !backup.exists();
    report.removed_old = cleaned;
    report.message = format!(
        "数据已迁到 {}：{} 个文件 / {:.1} GB{}",
        target.display(),
        report.copied_files,
        report.copied_bytes as f64 / 1e9,
        if report.removed_old { "，旧目录已清空" } else { "，旧目录没删干净" });
    Ok(report)
}

/// 换权重目录。`mode`：`move` 把现有权重搬过去，`switch` 只改设置（新目录里已经有权重时用）。
///
/// 权重和安装目录的区别只有一处：HF 缓存里 `snapshots/<rev>/xxx` 是指向 `blobs/` 的
/// 文件符号链接，搬完要按硬链接补回来（见 `recreate_links`）。其余流程完全复用。
#[tauri::command]
pub async fn relocate_models(
    app: AppHandle,
    target: String,
    mode: String,
) -> Result<RelocateReport, String> {
    let app_for_task = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        match mode.as_str() {
            "move" => move_models(&app_for_task, Path::new(target.trim())),
            "switch" => switch_models(&app_for_task, Path::new(target.trim())),
            other => Err(format!("未知的迁移方式：{other}")),
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    match &result {
        Ok(report) => finish_done(&app, &report.message),
        Err(error) => finish_failed(&app, error),
    }
    result
}

fn save_models_dir(app: &AppHandle, target: &Path) -> Result<(), String> {
    let mut settings = app
        .try_state::<SettingsState>()
        .map(|state| state.snapshot())
        .unwrap_or_else(settings::load);
    settings.shared.models_dir = target.display().to_string();
    settings::save(&settings)?;
    if let Some(state) = app.try_state::<SettingsState>() {
        *state.0.lock().map_err(|error| error.to_string())? = settings;
    }
    Ok(())
}

fn switch_models(app: &AppHandle, target: &Path) -> Result<RelocateReport, String> {
    if !target.is_absolute() {
        return Err("权重目录必须是绝对路径。".into());
    }
    let check = locations::inspect("models", target);
    if let Some(blocker) = check.blockers.first() {
        return Err(format!("权重目录不可用：{blocker}"));
    }
    fs::create_dir_all(target).map_err(|error| format!("无法创建权重目录：{error}"))?;
    save_models_dir(app, target)?;
    Ok(RelocateReport {
        target: target.display().to_string(),
        message: format!("已把权重目录切到 {}（没有搬动文件）", target.display()),
        ..Default::default()
    })
}

fn move_models(app: &AppHandle, target: &Path) -> Result<RelocateReport, String> {
    let root = variant_runtime::install_dir()?;
    let settings_now = app
        .try_state::<SettingsState>()
        .map(|state| state.snapshot())
        .unwrap_or_else(settings::load);
    let current = settings_now.models_path(&root);
    check_target("权重目录", &current, target)?;

    let mut report = RelocateReport { target: target.display().to_string(), ..Default::default() };
    if !current.is_dir() {
        save_models_dir(app, target)?;
        report.message = format!(
            "{} 里还没有权重，已直接把权重目录设成 {}",
            current.display(), target.display());
        return Ok(report);
    }

    emit(app, "relocate", "正在停止实例", root.display().to_string());
    let _ = variant_runtime::stop_sync();
    emit(app, "copy", "正在统计要搬多少", current.display().to_string());
    let (share, measure) = start_measure(&current);
    let mut last_tick = std::time::Instant::now();
    let mut meter = RateMeter::new();
    let copied = copy_tree(&current, target, &current, |files, bytes| {
        let now = std::time::Instant::now();
        if now.duration_since(last_tick).as_secs_f64() < 0.25 {
            return;
        }
        last_tick = now;
        emit_copy(app, share.tick(bytes, files, meter.rate(bytes)));
    });
    let _ = measure.join();
    let outcome = copied.map_err(|error| {
        let _ = remove_tree(target);
        format!("迁移中断，权重没有改动：{error}")
    })?;
    report.copied_files = outcome.files;
    report.copied_bytes = outcome.bytes;
    report.kept_links = outcome.kept_links;
    emit_copy(app, CopyTick {
        bytes: report.copied_bytes,
        files: report.copied_files,
        total_bytes: share.total_bytes(),
        scanned_files: report.copied_files,
        rate: 0,
    });
    recreate_links(target, &current, &outcome.links)?;

    save_models_dir(app, target)?;
    // 权重目录常常在安装根之外（甚至是别的仓库的 checkpoints），那种旧目录不替用户删。
    let ours = current.starts_with(&root);
    report.removed_old = if ours {
        emit(app, "clean", "正在清理旧权重", current.display().to_string());
        clean_up_with_progress(app, &current, "正在清理旧权重").is_ok()
    } else {
        false
    };
    report.message = format!(
        "权重已迁到 {}：{} 个文件 / {:.1} GB{}",
        target.display(),
        report.copied_files,
        report.copied_bytes as f64 / 1e9,
        if report.removed_old {
            "，旧目录已清空".to_string()
        } else {
            format!("；旧目录 {} 在安装根之外，没有替你删除", current.display())
        });
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
    ) -> Result<(u64, u64, Vec<LinkRecord>, Vec<String>), String> {
        let outcome = copy_tree(src, dst, src, |_, _| {})?;
        Ok((outcome.files, outcome.bytes, outcome.links, outcome.kept_links))
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

    /// HF 缓存的快照是指向 `../blobs/<sha>` 的文件符号链接。管理员权限才能建真符号链接，
    /// 所以搬迁后用硬链接补：内容共享、不额外占空间。
    #[test]
    fn file_links_are_recreated_as_hard_links() {
        let base = scratch("file-links");
        let root = base.join("new");
        let blob = root.join("blobs").join("abc123");
        fs::create_dir_all(blob.parent().expect("blobs dir")).expect("blobs");
        fs::write(&blob, b"weights").expect("blob");
        let links = vec![LinkRecord {
            relative: PathBuf::from("snapshots").join("rev").join("config.json"),
            target: PathBuf::from("..").join("..").join("blobs").join("abc123"),
            dir: false,
        }];
        recreate_links(&root, &root, &links).expect("recreate");
        let link = root.join("snapshots").join("rev").join("config.json");
        assert_eq!(fs::read(&link).expect("read"), b"weights");
        // 硬链接的两条路径是同一份数据：改一个，另一个也变。
        fs::write(&link, b"changed").expect("write through link");
        assert_eq!(fs::read(&blob).expect("read blob"), b"changed", "应该是硬链接而不是复制");
        let _ = fs::remove_dir_all(&base);
    }

    /// 16 线程分摊目录：数量对不上就是队列计数错了（少搬或多搬都会在这里露出来）。
    #[test]
    fn parallel_copy_copies_every_file() {
        let base = scratch("parallel");
        let source = base.join("root");
        for index in 0..120 {
            let dir = source.join(format!("d{index:03}"));
            fs::create_dir_all(&dir).expect("dir");
            for file in 0..20 {
                fs::write(dir.join(format!("f{file:02}.txt")), vec![b'x'; 64]).expect("file");
            }
        }
        let (files, bytes, _, _) =
            copy_without_progress(&source, &base.join("target")).expect("copy");
        assert_eq!(files, 2400, "文件数必须一个不少");
        assert_eq!(bytes, 2400 * 64);
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
        let (files, _, links, kept) = copy_without_progress(&source, &target).expect("copy");

        assert_eq!(files, 1, "联接里的内容不该被复制");
        assert!(target.join("atelier-kernel.exe").is_file());
        assert!(kept.contains(&"data".to_string()));
        assert_eq!(
            links,
            vec![LinkRecord { relative: PathBuf::from("data"), target: outside.clone(), dir: true }]);
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
        let report = relocate_tree(&root, &target, &mut |tick| {
            phases.push((tick.bytes, tick.files, tick.total_bytes, tick.rate));
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
        assert!(phases.iter().any(|(bytes, _, total, _)| *total > 0 && *bytes <= *total));
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
        let error = relocate_tree(&root, &target, &mut |_| {}).expect_err("must fail");
        assert!(error.contains("atelier-kernel.exe"), "{error}");
        assert!(!target.exists(), "失败时不该留下半个新目录");
        let _ = fs::remove_dir_all(&base);
    }

    /// 并行删：嵌套目录、600 个文件，删完必须一个不剩，且进度回调报过数。
    #[test]
    fn parallel_delete_removes_nested_trees() {
        let base = scratch("delete");
        let root = base.join("old");
        for index in 0..20 {
            let dir = root.join(format!("d{index:02}")).join("inner");
            fs::create_dir_all(&dir).expect("dir");
            for file in 0..30 {
                fs::write(dir.join(format!("f{file:02}.txt")), b"x").expect("file");
            }
        }
        let mut seen = 0u64;
        remove_tree_with(&root, &mut |files| seen = seen.max(files)).expect("remove");
        assert!(!root.exists(), "整棵树必须删干净");
        assert!(base.is_dir(), "父目录不能被误删");
        assert!(seen <= 600, "进度回调不会多报");
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
