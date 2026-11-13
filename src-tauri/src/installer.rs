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
    /// 镜像 release 的 tag（形如 `v0.1.20`）。发布脚本会写；缺省时退回任一域的分支。
    #[serde(default)]
    release: String,
    artifacts: BTreeMap<String, Component>,
}

impl Composition {
    fn release_tag(&self) -> String {
        let trimmed = self.release.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
        self.artifacts.values().next().map(|item| item.release.clone()).unwrap_or_default()
    }
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
    /// 完整下载地址；sidecar 校验用同一个地址加 `.sha256`。
    url: String,
    asset: String,
    size: u64,
    placement: Placement,
    /// 内核 release 没有 sha256 旁挂，这一项跳过逐字节校验（其余一律拒绝安装）。
    kernel: bool,
}

/// 组件从哪来：Free 仓库的 release 把各域 artifact 镜像成**公开**资产，启动器直接走
/// `https://github.com/{repo}/releases/download/{tag}/{asset}`。
///
/// 这里不再依赖 GitHub CLI：普通用户机器上根本不会装 `gh`，以前第一次点安装就卡在
/// 「无法执行 gh」。整条链路只用 HTTPS，唯一的外部程序是系统自带的 `certutil`（算 sha256）。
const COMPOSITION_REPO: &str = "NomadAnget/AtelierFree";
/// Where the kernel runtime and the Python SDK come from.
const KERNEL_REPO: &str = "NomadAnget/Atelier-Kernel";
/// 引导入口：最新 release 里的两个小文件。只有它们走 `latest`；组件全部按清单里的 tag 取，
/// 保证一次安装用的始终是同一版（release 里的资产不可变）。
const MANIFEST_URL: &str = "https://github.com/NomadAnget/AtelierFree/releases/latest/download/artifacts.json";
const BOOT_ASSET: &str = "kernel_boot.free.json";

/// Spawn without a console window: a flood of cmd windows is not acceptable.
fn hidden(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

/// 出网客户端走统一入口：真机常见"浏览器能开、启动器打不开"，是系统代理没被使用。
fn client() -> Result<reqwest::blocking::Client, String> {
    crate::net::client(1800)
}

fn asset_url(repo: &str, release: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{release}/{asset}")
}

fn http_text(url: &str) -> Result<String, String> {
    let response = client()?.get(url).send()
        .map_err(|e| format!("无法连接下载服务：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载服务返回 {}（{}）", status.as_u16(), url.rsplit('/').next().unwrap_or(url)));
    }
    response.text().map_err(|e| format!("读取响应失败：{e}"))
}

/// 文件大小：给进度条一个真实分母。拿不到就返回 None，界面显示不确定态而不是瞎猜。
fn http_size(url: &str) -> Option<u64> {
    let response = client().ok()?.head(url).send().ok()?;
    if !response.status().is_success() { return None }
    response.headers().get(reqwest::header::CONTENT_LENGTH)?
        .to_str().ok()?.parse().ok()
}

/// 清单缓存：上次成功取到的组合配置。
///
/// 断网、代理没起来、GitHub 抽风的时候，启动器仍然能显示版本和组件列表（下载仍然要网络），
/// 不至于一打开就空白报错。
fn manifest_cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let dir = base.join("Atelier");
    let _ = fs::create_dir_all(&dir);
    Some(dir.join("artifacts-cache.json"))
}

fn composition() -> Result<Composition, String> {
    // 1) 显式覆盖：`ATELIER_MANIFEST` 可以是本地文件路径，也可以是自定义 URL。
    //    给离线安装/内网镜像留一个口子，不用改代码。
    if let Ok(raw_override) = std::env::var("ATELIER_MANIFEST") {
        let value = raw_override.trim();
        if !value.is_empty() {
            let raw = if value.starts_with("http://") || value.starts_with("https://") {
                http_text(value)?
            } else {
                fs::read_to_string(value).map_err(|e| format!("读取本地清单失败：{e}"))?
            };
            return serde_json::from_str(&raw).map_err(|e| format!("组合配置解析失败：{e}"));
        }
    }
    // 2) 正常路径：取远端，成功就刷新缓存。
    match http_text(MANIFEST_URL) {
        Ok(raw) => {
            let parsed: Composition =
                serde_json::from_str(&raw).map_err(|e| format!("组合配置解析失败：{e}"))?;
            if let Some(path) = manifest_cache_path() {
                let _ = fs::write(path, raw);
            }
            Ok(parsed)
        }
        Err(error) => {
            // 3) 兜底：用上次成功的缓存，并在界面上说清楚这是缓存。
            let cached = manifest_cache_path().and_then(|path| fs::read_to_string(path).ok());
            match cached {
                Some(raw) => {
                    let parsed: Composition = serde_json::from_str(&raw)
                        .map_err(|e| format!("组合配置解析失败：{e}"))?;
                    trace(format!("manifest: 使用缓存（远端失败 {error}）"));
                    Ok(parsed)
                }
                None => Err(format!(
                    "无法读取更新源：{error}\n\
                     如果这台机器访问 GitHub 需要代理，请先在系统代理里设置好（或设置环境变量 HTTPS_PROXY），再重试。\n\
                     启动器当前出网：{}",
                    crate::net::describe_proxy()
                )),
            }
        }
    }
}

/// 内核启动配置：同一个 release 里的 `kernel_boot.free.json`。用发行版自己的文件而不是
/// 在启动器里硬编码，免得组出来的树和域 artifact 对不上。
fn variant_boot(release: &str) -> Result<String, String> {
    let raw = http_text(&asset_url(COMPOSITION_REPO, release, BOOT_ASSET))?;
    let value = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|e| format!("启动配置解析失败：{e}"))?;
    let domains = value.get("domains").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0);
    if domains == 0 {
        return Err("启动配置里没有域声明，拒绝安装".into());
    }
    Ok(raw)
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
        let url = asset_url(&component.repo, &component.release, &component.asset);
        items.push(Item {
            name: label,
            size: http_size(&url).unwrap_or(0),
            url,
            asset: component.asset.clone(),
            placement: Placement::Tree,
            kernel: false,
        });
    }
    // 内核运行时 + SDK 来自内核仓库自己的 release（本来就是公开的）。
    let kernel_release = format!("v{}", composition.kernel_version);
    let runtime = format!("Atelier-Kernel-runtime-windows-x86_64-v{}.zip", composition.kernel_version);
    let runtime_url = asset_url(KERNEL_REPO, &kernel_release, &runtime);
    items.push(Item {
        name: "Kernel 运行时",
        size: http_size(&runtime_url).unwrap_or(0),
        url: runtime_url,
        asset: runtime,
        placement: Placement::Kernel,
        kernel: true,
    });
    let sdk = format!("atelier_sdk-{}-py3-none-any.whl", composition.sdk_version);
    let sdk_url = asset_url(KERNEL_REPO, &kernel_release, &sdk);
    items.push(Item {
        name: "Python SDK",
        size: http_size(&sdk_url).unwrap_or(0),
        url: sdk_url,
        asset: sdk,
        placement: Placement::Sdk,
        kernel: true,
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
/// 只留最后一段真正的原因(例如 `EOF`、`403`)。
fn reason_of(stderr: &str) -> String {
    let tail = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    let tail = tail.trim_start_matches(':').trim();
    if tail.is_empty() { "网络中断".to_string() } else { tail.chars().take(140).collect() }
}

/// 一次下载尝试：流式写到磁盘，边写边喂进度。
///
/// 旧实现起一个 `gh` 子进程再轮询文件大小；现在直接 HTTPS，普通用户不需要装任何东西。
fn fetch_once(
    app: &AppHandle, item: &Item, dir: &Path, index: usize, count: usize, base: u64, total_all: u64,
) -> Result<(), String> {
    let target = dir.join(&item.asset);
    // 上一次失败留下的半截文件会让第一帧读到满尺寸 → 进度先跳高再回退。每次重试都清掉。
    let _ = fs::remove_file(&target);
    let mut response = client()?.get(&item.url).send()
        .map_err(|e| format!("无法连接下载地址：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载地址返回 {}", status.as_u16()));
    }
    // 优先用响应头里的长度（比 HEAD 更准）；拿不到就退回清单里量到的值。
    let expected = response.headers().get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(item.size);

    let mut file = fs::File::create(&target).map_err(|e| format!("无法创建 {}：{e}", target.display()))?;
    let started = Instant::now();
    let mut done = 0u64;
    let mut last_emit = Instant::now();
    let mut buffer = vec![0u8; 256 * 1024];
    {
        use std::io::{Read, Write};
        loop {
            let read = response.read(&mut buffer).map_err(|e| format!("下载中断：{e}"))?;
            if read == 0 { break }
            file.write_all(&buffer[..read]).map_err(|e| format!("写入失败：{e}"))?;
            done += read as u64;
            // 每 200ms 刷一次界面就够，无需每个块都发事件。
            if last_emit.elapsed() >= Duration::from_millis(200) || done >= expected {
                last_emit = Instant::now();
                let elapsed = started.elapsed().as_secs().max(1);
                // 进度按组件数加权:每个文件各自 0→100% 占 1/count。
                let frac = if expected > 0 { (done.min(expected) as f64 / expected as f64) as f32 } else { 0.0 };
                let mut progress = Progress::phase("download", format!("正在下载 {}", item.name))
                    .with_bytes(base + done, Some(total_all), Some(done / elapsed))
                    .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name));
                progress.percent = Some(((index as f32 + frac) / (count.max(1) as f32)) * 100.0);
                emit(app, progress);
            }
        }
        file.flush().ok();
    }
    drop(file);
    if !target.is_file() { return Err(format!("下载后找不到 {}", item.asset)) }
    if expected > 0 && done < expected {
        return Err(format!("下载不完整：{} / {}", done, expected));
    }
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
    // 旁挂就在同一个 release 里，地址是资产地址加 `.sha256`；同样重试三次，
    // 并区分「发布方没发」和「网络没拿到」——以前两者都报成前者，误导排查。
    let mut failure = String::new();
    let mut expected = None;
    for attempt in 0..3 {
        if attempt > 0 { thread::sleep(Duration::from_secs(1 << attempt)) }
        match http_text(&format!("{}.sha256", item.url)) {
            Ok(text) => {
                let digest = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                if digest.len() == 64 { expected = Some(digest); }
                break;
            }
            Err(error) => failure = reason_of(&error),
        }
    }
    let Some(expected) = expected else {
        // Domains publish `<asset>.sha256`; the kernel release does not (yet). Refuse to skip
        // verification for a domain, but do not brick the update on the kernel runtime — the
        // gap is tracked and will be closed by publishing sidecars there.
        if item.kernel {
            emit(app, Progress::phase("verify", format!("跳过校验 {}", item.name))
                .log(format!("{} 没有可用的 sha256 旁挂（{}），本次不做逐字节校验", item.asset, failure)));
            return Ok(());
        }
        if failure.contains("404") {
            return Err(format!("发布包缺少 {} 的 sha256 旁挂，拒绝安装", item.asset));
        }
        return Err(format!("无法下载 {} 的校验文件：{}", item.asset, failure));
    };
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
                        // started=0 有两种含义(已全部就绪 / 上一次预热还在建),这里不下结论,
                        // 由随后订阅的 /api/environments 快照说明真实进度。
                        let note = if targets == 0 {
                            "没有新建环境，进度以实例上报为准".to_string()
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
/// 环境供给快照:由 web 域汇总引擎域的推送,启动器只读这一份,不再自己扫目录/读账本。
#[derive(Default, Clone, Debug)]
struct EnvView {
    supported: bool,
    source: String,
    stage: String,
    ready: u64,
    total: u64,
    missing: Vec<String>,
    failed: Vec<String>,
    current: Option<String>,
    rate: u64,
    log: String,
    updated: Option<f64>,
    error: Option<String>,
}

impl EnvView {
    /// 界面那一行的说明文字:永远说清"几个就绪、还在等谁"。
    fn detail(&self) -> String {
        if self.source == "ledger" {
            return match &self.error {
                Some(note) => format!("{} 个运行环境已就绪（{note}）", self.ready),
                None => format!("{} 个运行环境已就绪（等待引擎域进度）", self.ready),
            };
        }
        match self.stage.as_str() {
            "done" => format!("运行环境 {}/{} 就绪", self.ready, self.total),
            "idle" => "没有需要构建的运行环境".to_string(),
            "failed" => format!("运行环境 {}/{} 就绪，失败：{}",
                self.ready, self.total, self.failed.join("、")),
            _ if !self.missing.is_empty() => format!("运行环境 {}/{} 就绪 · 还在等 {}",
                self.ready, self.total, self.missing.join("、")),
            _ => format!("运行环境 {}/{} 就绪", self.ready, self.total),
        }
    }
}

/// 读一次 web 的 /api/environments。404 表示这套装配没有该接口,调用方据此收手。
fn environment_view(port: u16, client: &reqwest::blocking::Client) -> Result<EnvView, String> {
    let response = client.get(format!("http://127.0.0.1:{port}/api/environments"))
        .send().map_err(|error| format!("请求 /api/environments 失败: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(EnvView::default());
    }
    if !status.is_success() {
        return Err(format!("/api/environments 返回 {}", status.as_u16()));
    }
    let body: serde_json::Value = response.json()
        .map_err(|error| format!("解析 /api/environments 失败: {error}"))?;
    let text_of = |value: &serde_json::Value, key: &str| value.get(key)
        .and_then(|item| item.as_str()).map(|item| item.to_string());
    let count_of = |value: &serde_json::Value, key: &str| value.get(key)
        .and_then(|item| item.as_u64()).unwrap_or(0);
    let list_of = |value: &serde_json::Value, key: &str| value.get(key)
        .and_then(|item| item.as_array())
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let source = text_of(&body, "source").unwrap_or_else(|| "engines".to_string());
    let error = text_of(&body, "error");
    if source == "ledger" {
        // 引擎域还没推送:web 只报账本里已就绪的个数,总数与当前项都不知道。
        return Ok(EnvView {
            supported: true, source, error,
            ready: count_of(&body, "ledger_ready"),
            stage: "unknown".to_string(),
            ..Default::default()
        });
    }
    let snapshot = body.get("environments").cloned().unwrap_or(serde_json::Value::Null);
    Ok(EnvView {
        supported: true,
        source,
        stage: text_of(&snapshot, "stage").unwrap_or_else(|| "unknown".to_string()),
        ready: count_of(&snapshot, "ready"),
        total: count_of(&snapshot, "total"),
        missing: list_of(&snapshot, "missing"),
        failed: list_of(&snapshot, "failed"),
        current: text_of(&snapshot, "current"),
        rate: count_of(&snapshot, "rate"),
        log: text_of(&snapshot, "log").unwrap_or_default(),
        updated: snapshot.get("updated_at").and_then(|item| item.as_f64()),
        error: None,
    })
}

/// 订阅 web 的环境供给进度:每 2 秒一帧,到终态就收工。
///
/// 收工条件:stage=done / stage=failed 立即结束;stage=idle 等满 180 秒(管线还没声明需求,
/// 再等也没意义);6 分钟没有任何变化(构建在 kernel 里继续,界面不该无限挂着);6 小时硬上限。
///
/// 旧实现自己扫 `environments/*/windows-x86_64` 数总数、读 `engine-environments.json` 算就绪、
/// 轮询 `[PREWARM]` 日志并量 uv 缓存算速率——三处都会和引擎域的真实策略漂移。现在全部以
/// web 给的快照为准,启动器只负责渲染。
fn poll_environment(app: &AppHandle, port: u16) -> EnvView {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20)).no_proxy().build() else {
        return EnvView::default()
    };
    let started = Instant::now();
    let mut last_change = Instant::now();
    let mut fingerprint = String::new();
    let mut view = EnvView::default();
    let mut failures = 0usize;
    loop {
        thread::sleep(Duration::from_secs(2));
        if started.elapsed() > Duration::from_secs(6 * 3600) {
            emit(app, Progress::phase("env", "构建运行环境")
                .log("运行环境构建超过 6 小时，先让它继续跑"));
            break;
        }
        match environment_view(port, &client) {
            Ok(fresh) => {
                failures = 0;
                if !fresh.supported {
                    emit(app, Progress::phase("env", "构建运行环境")
                        .log("当前装配没有 /api/environments，跳过运行环境进度"));
                    break;
                }
                let next = format!("{}|{}|{}|{:.3}", fresh.stage, fresh.ready,
                    fresh.current.clone().unwrap_or_default(), fresh.updated.unwrap_or(0.0));
                if next != fingerprint {
                    fingerprint = next;
                    last_change = Instant::now();
                }
                emit(app, Progress::phase("env", "构建运行环境")
                    .with_rate(fresh.rate)
                    .with_detail(fresh.detail())
                    .log(fresh.log.clone()));
                let stage = fresh.stage.clone();
                view = fresh;
                if stage == "done" || stage == "failed" { break }
                if stage == "idle" && started.elapsed() > Duration::from_secs(180) { break }
                if last_change.elapsed() > Duration::from_secs(360) {
                    emit(app, Progress::phase("env", "构建运行环境")
                        .log("运行环境构建长时间没有进展，先让它继续跑"));
                    break;
                }
            }
            Err(error) => {
                failures += 1;
                trace(format!("env view failed ({failures}): {error}"));
                if failures >= 15 {
                    emit(app, Progress::phase("env", "构建运行环境")
                        .log(format!("读不到运行环境进度：{error}")));
                    break;
                }
            }
        }
    }
    view
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
    let composition = composition()?;
    // 版本号取自清单里的镜像 release（v0.1.20 → 0.1.20），组件也全部按这个 tag 取。
    let release = composition.release_tag();
    let version = release.trim_start_matches('v').to_string();
    if version.is_empty() {
        return Err("组合配置里没有 release 字段，拒绝安装".into());
    }
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
    emit(app, Progress::phase("stage", "写入启动配置").log(format!("读取 {} 的 {BOOT_ASSET}", release)));
    let boot = variant_boot(&release)?;
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
    // 更新即启动；首次安装(start=false)交给调用方在卡片页之后单独预热。
    // 运行环境不再由启动器自己扫目录/读账本统计:实例起来后订阅 web 的 /api/environments。
    let mut live: Option<u16> = None;
    let mut env_view = EnvView::default();
    if start {
    emit(app, Progress::phase("start", "启动实例"));
    match tauri::async_runtime::block_on(crate::variant_runtime::open_workbench(None, None)) {
        Ok(port) => {
            live = Some(port);
            emit(app, Progress::phase("check", "实例已就绪").log(format!("四域 active，端口 {port}")));
            let targets = prewarm_domains(app, port);
            emit(app, Progress::phase("env", format!("预热目标 {targets} 个")).log(format!("发现 {targets} 个 <domain>.prewarm")));
            env_view = poll_environment(app, port);
        }
        Err(error) => emit(app, Progress::phase("fail", "实例启动失败".to_string())
            .log(format!("已更新完成，但启动实例失败：{error}"))),
    }
    }
    let env_note = if env_view.supported { env_view.detail() } else { "无域环境".to_string() };
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
    // 预热触发完之后,进度全部由实例上报:启动器不再自己数环境。
    let view = poll_environment(app, port);
    trace("prewarm: poll returned");
    // 轮询退出时要给一个终态事件,否则覆盖层会一直停在最后一帧、数字再也不动。
    let (label, log) = match view.stage.as_str() {
        "done" => (view.detail(), "运行环境已全部就绪".to_string()),
        "failed" => (view.detail(), "有运行环境构建失败，先放它继续跑".to_string()),
        "idle" if view.supported => (view.detail(), String::new()),
        _ if view.supported => {
            trace(format!("prewarm: stage={} detail={}", view.stage, view.detail()));
            (view.detail(),
             format!("构建由 kernel 继续，可以关掉窗口{}", match &view.current {
                 Some(name) => format!("（当前 {name}）"),
                 None => String::new(),
             }))
        }
        _ => ("预热结束".to_string(), String::new()),
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

    /// 联网回归：确认真实发布的清单能被匿名解析，且每个组件都真的存在（HEAD 200）。
    /// 默认忽略，手动跑：`cargo test --release -- --ignored live_manifest`
    #[test]
    #[ignore]
    fn live_manifest_resolves_to_public_assets() {
        let composition = composition().expect("清单应当能匿名取到");
        let release = composition.release_tag();
        assert!(release.starts_with('v'), "清单缺少 release 字段，实际 {release:?}");
        for (key, component) in &composition.artifacts {
            assert_eq!(component.repo, COMPOSITION_REPO, "{key} 没有镜像到 Free 仓库");
        }
        let plan = plan(&composition);
        assert_eq!(plan.items.len(), 7, "五个域 + 内核 + SDK");
        for item in &plan.items {
            assert!(item.url.starts_with("https://github.com/"), "{}", item.url);
            let size = http_size(&item.url);
            assert!(size.unwrap_or(0) > 0, "{} 取不到大小：{}", item.asset, item.url);
            // 旁挂也必须可匿名取到（内核除外）。
            if !item.kernel {
                let sidecar = http_text(&format!("{}.sha256", item.url))
                    .unwrap_or_else(|e| panic!("{} 缺旁挂：{e}", item.asset));
                assert_eq!(sidecar.split_whitespace().next().unwrap_or("").len(), 64);
            }
        }
        let boot = variant_boot(&release).expect("kernel_boot.free.json 应当能取到");
        assert!(boot.contains("\"domains\""));

        // 真拉一个小组件（2 MB 的 video 插件），验证「地址 → 字节 → sha256」这条链路
        // 和发布出来的旁挂对得上。fetch_once 需要 AppHandle，这里用同一套地址与校验逻辑。
        let video = plan.items.iter().find(|item| item.asset.starts_with("AtelierVideoPlugin"))
            .expect("清单里应当有 video 插件");
        let dir = std::env::temp_dir().join("atelier-live-download");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join(&video.asset);
        let mut response = client().unwrap().get(&video.url).send().unwrap();
        assert!(response.status().is_success());
        let mut file = fs::File::create(&target).unwrap();
        std::io::copy(&mut response, &mut file).unwrap();
        drop(file);
        let expected = http_text(&format!("{}.sha256", video.url)).unwrap();
        let expected = expected.split_whitespace().next().unwrap().to_ascii_lowercase();
        let actual = sha256_of(&target).unwrap();
        assert_eq!(expected, actual, "下载内容与旁挂不一致");
        let _ = fs::remove_dir_all(&dir);
    }

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
            url: String::new(),
            size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            asset,
            placement,
            kernel: false,
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