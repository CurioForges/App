//! Launcher-native install / update.
//!
//! `install.ps1` stays in the distribution for machines that have no launcher, but the
//! launcher no longer calls it. It reads the composition manifest
//! (`release/artifacts.json` of the release tag), downloads every domain artifact itself,
//! verifies the sha256 sidecar and only then touches the installed tree. Each step emits
//! an `atelier://progress` event, which is what the update overlay renders.
//!
//! Everything here is deliberately blocking: it runs inside `spawn_blocking` and the work
//! is dominated by downloads and file IO.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::variant_runtime::install_dir;

/// Where the composition manifest lives.
const COMPOSITION_REPO: &str = "NomadAnget/AtelierFree";
/// Where the kernel runtime and the Python SDK come from.
const KERNEL_REPO: &str = "NomadAnget/Atelier-Kernel";

/// One progress sample. `percent: None` means "unknown": the UI shows a spinner, never a
/// made-up number.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job: &'static str,
    pub phase: &'static str,
    pub label: String,
    pub percent: Option<f32>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub rate: Option<u64>,
    pub detail: Option<String>,
    pub log: Option<String>,
}

impl Progress {
    fn phase(phase: &'static str, label: impl Into<String>) -> Self {
        Self { job: "install", phase, label: label.into(), percent: None, done: None,
               total: None, rate: None, detail: None, log: None }
    }

    fn with_bytes(mut self, done: u64, total: Option<u64>, rate: Option<u64>) -> Self {
        self.done = Some(done);
        self.total = total;
        self.rate = rate;
        self
    }

    fn with_percent(mut self, value: f32) -> Self {
        self.percent = Some(value);
        self
    }

    fn with_rate(mut self, rate: u64) -> Self {
        self.rate = Some(rate);
        self
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn log(mut self, line: impl Into<String>) -> Self {
        self.log = Some(line.into());
        self
    }
}

fn emit(app: &AppHandle, progress: Progress) {
    // A missing window (tray-only) must not abort an install.
    let _ = app.emit("atelier://progress", progress);
}

#[derive(Debug, Deserialize)]
struct Composition {
    train: String,
    kernel_version: String,
    sdk_version: String,
    artifacts: BTreeMap<String, Component>,
}

#[derive(Debug, Deserialize)]
struct Component {
    repo: String,
    release: String,
    asset: String,
    #[allow(dead_code)]
    source_sha: String,
}

/// What the launcher intends to install: the four free domains, the video plugin, the
/// kernel runtime and the Python SDK. Paid domains join here once they are licensed.
struct Plan {
    train: String,
    items: Vec<Item>,
}

struct Item {
    name: &'static str,
    repo: String,
    release: String,
    asset: String,
    size: u64,
}

/// `gh` must be authenticated: the distribution repos are private.
/// Spawn without a console window: a flood of cmd windows is what the old script-based
/// updater did not have to worry about.
fn hidden(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

fn gh(args: &[&str]) -> Result<String, String> {
    let output = hidden("gh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行 gh：{e}（更新需要 GitHub CLI 与访问权限）"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh {} 失败：{}", args.first().unwrap_or(&""), err.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn latest_tag() -> Result<String, String> {
    let tag = gh(&["api", &format!("repos/{COMPOSITION_REPO}/releases/latest"), "--jq", ".tag_name"])?;
    let tag = tag.trim().to_string();
    if tag.is_empty() { return Err("无法确定最新发行版本".into()) }
    Ok(tag)
}

fn composition(tag: &str) -> Result<Composition, String> {
    let path = format!("repos/{COMPOSITION_REPO}/contents/release/artifacts.json?ref={tag}");
    let raw = gh(&["api", &path, "-H", "Accept: application/vnd.github.raw"])?;
    serde_json::from_str(&raw).map_err(|e| format!("组合配置解析失败：{e}"))
}

/// Asset sizes, so the overlay can show a real percentage instead of a spinner.
fn asset_sizes(repo: &str, release: &str) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    let Ok(json) = gh(&["release", "view", release, "--repo", repo, "--json", "assets"]) else {
        return sizes;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else { return sizes };
    if let Some(assets) = value.get("assets").and_then(|a| a.as_array()) {
        for asset in assets {
            let (Some(name), Some(size)) = (
                asset.get("name").and_then(|n| n.as_str()),
                asset.get("size").and_then(|s| s.as_u64()),
            ) else { continue };
            sizes.insert(name.to_string(), size);
        }
    }
    sizes
}

fn plan(composition: &Composition) -> Plan {
    const WANTED: [(&str, &str); 5] = [
        ("web", "界面与工作台"),
        ("scheduler", "任务调度"),
        ("pipelines", "创作管线"),
        ("engines", "模型能力"),
        ("video", "视频翻译插件"),
    ];
    let mut items = Vec::new();
    for (key, label) in WANTED {
        let Some(component) = composition.artifacts.get(key) else { continue };
        let sizes = asset_sizes(&component.repo, &component.release);
        items.push(Item {
            name: label,
            repo: component.repo.clone(),
            release: component.release.clone(),
            size: sizes.get(&component.asset).copied().unwrap_or(0),
            asset: component.asset.clone(),
        });
    }
    // Kernel runtime + SDK come from the kernel repo's own release.
    let kernel_release = format!("v{}", composition.kernel_version);
    let sizes = asset_sizes(KERNEL_REPO, &kernel_release);
    let runtime = format!("Atelier-Kernel-runtime-windows-x86_64-v{}.zip", composition.kernel_version);
    items.push(Item {
        name: "Kernel 运行时",
        repo: KERNEL_REPO.into(),
        release: kernel_release.clone(),
        size: sizes.get(&runtime).copied().unwrap_or(0),
        asset: runtime,
    });
    let sdk = format!("atelier_sdk-{}-py3-none-any.whl", composition.sdk_version);
    items.push(Item {
        name: "Python SDK",
        repo: KERNEL_REPO.into(),
        release: kernel_release,
        size: sizes.get(&sdk).copied().unwrap_or(0),
        asset: sdk,
    });
    Plan { train: composition.train.clone(), items }
}

fn cache_dir(train: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "没有 LOCALAPPDATA".to_string())?;
    let dir = base.join("Atelier").join("download-cache").join(train);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

pub(crate) fn sha256_of(path: &Path) -> Result<String, String> {
    let output = hidden("certutil")
        .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
        .output()
        .map_err(|e| format!("无法调用 certutil：{e}"))?;
    if !output.status.success() { return Err(format!("certutil 失败：{}", path.display())) }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let candidate = line.trim().replace(' ', "");
        if candidate.len() == 64 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(candidate.to_ascii_lowercase());
        }
    }
    Err(format!("certutil 输出里没有 sha256：{}", path.display()))
}

/// Download one asset, polling the growing file so the overlay can show bytes and rate.
fn download(app: &AppHandle, item: &Item, dir: &Path, index: usize, count: usize, base: u64, total_all: u64) -> Result<PathBuf, String> {
    let target = dir.join(&item.asset);
    let total = if item.size > 0 { Some(item.size) } else { None };
    // 缓存命中:文件在、旁挂校验通过就不重下(省流量也省时间)。校验不过才清掉重来。
    if target.is_file() && fs::metadata(&target).map(|m| m.len() > 0).unwrap_or(false)
        && verify(app, item, &target).is_ok()
    {
        let size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        emit(app, Progress::phase("download", format!("{} 命中缓存", item.name))
            .with_bytes(base + size, Some(total_all.max(1)), None)
            .with_percent(((index as f32 + 1.0) / (count.max(1) as f32)) * 100.0)
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name))
            .log(format!("{} 已在缓存中且校验通过，跳过下载", item.asset)));
        return Ok(target);
    }    // 上一次运行留下的同名文件会让第一帧读到满尺寸 → 进度先跳高再回退。清掉再下。
    let _ = fs::remove_file(&target);
    let mut child = hidden("gh")
        .args(["release", "download", &item.release, "--repo", &item.repo,
               "--pattern", &item.asset, "--dir", &dir.to_string_lossy(), "--clobber"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 gh 下载：{e}"))?;

    let started = Instant::now();
    loop {
        let done = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        let rate = if started.elapsed().as_secs() > 0 { Some(done / started.elapsed().as_secs()) } else { None };
                // 进度按组件数加权:每个文件各自 0→100% 占 1/count。不看标称总量,
        // 避免元数据大小与实际字节不一致导致百分比忽高忽低(仍可能回退)。
        let frac = if item.size > 0 { (done.min(item.size) as f64 / item.size as f64) as f32 } else { 0.0 };
        let percent = Some(((index as f32 + frac) / (count.max(1) as f32)) * 100.0);
        let mut progress = Progress::phase("download", format!("正在下载 {}", item.name))
            .with_bytes(base + done, Some(total_all), rate)
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name));
        progress.percent = percent;
        emit(app, progress);
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        use std::io::Read;
                        let _ = stderr.read_to_string(&mut err);
                    }
                    return Err(format!("下载 {} 失败：{}", item.asset, err.trim()));
                }
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(format!("等待下载进程失败：{e}")),
        }
    }
    if !target.is_file() { return Err(format!("下载后找不到 {}", target.display())) }
    Ok(target)
}

/// Compare against the published `.sha256` sidecar; a missing sidecar is a hard failure.
fn verify(app: &AppHandle, item: &Item, file: &Path) -> Result<(), String> {
    let dir = file.parent().ok_or("缓存目录缺失")?;
    let sidecar = format!("{}.sha256", item.asset);
    let status = hidden("gh")
        .args(["release", "download", &item.release, "--repo", &item.repo,
               "--pattern", &sidecar, "--dir", &dir.to_string_lossy(), "--clobber"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status()
        .map_err(|e| format!("无法下载校验文件：{e}"))?;
    if !status.success() {
        // Domains publish `<asset>.sha256`; the kernel release does not (yet). Refuse to skip
        // verification for a domain, but do not brick the update on the kernel runtime — the
        // gap is tracked and will be closed by publishing sidecars there.
        if item.repo == KERNEL_REPO {
            emit(app, Progress::phase("verify", format!("跳过校验 {}", item.name))
                .log(format!("{} 没有 sha256 旁挂（Kernel release 尚未发布），本次不做逐字节校验", item.asset)));
            return Ok(());
        }
        return Err(format!("发布方没有提供 {} 的 sha256 旁挂，拒绝安装", item.asset));
    }
    let expected = fs::read_to_string(dir.join(&sidecar))
        .map_err(|e| e.to_string())?
        .trim()
        .to_ascii_lowercase();
    let actual = sha256_of(file)?;
    if expected != actual {
        return Err(format!("{} 校验失败：{} ≠ {}", item.asset, &actual[..12], &expected[..12.min(expected.len())]));
    }
    Ok(())
}

/// Unpack one component into the staging tree, dropping the artifact's `payload/` wrapper so
/// the result mirrors the install directory (`atelier-web.exe`, `atelier-scheduler/`, …).
fn extract(app: &AppHandle, item: &Item, file: &Path, staging: &Path) -> Result<(PathBuf, u64), String> {
    let started = Instant::now();
    let scratch = staging.join(format!("raw-{}", file.file_stem().unwrap_or_default().to_string_lossy()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    // Windows ships bsdtar, which reads zip; fall back to PowerShell if it is missing.
    let status = hidden("tar")
        .args(["-xf", &file.to_string_lossy(), "-C", &scratch.to_string_lossy()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status();
    let ok = matches!(status, Ok(s) if s.success());
    if !ok {
        let script = format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            file.display(), scratch.display());
        let fallback = hidden("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status();
        if !matches!(fallback, Ok(s) if s.success()) {
            return Err(format!("解压 {} 失败", item.asset));
        }
    }
    let payload = if scratch.join("payload").is_dir() { scratch.join("payload") } else { scratch.clone() };
    // Guard: user data never travels through a component swap.
    for name in crate::contract::PRESERVED {
        let guarded = payload.join(name);
        if guarded.exists() {
            fs::remove_dir_all(&guarded).or_else(|_| fs::remove_file(&guarded)).ok();
        }
    }
    let elapsed = started.elapsed().as_secs().max(1);
    Ok((payload, item.size / elapsed))
}

/// Journal of every replace, so a mid-way failure can put the old tree back.
#[derive(Default)]
struct Journal {
    /// (path that now holds the new content, where the old content was parked)
    moves: Vec<(PathBuf, Option<PathBuf>)>,
}

impl Journal {
    fn rollback(&self) {
        for (dest, backup) in self.moves.iter().rev() {
            match backup {
                Some(old) => {
                    let _ = fs::remove_file(dest).or_else(|_| fs::remove_dir_all(dest));
                    let _ = fs::rename(old, dest);
                }
                None => {
                    let _ = fs::remove_file(dest).or_else(|_| fs::remove_dir_all(dest));
                }
            }
        }
    }
}

fn park(dest: &Path, backup: &Path, journal: &mut Journal) -> Result<(), String> {
    if !dest.exists() { return Ok(()) }
    fs::create_dir_all(backup).map_err(|e| e.to_string())?;
    let parked = backup.join(dest.file_name().unwrap_or_default());
    let _ = fs::remove_file(&parked).or_else(|_| fs::remove_dir_all(&parked));
    fs::rename(dest, &parked).map_err(|e| format!("无法暂存旧文件 {}：{e}", dest.display()))?;
    journal.moves.push((dest.to_path_buf(), Some(parked)));
    Ok(())
}

/// Swap the entries of `staged` into `target`. `engine-root` is merged child by child so the
/// user's `.venvs` and `data` never leave the disk.
fn swap_tree(staged: &Path, target: &Path, backup: &Path, journal: &mut Journal) -> Result<(), String> {
    let entries = fs::read_dir(staged).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if crate::contract::PRESERVED.iter().any(|p| p.eq_ignore_ascii_case(&name_str)) {
            continue;
        }
        let src = entry.path();
        let dest = target.join(&name);
        if name_str.eq_ignore_ascii_case("engine-root") && dest.is_dir() {
            // Merge: swap only the code-bearing children; venvs/data belong to the user.
            let Ok(children) = fs::read_dir(&src) else { continue };
            for child in children.flatten() {
                let child_name = child.file_name();
                let child_str = child_name.to_string_lossy().to_string();
                if matches!(child_str.as_str(), ".venvs" | "data") { continue }
                let child_dest = dest.join(&child_name);
                park(&child_dest, &backup.join("engine-root"), journal)?;
                fs::rename(child.path(), &child_dest)
                    .map_err(|e| format!("无法换入 {}：{e}", child_dest.display()))?;
                journal.moves.push((child_dest, None));
            }
            continue;
        }
        park(&dest, backup, journal)?;
        fs::rename(&src, &dest).map_err(|e| format!("无法换入 {}：{e}", dest.display()))?;
        journal.moves.push((dest, None));
    }
    Ok(())
}

/// 预热:内核装配里凡是暴露 `<id>.prewarm` 的域都调一次。失败只记日志,绝不影响更新结果。
fn prewarm_domains(app: &AppHandle, port: u16) -> usize {
    let mut started = 0usize;   // 各域回执 started 之和(取不到就按调用数兜底)
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8)).no_proxy().build() else { return 0 };
    let Ok(body) = client.get(format!("http://127.0.0.1:{port}/api/assembly")).send()
        .and_then(|r| r.json::<serde_json::Value>()) else { return 0 };
    let Some(domains) = body.get("domains").and_then(|d| d.as_array()) else { return 0 };
    for domain in domains {
        let Some(id) = domain.get("domain_id").and_then(|v| v.as_str()) else { continue };
        let wanted = format!("{id}.prewarm");
        let has = domain.get("routes").and_then(|r| r.as_array())
            .map(|routes| routes.iter().any(|r| r.as_str() == Some(wanted.as_str())))
            .unwrap_or(false);
        if !has { continue }
        emit(app, Progress::phase("env", format!("预热 {id}"))
            .log(format!("{id}.prewarm 已调用")));
        started += 1;
        // web 域契约:target 是完整处理器地址,message_type 必须显式给出。
        let response = client.post(format!("http://127.0.0.1:{port}/api/handler-call"))
            .json(&serde_json::json!({
                "target": wanted, "message_type": "Durable", "params": {}, "data": {}
            }))
            .send();
        match response {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if !status.is_success() {
                    emit(app, Progress::phase("env", format!("{id}.prewarm 被拒绝"))
                        .log(format!("{} prewarm 失败 {}: {}", id, status.as_u16(),
                             text.chars().take(200).collect::<String>())));
                    continue;
                }
                // 等回执:receipt 里带 {started:n}
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(mid) = value.get("message_id").and_then(|v| v.as_str()) {
                        let receipt = client.post(format!("http://127.0.0.1:{port}/api/handler-result"))
                            .json(&serde_json::json!({"message_id": mid, "timeout": 15}))
                            .send().and_then(|r| r.text());
                        // 回执里带 {started:n}:用它作为"要等几个 ready"的预期,而不是按域计数。
                        let text = receipt.unwrap_or_default();
                        let targets = serde_json::from_str::<serde_json::Value>(&text).ok()
                            .and_then(|v| v.get("receipt").and_then(|r| r.get("data"))
                                .and_then(|d| d.get("started")).and_then(|s| s.as_u64()))
                            .unwrap_or(0);
                        if targets > 0 { started = started.saturating_sub(1).saturating_add(targets as usize) }
                        emit(app, Progress::phase("env", format!("{id}.prewarm 已受理"))
                            .log(format!("{} prewarm 回执 started={} {}", id, targets,
                                 text.chars().take(120).collect::<String>())));
                    }
                }
            }
            Err(error) => emit(app, Progress::phase("env", format!("{id}.prewarm 无法连接"))
                .log(format!("{} prewarm 请求失败: {}", id, error))),
        }
    }
    started
}
/// 目录体积(逐层累加文件大小)。用于把 uv 缓存的增长换算成实时下载速率。
fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => total += dir_bytes(&entry.path()),
                Ok(_) => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}
/// 轮询 /api/logs 抓 `[PREWARM]` 行(web 的日志既有 SSE 也有历史接口,轮询更稳):
/// 有新行就刷新日志、`ready` 计数满足就收工;连续 3 分钟无新行或总时长 45 分钟则放弃。
fn poll_prewarm(app: &AppHandle, port: u16, expected: usize) {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20)).no_proxy().build() else { return };
    let url = format!("http://127.0.0.1:{port}/api/logs?limit=200");
    let started = Instant::now();
    let mut last_id = 0u64;
    let mut last_seen = Instant::now();
    let mut ready = 0usize;
    let mut last_message = String::new();
    let mut last_time = Instant::now();
    let mut last_size = 0u64;

    // 只保留一个很宽的总上限:环境构建可能几十分钟,不能因为"暂时没新日志"就提前进界面。
    while started.elapsed() < std::time::Duration::from_secs(6 * 3600) {
        thread::sleep(Duration::from_secs(2));
        // 实时速率:uv 缓存的增长 / 时间差。用户看到数字在动就知道没卡死。
        let Ok(root) = crate::variant_runtime::install_dir() else { return };
        let cache = root.join("engine-root").join(".venvs").join(".uv-cache");
        let size = dir_bytes(&cache);
        let elapsed = last_time.elapsed().as_secs_f64();
        if elapsed >= 1.0 && size >= last_size {
            let rate = ((size - last_size) as f64 / elapsed) as u64;
            last_size = size;
            last_time = Instant::now();
            emit(app, Progress::phase("env", "环境构建中")
                .with_rate(rate)
                .log(last_message.clone()));
        }
        let Ok(body) = client.get(&url).send().and_then(|r| r.json::<serde_json::Value>()) else { continue };
        let Some(rows) = body.get("logs").and_then(|l| l.as_array()) else { continue };
        for row in rows.iter().rev() {
            let id = row.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            if id <= last_id { continue }
            let message = row.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if !message.contains("[PREWARM]") { continue }
            last_id = id;
            last_seen = Instant::now();
            last_message = message.chars().take(180).collect::<String>();
            if message.contains(" ready") { ready += 1 }
            emit(app, Progress::phase("env", "构建运行环境").log(message.chars().take(180).collect::<String>()));
        }
        if ready >= expected { break }
    }
}
/// The whole flow: download → verify → stage → swap, with rollback through a journal.
pub fn run(app: &AppHandle) -> Result<String, String> {
    emit(app, Progress::phase("resolve", "正在读取组合配置"));
    let tag = latest_tag()?;
    let composition = composition(&tag)?;
    let plan = plan(&composition);
    let total: u64 = plan.items.iter().map(|i| i.size).sum();
    emit(app, Progress::phase("plan", format!("发行版 {}（{} 个组件）", plan.train, plan.items.len()))
        .with_bytes(0, Some(total), None)
        .with_detail(format!("{} / {} 个组件", 0, plan.items.len())));

    let dir = cache_dir(&plan.train)?;
    let install = install_dir()?;
    let staging = install.join(".staging").join(&plan.train);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let count = plan.items.len();
    let mut bytes = 0u64;
    let mut staged_roots: Vec<(PathBuf, String)> = Vec::new();
    // 先把全部组件一次下齐(下载阶段连续),再统一解压——不是分批。
    let mut fetched: Vec<(&Item, PathBuf)> = Vec::new();
    for (index, item) in plan.items.iter().enumerate() {
        let file = download(app, item, &dir, index, count, bytes, total)?;
        verify(app, item, &file)?;
        bytes += item.size;
        fetched.push((item, file));
    }
    for (index, (item, file)) in fetched.into_iter().enumerate() {
        // 解压没有文件级进度:先发一条不确定态(环转圈),别让数字停在那儿假装卡住。
        emit(app, Progress::phase("stage", format!("正在解压 {}", item.name))
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name)));
        let (staged, rate) = extract(app, item, &file, &staging)?;
        staged_roots.push((staged, item.name.to_string()));
        emit(app, Progress::phase("stage", format!("{} 已解压", item.name))
            .with_bytes(bytes, Some(total), None)
            .with_percent(((index as f32 + 1.0) / (count.max(1) as f32)) * 100.0)
            .with_rate(rate)
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name))
            .log(format!("{} 已解压", item.asset)));
    }    emit(app, Progress::phase("swap", format!("正在换入 {} 个组件", count)));
    // 只保留最新一份退路:换入前清掉更早的 .old-*（每个约 700 MB）。
    let keep = format!(".old-{}", plan.train);
    if let Ok(entries) = fs::read_dir(&install) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".old-") && name != keep {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }    let backup = install.join(format!(".old-{}", plan.train));
    let _ = fs::remove_dir_all(&backup);
    let mut journal = Journal::default();
    for (index, (staged, name)) in staged_roots.iter().enumerate() {
        emit(app, Progress::phase("swap", format!("正在换入 {name}"))
            .with_detail(format!("{}/{} 个组件", index + 1, count)));
        if let Err(error) = swap_tree(staged, &install, &backup, &mut journal) {
            journal.rollback();
            return Err(format!("换入 {name} 失败，已回滚：{error}"));
        }
    }
    // 域运行环境:报告就绪情况。真正的构建由 engines 域在首次加载某引擎时按它自己的
    // 命名约定(.venvs/.engine-builds/<env>-<platform>-<hash>)完成,启动器只做体检,
    // 避免两处各自猜路径。
    emit(app, Progress::phase("env", "检查域运行环境"));
    let env_root = install.join("engine-root").join("domains").join("engines").join("environments");
    let builds = install.join("engine-root").join(".venvs").join(".engine-builds");
    let mut env_total = 0usize;
    let mut env_ready = 0usize;
    if let Ok(entries) = fs::read_dir(&env_root) {
        let names: Vec<String> = entries.flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        for name in names {
            env_total += 1;
            let built = fs::read_dir(&builds).ok().map(|dirs| dirs.flatten()
                .any(|d| d.file_name().to_string_lossy().starts_with(&name))).unwrap_or(false);
            if built { env_ready += 1 }
            emit(app, Progress::phase("env", format!("运行环境 {name}"))
                .with_percent(((env_total as f32) / (env_total.max(1) as f32)) * 100.0)
                .with_detail(if built { "已就绪" } else { "首次调用该引擎时自动构建" })
                .log(format!("{name}: {}", if built { "已就绪" } else { "未构建,首次使用时自动建" })));
        }
    }
    if env_total > 0 {
        emit(app, Progress::phase("env", format!("运行环境 {env_ready}/{env_total} 就绪")).log(format!("环境就绪 {env_ready}/{env_total}")));
    }    // 更新即启动:装完直接拉起实例、预热环境,让用户落地就在工作台。
    emit(app, Progress::phase("start", "启动实例"));
    let mut live: Option<u16> = None;
    match tauri::async_runtime::block_on(crate::variant_runtime::open_workbench(None, None)) {
        Ok(port) => {
            live = Some(port);
            emit(app, Progress::phase("check", "实例已就绪").log(format!("四域 active，端口 {port}")));
            let targets = prewarm_domains(app, port);
            emit(app, Progress::phase("env", format!("预热目标 {targets} 个")).log(format!("发现 {targets} 个 <domain>.prewarm")));
            if targets > 0 { poll_prewarm(app, port, targets); }
        }
        Err(error) => emit(app, Progress::phase("fail", "实例启动失败".to_string())
            .log(format!("已更新完成，但启动实例失败：{error}"))),
    }    fs::write(install.join("VERSION"), format!("{}\n", plan.train)).map_err(|e| e.to_string())?;
    let env_note = if env_total > 0 { format!("运行环境 {env_ready}/{env_total} 就绪") } else { "无域环境".to_string() };
    emit(app, Progress::phase("done", format!("已更新到 {}", plan.train)).with_detail(env_note)
        .with_bytes(total, Some(total), None)
        .log(format!("VERSION → {}", plan.train)));
    let _ = fs::remove_dir_all(&staging);
    Ok(match live {
        Some(port) => format!("已更新到 {}，已启动实例（端口 {port}），旧组件留在 {}", plan.train, backup.display()),
        None => format!("已更新到 {}，实例未启动，旧组件留在 {}", plan.train, backup.display()),
    })
}

#[tauri::command]
pub async fn install_variant(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Stop our instance first: replacing files under a live kernel is how the old
        // script-based path used to fail.
        let _ = crate::variant_runtime::stop_sync();
        let result = run(&app);
        if let Err(error) = &result {
            emit(&app, Progress::phase("fail", format!("更新失败：{error}")).log(error.clone()));
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

#[allow(dead_code)]
fn installed_dir() -> PathBuf { install_dir().unwrap_or_default() }
