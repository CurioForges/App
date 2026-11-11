//! 三个目录的占用统计（程序 / 数据 / 权重）。
//!
//! 数字只服务于卡片上那条比例条，所以口径是"每个字节只归一段"：
//! `程序` 扫安装根但排除 data/models，`数据` 与 `权重` 各自独立扫 —— 否则
//! `F:\tmp` 里本来就包含 `F:\tmp\data`，直接相加会把同一份字节算两遍。
//!
//! 扫描很贵：70 GB / 38 万个文件在机械盘上要 80 秒。因此一律在后台线程里算，
//! 结果缓存进 `StorageState`，安装/卸载/预热结束后才失效重算；界面永远读缓存。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

/// 一条色块：一个区间。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub key: String,
    pub label: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub segments: Vec<Segment>,
    pub total: u64,
    /// 后台正在重新统计；界面上数字是上一次的结果。
    pub scanning: bool,
    /// 毫秒时间戳；`None` 表示还没扫过。
    pub updated_at: Option<u64>,
}

#[derive(Default)]
pub struct StorageState(pub Mutex<Usage>);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 递归累加目录体积，跳过 `skip` 里的路径（`data`/`models` 单独成段）。
fn dir_bytes(path: &Path, skip: &[PathBuf]) -> u64 {
    let Ok(entries) = fs::read_dir(path) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let child = entry.path();
        if skip.iter().any(|s| child == *s) {
            continue;
        }
        match entry.file_type() {
            // 目录联接/符号链接不追进去：追了会把同一份数据算两遍，甚至循环。
            Ok(kind) if kind.is_symlink() => {}
            Ok(kind) if kind.is_dir() => total += dir_bytes(&child, skip),
            Ok(_) => total += entry.metadata().map(|meta| meta.len()).unwrap_or(0),
            Err(_) => {}
        }
    }
    total
}

fn inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// 三个根：安装根、数据目录、权重目录。数据/权重可能就在安装根里，所以要把它们
/// 从安装根的统计里排除，保证"每个字节只归一段"。
fn roots() -> (PathBuf, PathBuf, PathBuf) {
    let settings = crate::settings::load();
    let root = crate::variant_runtime::install_dir()
        .unwrap_or_else(|_| PathBuf::from(r"C:\AtelierFree"));
    let data = settings.data_path(&root, crate::catalog::INSTALL_ID);
    let models = settings.models_path(&root);
    (root, data, models)
}

fn measure() -> Usage {
    let (root, data, models) = roots();
    // `engine-root` 单独成段：8 个 Python 环境通常占安装根的 95% 以上，混在"程序"里
    // 会把 200 MB 的程序文件压成看不见的一丝。
    let env = root.join("engine-root");
    // 安装根要跳过的：内嵌的数据/权重目录，以及单独统计的 engine-root。
    let mut skip: Vec<PathBuf> = [&data, &models]
        .into_iter()
        .filter(|path| inside(path, &root) && *path != &root)
        .cloned()
        .collect();
    if env.is_dir() {
        skip.push(env.clone());
    }

    // 四个目录各开一个线程量，墙钟时间取最长的那一个而不是相加。
    let app_thread = { let root = root.clone(); let skip = skip.clone(); std::thread::spawn(move || dir_bytes(&root, &skip)) };
    let env_thread = { let env = env.clone(); std::thread::spawn(move || dir_bytes(&env, &[])) };
    let data_thread = { let data = data.clone(); std::thread::spawn(move || dir_bytes(&data, &[])) };
    let models_thread = { let models = models.clone(); std::thread::spawn(move || dir_bytes(&models, &[])) };

    let mut segments = vec![
        Segment { key: "app".into(), label: "程序".into(), bytes: app_thread.join().unwrap_or(0) },
        Segment { key: "env".into(), label: "环境".into(), bytes: env_thread.join().unwrap_or(0) },
        Segment { key: "data".into(), label: "数据".into(), bytes: data_thread.join().unwrap_or(0) },
        Segment { key: "models".into(), label: "权重".into(), bytes: models_thread.join().unwrap_or(0) },
    ];
    segments.sort_by_key(|segment| std::cmp::Reverse(segment.bytes));
    let total = segments.iter().map(|segment| segment.bytes).sum();
    Usage { segments, total, scanning: false, updated_at: Some(now_ms()) }
}

/// 上次结果落盘，下次启动先把它摆出来，再在后台重新校准 —— 否则每次开启动器都要
/// 盯着"统计中…"等几十秒（这台机器首扫 80 秒）。
fn snapshot_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let dir = base.join("Atelier");
    let _ = fs::create_dir_all(&dir);
    Some(dir.join("storage.json"))
}

fn save_snapshot(usage: &Usage) {
    let Some(path) = snapshot_path() else { return };
    if let Ok(text) = serde_json::to_string(usage) {
        let _ = fs::write(path, text);
    }
}

fn load_snapshot() -> Option<Usage> {
    let path = snapshot_path()?;
    let text = fs::read_to_string(path).ok()?;
    let mut usage: Usage = serde_json::from_str(&text).ok()?;
    usage.scanning = true; // 数字是上次的，后台正在重新校准
    Some(usage)
}

fn publish(app: &AppHandle, usage: Usage) {
    if let Some(state) = app.try_state::<StorageState>() {
        if let Ok(mut slot) = state.0.lock() {
            *slot = usage.clone();
        }
    }
    save_snapshot(&usage);
    let _ = app.emit("atelier://storage", usage);
}

/// 启动时把上次的账本灌进状态，界面立刻有数字。
pub fn prime(app: &AppHandle) {
    let Some(usage) = load_snapshot() else { return };
    if let Some(state) = app.try_state::<StorageState>() {
        if let Ok(mut slot) = state.0.lock() {
            *slot = usage;
        }
    }
}

/// 后台重新统计。`app` 缺失时只算不广播（测试/关窗后）。
pub fn refresh(app: AppHandle) {
    if let Some(state) = app.try_state::<StorageState>() {
        if let Ok(mut slot) = state.0.lock() {
            slot.scanning = true;
        }
    }
    std::thread::spawn(move || {
        // 让界面先把"统计中"画出来，再开始啃磁盘。
        std::thread::sleep(Duration::from_millis(50));
        let usage = measure();
        publish(&app, usage);
    });
}

#[tauri::command]
pub fn storage_usage(state: State<'_, StorageState>) -> Usage {
    state.0.lock().map(|slot| slot.clone()).unwrap_or_default()
}
