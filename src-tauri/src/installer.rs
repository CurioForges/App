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

/// 排障用的痕迹日志:只有**开发构建**且设了 `ATELIER_TRACE` 才写。
/// 发布版整段编掉 —— 一个藏在环境变量后面、会往用户 AppData 写文件的开关不该随发行版出去。
/// 进度链路是异步的(覆盖层 / 轮询 / 内核日志),出问题时光看界面分不清"没跑"还是"卡住"。
pub(crate) fn trace(message: impl AsRef<str>) {
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("ATELIER_TRACE").is_none() { return }
        let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
        let path = Path::new(&base).join("Atelier").join("trace.log");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(file, "{stamp} {}", message.as_ref());
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = message.as_ref();
    }
}

/// 让其它流程（目前是卸载）复用同一条覆盖层进度通道。
/// 卸载要删的是几万个文件，必须把这活儿挪出 UI 线程，否则窗口直接"未响应"。
pub(crate) fn report(app: &AppHandle, phase: &'static str, label: impl Into<String>, percent: Option<f32>, log: Option<String>) {
    let mut progress = Progress::phase(phase, label);
    progress.percent = percent;
    progress.log = log;
    emit(app, progress);
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

/// How one downloaded asset becomes files under the install root.
#[derive(Clone, Copy, PartialEq)]
enum Placement {
    /// Domain artifacts carry `payload/` at the top, and `payload/*` mirrors the
    /// install root (`atelier-web.exe`, `engine-root/`, `pipeline-plugins/`, …).
    Tree,
    /// The kernel runtime zip wraps everything in `kernel-runtime/`. The shipped
    /// distribution flattens it (`atelier-kernel.exe` sits next to `start.ps1`),
    /// so the wrapper is stripped rather than installed.
    Kernel,
    /// A wheel is a file the distribution keeps as-is under `sdk/`. Unpacking it
    /// would leave `atelier_sdk/` in the root, which is not the layout.
    Sdk,
}

struct Item {
    name: &'static str,
    repo: String,
    release: String,
    asset: String,
    size: u64,
    placement: Placement,
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

/// The domain list the distribution starts with: `kernel_boot.free.json` at the same tag.
/// Taken from the release instead of being generated here, so the commands the kernel runs
/// are always the ones that tag was assembled and tested with.
fn variant_boot(tag: &str) -> Result<String, String> {
    let path = format!("repos/{COMPOSITION_REPO}/contents/kernel_boot.free.json?ref={tag}");
    let raw = gh(&["api", &path, "-H", "Accept: application/vnd.github.raw"])?;
    let value = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|e| format!("启动配置解析失败：{e}"))?;
    let domains = value.get("domains").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0);
    if domains == 0 {
        return Err("启动配置里没有域声明，拒绝安装".into());
    }
    Ok(raw)
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
            placement: Placement::Tree,
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
        placement: Placement::Kernel,
    });
    let sdk = format!("atelier_sdk-{}-py3-none-any.whl", composition.sdk_version);
    items.push(Item {
        name: "Python SDK",
        repo: KERNEL_REPO.into(),
        release: kernel_release,
        size: sizes.get(&sdk).copied().unwrap_or(0),
        asset: sdk,
        placement: Placement::Sdk,
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

/// `gh` 的失败信息里带一整条带签名的下载 URL,对用户毫无意义,也长到把界面撑爆。
/// 只留最后一段真正的原因(例如 `EOF`、`403`、`no assets match the file pattern`)。
fn reason_of(stderr: &str) -> String {
    let tail = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let tail = tail.trim_start_matches(':').trim();
    if tail.is_empty() { "网络中断".to_string() } else { tail.chars().take(140).collect() }
}

/// 一次下载尝试:启动 `gh`、轮询文件大小喂进度条、等它退出。
fn fetch_once(
    app: &AppHandle, item: &Item, dir: &Path, index: usize, count: usize, base: u64, total_all: u64,
) -> Result<(), String> {
    let target = dir.join(&item.asset);
    // 上一次失败留下的半截文件会让第一帧读到满尺寸 → 进度先跳高再回退。每次重试都清掉。
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
                    return Err(reason_of(&err));
                }
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(format!("等待下载进程失败：{e}")),
        }
    }
    if !target.is_file() { return Err(format!("下载后找不到 {}", item.asset)) }
    Ok(())
}

/// Download one asset, polling the growing file so the overlay can show bytes and rate.
///
/// GitHub 的资源 CDN 会偶发 `EOF`/`connection reset`,一次失败就放弃整个安装太脆;
/// 退避重试 4 次(2s/4s/8s),重试时界面上明确写"第几次",不装作没发生。
const DOWNLOAD_ATTEMPTS: usize = 4;

fn download(app: &AppHandle, item: &Item, dir: &Path, index: usize, count: usize, base: u64, total_all: u64) -> Result<PathBuf, String> {
    let target = dir.join(&item.asset);
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
    }
    let mut last = String::new();
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        if attempt > 1 {
            let backoff = 1u64 << (attempt as u32 - 1); // 2s / 4s / 8s
            emit(app, Progress::phase("download", format!("{} 重试中（{}）", item.name, attempt))
                .with_bytes(base, Some(total_all), None)
                .with_detail(format!("组件 {}/{} · 第 {}/{} 次尝试", index + 1, count, attempt, DOWNLOAD_ATTEMPTS))
                .log(format!("{} 下载中断（{}），{} 秒后第 {} 次重试", item.asset, last, backoff, attempt)));
            thread::sleep(Duration::from_secs(backoff));
        }
        match fetch_once(app, item, dir, index, count, base, total_all) {
            Ok(()) => return Ok(target),
            Err(error) => last = error,
        }
    }
    Err(format!("下载 {} 失败（已重试 {} 次）：{}", item.asset, DOWNLOAD_ATTEMPTS, last))
}

/// Compare against the published `.sha256` sidecar; a missing sidecar is a hard failure.
fn verify(app: &AppHandle, item: &Item, file: &Path) -> Result<(), String> {
    let dir = file.parent().ok_or("缓存目录缺失")?;
    let sidecar = format!("{}.sha256", item.asset);
    // 旁挂也可能碰上同一个 CDN 抖动,所以同样重试;三次都失败才下结论,
    // 而且要分清"发布方没发"和"网络没拿到"——以前两者都报成前者,误导排查。
    let mut failure = String::new();
    let mut status = None;
    for attempt in 0..3 {
        if attempt > 0 { thread::sleep(Duration::from_secs(1 << attempt)) }
        let output = hidden("gh")
            .args(["release", "download", &item.release, "--repo", &item.repo,
                   "--pattern", &sidecar, "--dir", &dir.to_string_lossy(), "--clobber"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("无法下载校验文件：{e}"))?;
        if output.status.success() { status = Some(true); break }
        failure = reason_of(&String::from_utf8_lossy(&output.stderr));
    }
    if status.is_none() {
        // Domains publish `<asset>.sha256`; the kernel release does not (yet). Refuse to skip
        // verification for a domain, but do not brick the update on the kernel runtime — the
        // gap is tracked and will be closed by publishing sidecars there.
        if item.repo == KERNEL_REPO {
            emit(app, Progress::phase("verify", format!("跳过校验 {}", item.name))
                .log(format!("{} 没有可用的 sha256 旁挂（{}），本次不做逐字节校验", item.asset, failure)));
            return Ok(());
        }
        if failure.contains("no assets match") || failure.contains("not found") {
            return Err(format!("发布方没有提供 {} 的 sha256 旁挂，拒绝安装", item.asset));
        }
        return Err(format!("无法下载 {} 的校验文件：{}", item.asset, failure));
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

/// Move everything under `src` into `dst`, merging directories that exist on both sides.
///
/// Only renames are used: the scratch tree and the assembled tree live on the target
/// disk, so this costs no extra space and no extra copy time.
fn merge_tree(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            merge_tree(&entry.path(), &target)?;
            let _ = fs::remove_dir(entry.path());
        } else {
            let _ = fs::remove_file(&target);
            fs::rename(entry.path(), &target)
                .map_err(|e| format!("无法就位 {}：{e}", target.display()))?;
        }
    }
    Ok(())
}

/// Unpack one zip into `scratch`. Windows ships bsdtar, which reads zip; PowerShell is
/// only the fallback for the rare machine where it is missing.
fn unzip(file: &Path, scratch: &Path) -> Result<(), String> {
    let status = hidden("tar")
        .args(["-xf", &file.to_string_lossy(), "-C", &scratch.to_string_lossy()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status();
    if matches!(status, Ok(s) if s.success()) { return Ok(()) }
    let script = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        file.display(), scratch.display());
    let fallback = hidden("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status();
    if matches!(fallback, Ok(s) if s.success()) { Ok(()) } else { Err(format!("解压 {} 失败", file.display())) }
}

/// Unpack one component and move its payload into the assembled tree.
///
/// The tree built here is exactly what the shipped distribution zip contains, because
/// that is what `start.ps1`, the kernel configuration and the domain hosts all assume:
/// `atelier-kernel.exe` at the root, `kernel_boot.json`, `engine-root/`, `dist/`, …
fn extract(item: &Item, file: &Path, staging: &Path, tree: &Path) -> Result<u64, String> {
    let started = Instant::now();
    if matches!(item.placement, Placement::Sdk) {
        // A wheel is installed as a file, never expanded.
        let sdk = tree.join("sdk");
        fs::create_dir_all(&sdk).map_err(|e| e.to_string())?;
        let target = sdk.join(&item.asset);
        fs::copy(file, &target).map_err(|e| format!("无法放置 {}：{e}", target.display()))?;
        return Ok(item.size / started.elapsed().as_secs().max(1));
    }
    let scratch = staging.join(format!("raw-{}", file.file_stem().unwrap_or_default().to_string_lossy()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    unzip(file, &scratch)?;
    let payload = if scratch.join("payload").is_dir() { scratch.join("payload") } else { scratch.clone() };
    if matches!(item.placement, Placement::Kernel) {
        // The kernel release wraps everything in `kernel-runtime/`, and the distribution takes
        // exactly one file out of it. Its docs, template and release notes stay in the zip: the
        // installed root belongs to the distribution, not to the kernel's own packaging.
        let exe = payload.join("kernel-runtime").join("atelier-kernel.exe");
        let exe = if exe.is_file() { exe } else { payload.join("atelier-kernel.exe") };
        if !exe.is_file() {
            return Err("Kernel 运行时里没有 atelier-kernel.exe，发布包不完整".into());
        }
        let target = tree.join("atelier-kernel.exe");
        let _ = fs::remove_file(&target);
        fs::copy(&exe, &target).map_err(|e| format!("无法放置 {}：{e}", target.display()))?;
        let _ = fs::remove_dir_all(&scratch);
        return Ok(item.size / started.elapsed().as_secs().max(1));
    }
    merge_tree(&payload, tree)?;
    let _ = fs::remove_dir_all(&scratch);
    Ok(item.size / started.elapsed().as_secs().max(1))
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

/// Swap the entries of `staged` into `target`. `data/` and `models/` are the user's and are
/// never touched; `engine-root/` is merged child by child so the built environments (`.venvs`)
/// and anything the engines domain wrote stay on disk without being re-downloaded.
fn swap_tree(staged: &Path, target: &Path, backup: &Path, journal: &mut Journal) -> Result<(), String> {
    let entries = fs::read_dir(staged).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        // Only the user's own directories are preserved wholesale.
        if matches!(name_str.as_str(), "data" | "models") {
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
                if matches!(child_str.as_str(), ".venvs" | "data" | "models") { continue }
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

/// Directories left in the install root by launcher versions that extracted payloads and the
/// SDK wheel straight into it. The distribution never has them at the top level, so they are
/// removed when a new train is swapped in — otherwise two copies of the runtime would coexist.
fn retire_legacy(install: &Path) {
    if let Ok(entries) = fs::read_dir(install) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let legacy = name.eq_ignore_ascii_case("kernel-runtime")
                || name.eq_ignore_ascii_case("atelier_sdk")
                || (name.starts_with("atelier_sdk-") && name.ends_with(".dist-info"));
            if !legacy { continue }
            let _ = fs::remove_dir_all(entry.path()).or_else(|_| fs::remove_file(entry.path()));
        }
    }
}

/// 预热:内核装配里凡是暴露 `<id>.prewarm` 的域都调一次。失败只记日志,绝不影响更新结果。
fn prewarm_domains(app: &AppHandle, port: u16) -> usize {
    let mut started = 0usize;
    let mut from_receipts = 0usize;
    let mut receipts = 0usize;   // 各域回执 started 之和(取不到就按调用数兜底)
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
                        receipts += 1;
                        from_receipts += targets as usize;
                        // 回执里的原始 JSON 不该出现在界面上(用户看的是"在做什么")。
                        // started=0 表示这次没新建环境:可能是全建好了,也可能是之前那次预热
                        // 还在建,所以拿环境账本说清楚,别让日志自相矛盾。
                        let note = if targets == 0 {
                            match crate::variant_runtime::install_dir().map(|root| env_progress(&root)) {
                                Ok((ready, total, missing)) if total > 0 && ready < total =>
                                    format!("运行环境 {ready}/{total}，还在等 {}", missing.join("、")),
                                _ => "环境已全部就绪，沿用已有的".to_string(),
                            }
                        } else {
                            format!("{targets} 个环境要构建")
                        };
                        emit(app, Progress::phase("env", format!("{id} 预热已受理")).log(note));
                    }
                }
            }
            Err(error) => emit(app, Progress::phase("env", format!("{id}.prewarm 无法连接"))
                .log(format!("{} prewarm 请求失败: {}", id, error))),
        }
    }
    // 有回执就以回执为准(0 也算数:已全部就绪,不必等任何 ready)
    if receipts > 0 { from_receipts } else { started }
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
/// 环境就绪情况,直接读域的账本而不是数日志:
/// `engine-environments.json` 是 engines 域建好一个环境后写的(键 `<env>/<platform>`,
/// 值是 python 路径);`environments/<env>/windows-x86_64` 是发行包里声明要建的清单。
/// 两边的交集就是这个安装"该有几个环境 / 已经有几个"。
fn env_progress(root: &Path) -> (usize, usize, Vec<String>) {
    let specs = root.join("engine-root").join("domains").join("engines").join("environments");
    let names: Vec<String> = match fs::read_dir(&specs) {
        Ok(entries) => entries.flatten()
            .filter(|entry| entry.path().join("windows-x86_64").is_dir())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() { return (0, 0, Vec::new()) }
    let registry = root.join("engine-root").join(".venvs").join("engine-environments.json");
    let built = fs::read_to_string(&registry).ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut missing = Vec::new();
    let mut ready = 0usize;
    for name in &names {
        let done = built.iter().any(|(key, entry)| {
            key.split('/').next() == Some(name.as_str())
                && entry.get("python").and_then(|path| path.as_str())
                    .map(|path| Path::new(path).is_file()).unwrap_or(false)
        });
        if done { ready += 1 } else { missing.push(name.clone()) }
    }
    (ready, names.len(), missing)
}

/// 轮询 /api/logs 抓 `[PREWARM]` 行(web 的日志既有 SSE 也有历史接口,轮询更稳):
/// 每 2 秒刷一次覆盖层,速率用 uv 缓存的增长算,就绪数读环境账本。
///
/// **什么时候收工**:
/// 1. 成功:`engine-environments.json` 里的就绪数 >= 发行包声明的环境数(读账本,不数日志);
/// 2. 空转:连续 300 秒既没有新的 `[PREWARM]` 行、uv 缓存也没有增长 —— 两条都不动了才算卡住,
///    只静一边不算(下载时会闷头刷缓存,编译 wheel 时会闷头不写缓存);
/// 3. 硬上限 6 小时,防止某个环境一直失败时挂死。
///
/// `expected` 是回执说"要等几个 ready"。回执给 0 有两种含义——真的都就绪了,或者构建
/// 已经在跑(预热是幂等的,重复请求不会新建环境)。前一种该立刻收工,后一种要接着看,
/// 所以只有调用方明确要等(`wait_for_unknown`)时才继续轮询。
///
/// 关键:每次只发**一条**带速率的事件。以前速率和日志分两条发,日志那条不带 rate,
/// 界面上的实时速度就会被下一条事件抹掉 —— 看起来像"速度被删了"。
fn poll_prewarm(app: &AppHandle, port: u16, expected: usize, wait_for_unknown: bool) {
    trace(format!("poll: enter expected={expected} wait_unknown={wait_for_unknown}"));
    if expected == 0 && !wait_for_unknown { trace("poll: nothing to wait for"); return }
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20)).no_proxy().build() else {
        trace("poll: client build failed");
        return
    };
    trace("poll: client ready");
    let url = format!("http://127.0.0.1:{port}/api/logs?limit=200");
    let started = Instant::now();
    // 日志边界:先取当前最新 id 作为基线,避免把历史 ready 行当成这次的事件。
    let mut last_id = 0u64;
    if let Ok(body) = client.get(&url).send().and_then(|r| r.json::<serde_json::Value>()) {
        if let Some(rows) = body.get("logs").and_then(|l| l.as_array()) {
            last_id = rows.iter()
                .filter_map(|row| row.get("id").and_then(|v| v.as_u64()))
                .max()
                .unwrap_or(0);
        }
    }
    let mut last_message = String::new();
    // 缓存体积在**独立线程**里量:`.uv-cache` 是几万个文件,在机械盘上走一遍要几十秒。
    // 以前在轮询线程里同步量,第一帧就被卡住,界面停在回执那一句上好几分钟不动。
    let stop_sampler = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sample: std::sync::Arc<std::sync::Mutex<Option<(u64, Instant)>>> = Default::default();
    if let Ok(root) = crate::variant_runtime::install_dir() {
        let cache = root.join("engine-root").join(".venvs").join(".uv-cache");
        let sample = sample.clone();
        let stop = stop_sampler.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let size = dir_bytes(&cache);
                if let Ok(mut slot) = sample.lock() {
                    *slot = Some((size, Instant::now()));
                }
                for _ in 0..8 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) { return }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
    }
    let mut rate = 0u64;
    let mut previous_sample: Option<(u64, Instant)> = None;
    let mut last_activity = Instant::now();
    trace(format!("poll: baseline id={last_id}"));

    // 只保留一个很宽的总上限:环境构建可能几十分钟,不能因为"暂时没新日志"就提前进界面。
    while started.elapsed() < std::time::Duration::from_secs(6 * 3600) {
        thread::sleep(Duration::from_secs(2));
        // 速率只读采样线程的最近结果,这里绝不走目录。
        let latest = sample.lock().ok().and_then(|slot| *slot);
        if let (Some((size, at)), Some((before, at_before))) = (latest, previous_sample) {
            let elapsed = at.duration_since(at_before).as_secs_f64();
            if elapsed >= 0.5 {
                if size > before {
                    rate = ((size - before) as f64 / elapsed) as u64;
                    // 缓存在长就说明还在干活:编译 wheel 时日志会安静几分钟,不能因此判空转。
                    last_activity = Instant::now();
                } else {
                    rate = 0;
                }
                previous_sample = Some((size, at));
            }
        } else if let Some(latest) = latest {
            previous_sample = Some(latest);
        }
        if let Ok(body) = client.get(&url).send().and_then(|r| r.json::<serde_json::Value>()) {
            if let Some(rows) = body.get("logs").and_then(|l| l.as_array()) {
                // 按 id 升序处理:一次轮询里可能同时来了多条 ready,倒序迭代会漏数。
                let mut fresh: Vec<(u64, String)> = rows.iter()
                    .filter_map(|row| {
                        let id = row.get("id").and_then(|v| v.as_u64())?;
                        let message = row.get("message").and_then(|m| m.as_str())?;
                        (id > last_id && message.contains("[PREWARM]"))
                            .then(|| (id, message.chars().take(180).collect::<String>()))
                    })
                    .collect();
                fresh.sort_by_key(|(id, _)| *id);
                if !fresh.is_empty() { last_activity = Instant::now() }
                for (id, message) in fresh {
                    last_id = id;
                    last_message = message;
                }
            }
        }
        trace(format!("poll: got logs, last_message={:?}", last_message.chars().take(60).collect::<String>()));
        let (ready, total, missing) = crate::variant_runtime::install_dir()
            .map(|root| env_progress(&root)).unwrap_or((0, 0, Vec::new()));
        trace(format!("poll: env {ready}/{total}"));
        // 卡在 7/8 时要说清楚在等谁,不然只能看到一个不动的数字。
        let detail = if total > 0 && !missing.is_empty() {
            format!("运行环境 {ready}/{total} 就绪 · 还在等 {}", missing.join("、"))
        } else if total > 0 {
            format!("运行环境 {ready}/{total} 就绪")
        } else {
            format!("{ready} 个环境已就绪")
        };
        emit(app, Progress::phase("env", "构建运行环境")
            .with_rate(rate)
            .with_detail(detail)
            .log(last_message.clone()));
        trace("poll: emitted");
        if total > 0 && ready >= total { trace("poll: all ready"); break }
        // 有环境构建失败时 ready 永远到不了 total:连续五分钟没有新的预热日志就当结束了
        // (构建本身在 kernel 里继续,界面不该无限挂着)。
        if last_activity.elapsed() > std::time::Duration::from_secs(300) { break }
    }
    // 收尾时停掉采样线程,别让它跟着进程一直空转。
    stop_sampler.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 目标盘剩余空间。走一次 PowerShell,避免为一次查询引 sysinfo 的 disk feature。
fn free_bytes(dir: &Path) -> Option<u64> {
    let letter = dir.components().next()
        .and_then(|c| match c {
            std::path::Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().trim_end_matches('\\').to_string()),
            _ => None,
        })?;
    let script = format!("(Get-PSDrive -Name '{}').Free", letter.replace(':', ""));
    let out = hidden("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}
/// The whole flow: download → verify → stage → swap, with rollback through a journal.
pub fn run(app: &AppHandle) -> Result<String, String> { run_with(app, true) }

/// start=false 只装不启动、不预热（首次安装用：装完先进卡片页，再单独预热）。
pub fn run_with(app: &AppHandle, start: bool) -> Result<String, String> {
    emit(app, Progress::phase("resolve", "正在读取组合配置"));
    let tag = latest_tag()?;
    let version = tag.trim_start_matches('v').to_string();
    let composition = composition(&tag)?;
    let plan = plan(&composition);
    let total: u64 = plan.items.iter().map(|i| i.size).sum();
    emit(app, Progress::phase("plan", format!("发行版 {}（{} 个组件）", plan.train, plan.items.len()))
        .with_bytes(0, Some(total), None)
        .with_detail(format!("{} / {} 个组件", 0, plan.items.len())));

    let dir = cache_dir(&plan.train)?;
    let install = install_dir()?;
    // 磁盘预检放在下载之前:组件本体 + 运行环境预留。不够就别开始下,免得下到一半失败。
    const ENV_RESERVE: u64 = 30 * 1024 * 1024 * 1024;
    if let Some(free) = free_bytes(&install) {
        let need = total.saturating_add(ENV_RESERVE);
        if free < need {
            let msg = format!("磁盘空间不足：需要约 {:.0} GB，可用 {:.0} GB",
                need as f64 / 1e9, free as f64 / 1e9);
            emit(app, Progress::phase("fail", msg.clone()).log(msg.clone()));
            return Err(msg);
        }
    }
    let staging = install.join(".staging").join(&plan.train);
    let _ = fs::remove_dir_all(&staging);
    // 组装树:所有组件的 payload 摊进同一棵树,最后一步整体换进安装目录。
    let tree = staging.join("tree");
    fs::create_dir_all(&tree).map_err(|e| e.to_string())?;
    let count = plan.items.len();
    let mut bytes = 0u64;
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
        let rate = extract(item, &file, &staging, &tree)?;
        emit(app, Progress::phase("stage", format!("{} 已就位", item.name))
            .with_bytes(bytes, Some(total), None)
            .with_percent(((index as f32 + 1.0) / (count.max(1) as f32)) * 100.0)
            .with_rate(rate)
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name))
            .log(format!("{} 已就位", item.asset)));
    }
    // 启动配置来自同一个 tag:域清单、命令路径都由发行版自己写,启动器不硬编码,
    // 免得组出来的树和域 artifact 对不上。
    emit(app, Progress::phase("stage", "写入启动配置").log(format!("读取 {} 的 kernel_boot.free.json", tag)));
    let boot = variant_boot(&tag)?;
    fs::write(tree.join("kernel_boot.json"), boot).map_err(|e| format!("无法写入 kernel_boot.json：{e}"))?;

    emit(app, Progress::phase("swap", "正在换入发行文件"));
    // 只保留最新一份退路:换入前清掉更早的 .old-*（每个约 700 MB）。
    let keep = format!(".old-{}", plan.train);
    if let Ok(entries) = fs::read_dir(&install) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".old-") && name != keep {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    let backup = install.join(format!(".old-{}", plan.train));
    let _ = fs::remove_dir_all(&backup);
    let mut journal = Journal::default();
    if let Err(error) = swap_tree(&tree, &install, &backup, &mut journal) {
        journal.rollback();
        return Err(format!("换入失败，已回滚：{error}"));
    }
    // 换入成功后才清旧布局:失败回滚时那些文件还是唯一的运行时。
    retire_legacy(&install);
    for name in [crate::contract::DATA, crate::contract::MODELS] {
        let _ = fs::create_dir_all(install.join(name));
    }
    // 版本号和暂存区在启动之前落地:首次预暖可能跑几十分钟,用户中途关掉窗口也必须
    // 留下一个"已装好"的目录,不能因为预热被打断就变成没版本号的半成品。
    fs::write(install.join("VERSION"), format!("{version}\n")).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&staging);
    // 组件换进来以后"程序"那一段就变了,后台重算一次占用。
    crate::storage::refresh(app.clone());
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
    }    // 更新即启动；首次安装(start=false)交给调用方在卡片页之后单独预热。
    let mut live: Option<u16> = None;
    if start {
    emit(app, Progress::phase("start", "启动实例"));
    match tauri::async_runtime::block_on(crate::variant_runtime::open_workbench(None, None)) {
        Ok(port) => {
            live = Some(port);
            emit(app, Progress::phase("check", "实例已就绪").log(format!("四域 active，端口 {port}")));
            let targets = prewarm_domains(app, port);
            emit(app, Progress::phase("env", format!("预热目标 {targets} 个")).log(format!("发现 {targets} 个 <domain>.prewarm")));
            poll_prewarm(app, port, targets, false);
        }
        Err(error) => emit(app, Progress::phase("fail", "实例启动失败".to_string())
            .log(format!("已更新完成，但启动实例失败：{error}"))),
    }
    }    let env_note = if env_total > 0 { format!("运行环境 {env_ready}/{env_total} 就绪") } else { "无域环境".to_string() };
    emit(app, Progress::phase("done", format!("已更新到 {version}")).with_detail(env_note)
        .with_bytes(total, Some(total), None)
        .log(format!("VERSION → {version}")));
    Ok(match live {
        Some(port) => format!("已更新到 {version}，已启动实例（端口 {port}），旧组件留在 {}", backup.display()),
        None => format!("已更新到 {version}，实例未启动，旧组件留在 {}", backup.display()),
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

/// 首次安装：只装（下载/校验/暂存/换入/写 VERSION），不启动、不预热。
#[tauri::command]
pub async fn install_only(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let result = run_with(&app, false);
        if let Err(error) = &result {
            emit(&app, Progress::phase("fail", format!("安装失败：{error}")).log(error.clone()));
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 启动实例 + 预热环境的主体。命令和排障钩子共用这一份,避免两条路径行为不一致。
pub(crate) fn prewarm_blocking(app: &AppHandle) -> Result<Option<u16>, String> {
    trace("prewarm: begin");
    emit(app, Progress::phase("start", "启动实例"));
    let port = tauri::async_runtime::block_on(crate::variant_runtime::open_workbench(None, None))?;
    trace(format!("prewarm: instance on {port}"));
    let targets = prewarm_domains(app, port);
    trace(format!("prewarm: targets={targets}"));
    // 手动「准备运行环境」时,即使回执说 0 也接着看:可能构建已经在跑。
    poll_prewarm(app, port, targets, true);
    trace("prewarm: poll returned");
    // 轮询退出时要给一个终态事件,否则覆盖层会一直停在最后一帧、数字再也不动。
    let (ready, total, missing) = crate::variant_runtime::install_dir()
        .map(|root| env_progress(&root)).unwrap_or((0, 0, Vec::new()));
    let (label, log) = if total > 0 && ready < total {
        (format!("运行环境 {ready}/{total} 就绪"),
         format!("还在等 {}；构建由 kernel 继续，可以关掉窗口", missing.join("、")))
    } else if total > 0 {
        (format!("运行环境 {ready}/{total} 就绪"), "运行环境已全部就绪".to_string())
    } else {
        ("预热结束".to_string(), String::new())
    };
    emit(app, Progress::phase("done", label).with_detail(log.clone()).log(log));
    // 环境建好/变大以后"程序"那一段也变了。
    crate::storage::refresh(app.clone());
    Ok(Some(port))
}

/// 只做"启动实例 + 预热环境"，用于首次安装在卡片页之后那一拍。
#[tauri::command]
pub async fn prewarm_only(app: AppHandle) -> Result<Option<u16>, String> {
    tauri::async_runtime::spawn_blocking(move || prewarm_blocking(&app))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 下载缓存里组件最全的一份（本机跑过一次就会有）。没有就跳过，CI 上不存在。
    fn cached_artifacts() -> Option<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Atelier").join("download-cache"))?;
        // 缓存必须是**成套**的:内核 + SDK + 至少一个域 + 视频插件。被清过一半的目录不算,
        // 否则测试会因为"少了个内核包"这种环境问题红掉,而不是产品真出问题。
        let complete = |path: &Path| -> bool {
            let Ok(entries) = fs::read_dir(path) else { return false };
            let names: Vec<String> = entries.flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect();
            names.iter().any(|name| name.starts_with("Atelier-Kernel-runtime"))
                && names.iter().any(|name| name.ends_with(".whl"))
                && names.iter().any(|name| name.starts_with("AtelierWeb-"))
                && names.iter().any(|name| name.starts_with("AtelierEngines-"))
                && names.iter().any(|name| name.starts_with("AtelierVideoPlugin-"))
        };
        let mut best: Option<(usize, PathBuf)> = None;
        for entry in fs::read_dir(base).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() || !complete(&path) { continue }
            let count = fs::read_dir(&path).ok().map(|dir| dir.flatten().count()).unwrap_or(0);
            if best.as_ref().map(|(seen, _)| count > *seen).unwrap_or(true) {
                best = Some((count, path));
            }
        }
        best.map(|(_, path)| path)
    }

    fn item_for(path: &Path) -> Item {
        let asset = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let placement = if asset.starts_with("Atelier-Kernel-runtime") {
            Placement::Kernel
        } else if asset.ends_with(".whl") || asset.ends_with(".tgz") {
            Placement::Sdk
        } else {
            Placement::Tree
        };
        Item {
            name: "test",
            repo: String::new(),
            release: String::new(),
            size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            asset,
            placement,
        }
    }

    /// 组装出来的目录必须和发行包同一套布局：内核摊在根、engine-root 在、sdk 是文件、
    /// kernel-runtime 这个包装层不许留。这是「壳能装、装完能跑」的那条底线。
    #[test]
    fn assembling_cached_artifacts_matches_the_distribution_layout() {
        let Some(cache) = cached_artifacts() else { return };
        let staging = std::env::temp_dir().join("atelier-assemble-test");
        let _ = fs::remove_dir_all(&staging);
        let tree = staging.join("tree");
        fs::create_dir_all(&tree).unwrap();

        let mut seen = 0;
        for entry in fs::read_dir(&cache).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !(name.starts_with("Atelier") || name.ends_with(".whl")) || path.extension().is_none() {
                continue;
            }
            if path.extension().unwrap() != "zip" && !name.ends_with(".whl") { continue }
            extract(&item_for(&path), &path, &staging, &tree).unwrap();
            seen += 1;
        }
        assert!(seen >= 3, "缓存里应有多个 artifact，实际 {seen}");

        // 内核:包装层被摊平，二进制在根。
        assert!(tree.join("atelier-kernel.exe").is_file(), "atelier-kernel.exe 应该在根目录");
        assert!(!tree.join("kernel-runtime").exists(), "kernel-runtime/ 包装层不应保留");
        assert!(!tree.join("kernel_boot.template.json").exists(), "内核包自带的模板不属于安装目录");
        assert!(!tree.join("docs").exists(), "内核包自带的文档不属于安装目录");
        // 域本体各就各位。
        assert!(tree.join("atelier-web.exe").is_file());
        assert!(tree.join("atelier-scheduler").is_dir());
        assert!(tree.join("engine-root").join("domains").join("engines").is_dir(), "engine-root 必须装进来");
        assert!(tree.join("pipeline-plugins").join("video_translate").is_dir());
        // SDK 是文件，不是被解开的包。
        let sdk = fs::read_dir(tree.join("sdk")).unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "whl"))
            .count();
        assert_eq!(sdk, 1, "sdk/ 里应当有一个 wheel");
        assert!(!tree.join("atelier_sdk").exists(), "wheel 不该被解开成 atelier_sdk/");

        let _ = fs::remove_dir_all(&staging);
    }

    /// 换入时 data/models 不动，engine-root 只换代码、保留已建环境。
    #[test]
    fn swap_keeps_user_directories_and_built_environments() {
        let base = std::env::temp_dir().join("atelier-swap-test");
        let _ = fs::remove_dir_all(&base);
        let staged = base.join("staged");
        let target = base.join("target");
        let backup = base.join("backup");
        fs::create_dir_all(staged.join("engine-root").join("domains")).unwrap();
        fs::create_dir_all(staged.join("engine-root").join(".venvs")).unwrap();
        fs::create_dir_all(staged.join("data")).unwrap();
        fs::create_dir_all(staged.join("models")).unwrap();
        fs::write(staged.join("kernel_boot.json"), "{}").unwrap();
        fs::write(staged.join("engine-root").join("domains").join("new.txt"), "new").unwrap();
        fs::write(staged.join("engine-root").join(".venvs").join("fresh.txt"), "fresh").unwrap();
        fs::create_dir_all(target.join("engine-root").join(".venvs")).unwrap();
        fs::write(target.join("engine-root").join(".venvs").join("built.txt"), "built").unwrap();
        fs::create_dir_all(target.join("engine-root").join("domains")).unwrap();
        fs::write(target.join("engine-root").join("domains").join("old.txt"), "old").unwrap();
        fs::create_dir_all(target.join("data")).unwrap();
        fs::write(target.join("data").join("keep.db"), "user").unwrap();

        let mut journal = Journal::default();
        swap_tree(&staged, &target, &backup, &mut journal).unwrap();

        assert!(target.join("kernel_boot.json").is_file());
        assert!(target.join("engine-root").join(".venvs").join("built.txt").is_file(), "已建环境不能被覆盖");
        assert!(!target.join("engine-root").join(".venvs").join("fresh.txt").exists());
        assert!(target.join("engine-root").join("domains").join("new.txt").is_file());
        assert!(!target.join("engine-root").join("domains").join("old.txt").exists(), "旧代码应被换走");
        assert!(target.join("data").join("keep.db").is_file(), "用户数据必须留下");
        assert!(!target.join("data").join("models").exists());

        // 回滚:新进来的文件撤掉，被暂存的旧文件放回。
        journal.rollback();
        assert!(target.join("engine-root").join("domains").join("old.txt").is_file());
        assert!(!target.join("engine-root").join("domains").join("new.txt").exists());
        let _ = fs::remove_dir_all(&base);
    }
}