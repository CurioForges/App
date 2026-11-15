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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
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
    pub(crate) fn phase(phase: &'static str, label: impl Into<String>) -> Self {
        Self { job: "install", phase, label: label.into(), percent: None, done: None,
               total: None, rate: None, detail: None, log: None }
    }

    pub(crate) fn with_bytes(mut self, done: u64, total: Option<u64>, rate: Option<u64>) -> Self {
        self.done = Some(done);
        self.total = total;
        self.rate = rate;
        self.percent = total
            .filter(|total| *total > 0)
            .map(|total| (done as f32 / total as f32) * 100.0);
        self
    }

    /// 换一批事件（迁移等）：界面据此决定用弹窗里的进度条还是全局覆盖层。
    pub(crate) fn in_job(mut self, job: &'static str) -> Self {
        self.job = job;
        self
    }

    fn with_percent(mut self, value: f32) -> Self {
        self.percent = Some(value);
        self
    }

    pub(crate) fn with_rate(mut self, rate: u64) -> Self {
        self.rate = Some(rate);
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn log(mut self, line: impl Into<String>) -> Self {
        self.log = Some(line.into());
        self
    }
}

pub(crate) fn emit_progress(app: &AppHandle, progress: Progress) {
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
    emit_progress(app, progress);
}

/// 各源通用的清单文件形状（`artifacts.free.json` / `artifacts.paid.json`）。
#[derive(Debug, Deserialize)]
struct ManifestFile {
    train: String,
    kernel_version: String,
    sdk_version: String,
    /// 镜像 release 的 tag（形如 `v0.0.1`）。缺省时退回任一组件自己的 release。
    #[serde(default)]
    release: String,
    artifacts: BTreeMap<String, RawComponent>,
}

#[derive(Debug, Deserialize)]
struct RawComponent {
    /// 清单里写的镜像仓库名（`Atelier/dist-free`），只用于诊断。
    #[serde(default)]
    repo: String,
    #[serde(default)]
    release: String,
    asset: String,
    #[serde(default)]
    source_sha: String,
    /// `sha256:...`。GitLab 的 generic package 没有 `.sha256` 旁挂，靠它校验。
    #[serde(default)]
    digest: Option<String>,
    /// 显式下载地址（内网镜像/离线清单用）；正常清单不写。
    #[serde(default)]
    url: Option<String>,
}

/// 拼接完成后的组合：地址已定好，可以直接下载。
#[derive(Debug)]
struct Composition {
    train: String,
    kernel_version: String,
    sdk_version: String,
    release: String,
    artifacts: BTreeMap<String, Component>,
    /// 本次实际拼进来的来源（给界面显示"这次装了哪些源"）。
    notes: Vec<String>,
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

#[derive(Debug, Clone)]
struct Component {
    /// 清单里写的镜像仓库名，只用于诊断。
    #[allow(dead_code)]
    repo: String,
    release: String,
    asset: String,
    /// 产出这次产物的源码提交，只用于诊断。
    #[allow(dead_code)]
    source_sha: String,
    /// 首选下载地址（CNB）。
    url: String,
    /// 其余镜像地址，按优先级（GitLab → Gitea）。
    fallbacks: Vec<String>,
    /// 清单里的 `sha256:...`；有它就不依赖 `.sha256` 旁挂。
    digest: Option<String>,
    /// 下载这个资产要不要带 `PRIVATE-TOKEN`。
    authorized: bool,
}

/// 把各种写法的摘要归一成 64 位小写 hex。
///
/// 清单里是 `sha256:<hex>`，GitLab 包接口给的是裸 hex，内核/SDK 走的是前者——
/// 比较之前必须统一，否则 `sha256:abc` 和 `abc` 永远不相等。
fn normalize_digest(value: &str) -> Option<String> {
    let hex = value.trim().strip_prefix("sha256:").unwrap_or(value.trim()).trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

impl Component {
    fn expected_digest(&self) -> Option<String> {
        self.digest.as_deref().and_then(normalize_digest)
    }
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
    /// 组合清单里的组件键（`web` / `scheduler` / …）。推导启动配置时它就是域 id 的来源。
    key: String,
    name: String,
    /// 完整下载地址（首选：CNB）。
    url: String,
    /// 同一份字节的其余镜像地址，按优先级排列（GitLab → Gitea）。首选失败时逐个退。
    fallbacks: Vec<String>,
    asset: String,
    size: u64,
    placement: Placement,
    /// 内核 release 没有 sha256 旁挂，这一项跳过逐字节校验（其余一律拒绝安装）。
    kernel: bool,
    /// 清单里带的 sha256；有就按它校验，不必依赖旁挂。
    digest: Option<String>,
    /// 下载时要带 `PRIVATE-TOKEN`（来自授权仓）。
    authorized: bool,
}

/// 一个产物来源（GitLab 项目）。
///
/// 免费域公开可读；授权域私有，没有令牌就**整个跳过**——"有多少装多少"，不是错误。
/// 镜像拓扑（用户定的优先级）：**CNB 优先 → GitLab 其次 → Gitea 兜底**。
///
/// 三个位置放的是同一批字节（资产名逐字节相同），差别只在从哪条腿搬：
/// * CNB：国内加速，公网匿名可读，走它的 CDN 最省时延，也不花自建仓的流量钱；
/// * GitLab：主投递，公网 CDN，最稳；
/// * Gitea：自建、家宽出网，只在前面全失效时才用——它是唯一会花流量费的。
///
/// 顺序写在 `candidates()` 里，下载侧逐个试，任一成功即停。
const CNB_ORIGIN: &str = "https://cnb.cool";
const GITEA_ORIGIN: &str = "https://atelier.hurcaguari.top";

/// CNB 的 release 资产直链（匿名可用，支持 Range）。
fn cnb_release_url(org_repo: &str, tag: &str, asset: &str) -> String {
    format!("{CNB_ORIGIN}/{org_repo}/-/releases/download/{tag}/{asset}")
}

/// Gitea（自建）的 release 资产直链，兜底用。
fn gitea_release_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("{GITEA_ORIGIN}/{repo}/releases/download/{tag}/{asset}")
}

/// CNB 仓库根下的原始文件（清单用）。`/-/git/raw/` 匿名可读，`api.cnb.cool` 那条要令牌。
fn cnb_raw_url(org_repo: &str, file: &str) -> String {
    format!("{CNB_ORIGIN}/{org_repo}/-/git/raw/master/{file}")
}

/// Gitea 仓库根下的原始文件。
fn gitea_raw_url(repo: &str, file: &str) -> String {
    format!("{GITEA_ORIGIN}/{repo}/raw/branch/master/{file}")
}

/// 一个产物来源。
///
/// 免费域公开可读；授权域私有，没有令牌就**整个跳过**——"有多少装多少"，不是错误。
/// 授权仓只在 GitLab 上可读（CNB/Gitea 那份是私有的，启动器没有对应令牌），
/// 所以授权源的两个镜像字段留空，`candidates()` 自然只给出 GitLab 一条腿。
struct Source {
    label: &'static str,
    /// GitLab 项目路径，已做 URL 编码。
    project: &'static str,
    /// 项目根目录下的清单文件名。
    manifest: &'static str,
    /// 需要授权才能读（付费仓）。
    authorized: bool,
    /// 同一份清单/资产在 CNB 上的仓库（`org/repo`）；空 = 这条腿不用。
    cnb: &'static str,
    /// 同一份清单/资产在自建 Gitea 上的仓库（`owner/repo`）；空 = 这条腿不用。
    gitea: &'static str,
}

const SOURCES: [Source; 2] = [
    Source {
        label: "免费域",
        project: "AtelierAll%2FDomainFree",
        manifest: "artifacts.free.json",
        authorized: false,
        cnb: "Atelier-2026/DomainFree",
        gitea: "Atelier/dist-free",
    },
    Source {
        label: "授权域",
        project: "AtelierAll%2FDomainPlus",
        manifest: "artifacts.paid.json",
        authorized: true,
        cnb: "",
        gitea: "",
    },
];

/// 内核运行时与 SDK 的来源。
const KERNEL_PROJECT: &str = "AtelierAll%2FKernel";
/// 内核产物在 CNB / 自建 Gitea 上的仓库（镜像对齐后这两条腿也全了）。
const KERNEL_CNB: &str = "Atelier-2026/Kernel";
const KERNEL_GITEA: &str = "Atelier/core-kernel";

/// 授权仓的令牌。没有它就只装免费域，不报错。
fn gitlab_token() -> Option<String> {
    std::env::var("ATELIER_GITLAB_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// 这里不再有任何 `Command::new`：代理、哈希、剩余空间、解压全部改成进程内完成。
// 每拉起一个控制台程序（reg / certutil / tar / powershell）就多一次闪黑框的机会。

/// 出网客户端走统一入口：真机常见"浏览器能开、启动器打不开"，是系统代理没被使用。
fn client() -> Result<reqwest::blocking::Client, String> {
    crate::net::client(1800)
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
fn http_size(url: &str, authorized: bool) -> Option<u64> {
    let mut request = client().ok()?.head(url);
    if let Some(token) = token_for(authorized) {
        request = request.header("PRIVATE-TOKEN", token);
    }
    let response = request.send().ok()?;
    if !response.status().is_success() { return None }
    response.headers().get(reqwest::header::CONTENT_LENGTH)?
        .to_str().ok()?.parse().ok()
}

/// GitLab API 前缀。
fn api(project: &str, suffix: &str) -> String {
    format!("https://gitlab.com/api/v4/projects/{project}/{suffix}")
}

/// 授权源才带令牌；免费源公开可读，带了反而多一层失败面。
fn token_for(authorized: bool) -> Option<String> {
    if authorized { gitlab_token() } else { None }
}

fn http_text_auth(url: &str, token: Option<&str>) -> Result<String, String> {
    let mut request = client()?.get(url);
    if let Some(token) = token {
        request = request.header("PRIVATE-TOKEN", token);
    }
    let response = request.send().map_err(|e| format!("无法连接下载服务：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "请求返回 {}（{}）",
            status.as_u16(),
            url.rsplit('/').next().unwrap_or(url)
        ));
    }
    response.text().map_err(|e| format!("读取响应失败：{e}"))
}

/// 每个源一份清单缓存：断网时至少还能显示上次的组合。
fn manifest_cache_path(source: &Source) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let dir = base.join("Atelier");
    let _ = fs::create_dir_all(&dir);
    Some(dir.join(format!("manifest-{}.json", source.project.replace("%2F", "-"))))
}

/// 组件用哪个 release：清单里写了就用写的，否则按 train 推。
fn component_tag(component: &RawComponent, train: &str) -> String {
    let trimmed = component.release.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    format!("v{train}")
}

/// 资产名里的域版本：`AtelierEngines-v0.0.6-k0.0.1-…` → `0.0.6`。
fn asset_domain_version(asset: &str) -> Option<String> {
    let bytes = asset.as_bytes();
    for index in 0..bytes.len().saturating_sub(3) {
        if bytes[index] == b'-' && bytes[index + 1] == b'v' && bytes[index + 2].is_ascii_digit() {
            let rest = &asset[index + 2..];
            let end = rest.find('-')?;
            let version = &rest[..end];
            let well_formed = !version.is_empty()
                && version.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
            if well_formed {
                return Some(version.to_string());
            }
        }
    }
    None
}

/// GitLab generic package 的下载地址（release 链接缺失时的兜底）。
///
/// 发布脚本的约定：`packages/generic/atelier-<域>/<域版本>/<资产>`。
fn generic_asset_url(project: &str, key: &str, asset: &str) -> Option<String> {
    let version = asset_domain_version(asset)?;
    Some(format!("https://gitlab.com/api/v4/projects/{project}/packages/generic/atelier-{key}/{version}/{asset}"))
}

fn generic_kernel_url(project: &str, version: &str, asset: &str) -> String {
    format!("https://gitlab.com/api/v4/projects/{project}/packages/generic/atelier-kernel/{version}/{asset}")
}

/// 把 release 里记录的界面式包地址归一化成 API 地址。
///
/// 发布脚本写进 release 的是 `https://gitlab.com/<项目>/-/packages/generic/...`（界面形式）。
/// 这种地址：HEAD 常常不给 Content-Length，而且**私有项目无法用 `PRIVATE-TOKEN` 取**。
/// API 形式 `https://gitlab.com/api/v4/projects/<编码项目>/packages/generic/...` 两者都成立。
fn normalize_package_url(url: &str) -> String {
    const PREFIX: &str = "https://gitlab.com/";
    const MARKER: &str = "/-/packages/generic/";
    let Some(rest) = url.strip_prefix(PREFIX) else { return url.to_string() };
    let Some((project, tail)) = rest.split_once(MARKER) else { return url.to_string() };
    let encoded = project.replace('/', "%2F");
    format!("{PREFIX}api/v4/projects/{encoded}/packages/generic/{tail}")
}

/// GitLab 包文件自带的 sha256。
///
/// GitLab 不像 Gitea 那样发 `<asset>.sha256` 旁挂，但每个包文件都记着 `file_sha256`
/// （页面「SHA-256」那一行就是它）。内核运行时和 SDK 没有清单字段，靠这个才校验得了。
fn package_digests(project: &str, token: Option<&str>) -> BTreeMap<String, String> {
    let mut digests = BTreeMap::new();
    let Ok(raw) = http_text_auth(&api(project, "packages?per_page=100"), token) else {
        return digests;
    };
    let Ok(packages) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
        return digests;
    };
    for package in packages {
        if package.get("package_type").and_then(|kind| kind.as_str()) != Some("generic") {
            continue;
        }
        let Some(id) = package.get("id").and_then(|id| id.as_u64()) else { continue };
        let Ok(raw) = http_text_auth(&api(project, &format!("packages/{id}/package_files")), token)
        else {
            continue;
        };
        let Ok(files) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else { continue };
        for file in files {
            let (Some(name), Some(sha)) = (
                file.get("file_name").and_then(|name| name.as_str()),
                file.get("file_sha256").and_then(|sha| sha.as_str()),
            ) else {
                continue;
            };
            let sha = sha.trim().to_ascii_lowercase();
            if sha.len() == 64 {
                digests.insert(name.to_string(), sha);
            }
        }
    }
    digests
}

/// 把 GitLab 的摘要包成清单同款写法。
fn digest_or_none(digests: &BTreeMap<String, String>, asset: &str) -> Option<String> {
    digests.get(asset).map(|sha| format!("sha256:{sha}"))
}

/// release 里的资产名 → 直链。
fn release_links(project: &str, tag: &str, token: Option<&str>) -> Result<BTreeMap<String, String>, String> {
    let raw = http_text_auth(&api(project, &format!("releases/{tag}")), token)?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("release 解析失败：{e}"))?;
    let mut map = BTreeMap::new();
    let Some(links) = value.get("assets").and_then(|assets| assets.get("links")).and_then(|links| links.as_array()) else {
        return Ok(map);
    };
    for link in links {
        let Some(name) = link.get("name").and_then(|name| name.as_str()) else { continue };
        let url = link
            .get("direct_asset_url")
            .and_then(|url| url.as_str())
            .or_else(|| link.get("url").and_then(|url| url.as_str()));
        if let Some(url) = url {
            map.insert(name.to_string(), normalize_package_url(url));
        }
    }
    Ok(map)
}

/// 清单的候选地址，顺序即优先级：CNB → GitLab → Gitea。
///
/// 授权源只给 GitLab 一条腿：CNB/Gitea 那份是私有的，启动器手上没有对应令牌。
fn manifest_candidates(source: &Source) -> Vec<String> {
    let mut urls = Vec::new();
    if !source.cnb.is_empty() {
        urls.push(cnb_raw_url(source.cnb, source.manifest));
    }
    urls.push(api(source.project, &format!("repository/files/{}/raw?ref=master", source.manifest)));
    if !source.gitea.is_empty() {
        urls.push(gitea_raw_url(source.gitea, source.manifest));
    }
    urls
}

/// 读一个源：清单 → 每个资产的下载地址。授权源没有令牌时直接判定不可用。
fn load_source(source: &Source) -> Result<Composition, String> {
    let token = token_for(source.authorized);
    if source.authorized && token.is_none() {
        return Err("没有可用的授权令牌".to_string());
    }
    // 清单按 CNB → GitLab → Gitea 的顺序取；第一个成功的作为本次清单。
    let manifest_urls = manifest_candidates(source);
    let mut raw = String::new();
    let mut manifest_error = String::new();
    for url in &manifest_urls {
        match http_text_auth(url, token.as_deref()) {
            Ok(text) => {
                raw = text;
                break;
            }
            Err(error) => manifest_error = error,
        }
    }
    let raw = if raw.is_empty() {
        if source.authorized {
            return Err(manifest_error);
        }
        match manifest_cache_path(source).and_then(|path| fs::read_to_string(path).ok()) {
            Some(cached) => cached,
            None => {
                return Err(format!(
                    "无法读取产物清单（CNB / GitLab / Gitea 三条腿都失败）：{manifest_error}\n\
                     如果这台机器访问外网需要代理，请先在系统代理里设置好（或设置环境变量 HTTPS_PROXY），再重试。\n\
                     启动器当前出网：{}",
                    crate::net::describe_proxy()
                ))
            }
        }
    } else {
        if let Some(path) = manifest_cache_path(source) {
            let _ = fs::write(path, &raw);
        }
        raw
    };

    let manifest: ManifestFile =
        serde_json::from_str(&raw).map_err(|e| format!("组合清单解析失败：{e}"))?;
    // 同一次安装的资产可能分布在不同 release 上；每个 tag 只查一次链接表。
    let mut tags: BTreeSet<String> = BTreeSet::new();
    for component in manifest.artifacts.values() {
        tags.insert(component_tag(component, &manifest.train));
    }
    let mut links: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for tag in tags {
        // release 查不到不算致命：下面还有 generic 包路径可以兜底。
        let map = release_links(source.project, &tag, token.as_deref()).unwrap_or_default();
        links.insert(tag, map);
    }
    // 清单没写摘要时用 GitLab 记的包摘要兜底（内核/SDK 没有清单，只能靠它）。
    let digests = package_digests(source.project, token.as_deref());
    let mut notes: Vec<String> = Vec::new();

    let mut artifacts = BTreeMap::new();
    for (key, raw_component) in manifest.artifacts {
        let tag = component_tag(&raw_component, &manifest.train);
        // 地址按镜像拓扑排：CNB 优先 → GitLab → Gitea。清单里写了显式 url 的照旧直用。
        let gitlab_url = links
            .get(&tag)
            .and_then(|map| map.get(&raw_component.asset).cloned())
            .or_else(|| generic_asset_url(source.project, &key, &raw_component.asset));
        let (resolved, fallbacks) = match (&raw_component.url, source.cnb.is_empty()) {
            (Some(explicit), _) => (explicit.clone(), Vec::new()),
            (None, false) => {
                let mut rest = Vec::new();
                if let Some(url) = gitlab_url.clone() {
                    rest.push(url);
                }
                if !source.gitea.is_empty() {
                    rest.push(gitea_release_url(source.gitea, &tag, &raw_component.asset));
                }
                (cnb_release_url(source.cnb, &tag, &raw_component.asset), rest)
            }
            (None, true) => (
                gitlab_url.ok_or_else(|| format!("{} 在{}里找不到下载地址", raw_component.asset, source.label))?,
                Vec::new(),
            ),
        };
        // 摘要以**GitLab 实际存的**为准：它描述的就是我们真正要下的那串字节。
        // 清单里的 digest 是发布时写的，重出包后可能没跟着更新——那种情况下按清单校验只会卡住安装。
        let from_gitlab = digest_or_none(&digests, &raw_component.asset);
        let from_manifest = raw_component.digest.clone();
        let digest = match (&from_gitlab, &from_manifest) {
            (Some(gitlab), Some(manifest)) => {
                let manifest_hex = manifest
                    .strip_prefix("sha256:")
                    .unwrap_or(manifest.as_str())
                    .trim()
                    .to_ascii_lowercase();
                let gitlab_hex = gitlab.strip_prefix("sha256:").unwrap_or(gitlab).to_ascii_lowercase();
                if manifest_hex != gitlab_hex {
                    notes.push(format!(
                        "{}：清单里的摘要和 GitLab 实际文件不一致，按 GitLab 的校验（清单可能没随重出包更新）",
                        raw_component.asset
                    ));
                }
                Some(gitlab.clone())
            }
            (Some(gitlab), None) => Some(gitlab.clone()),
            (None, manifest) => manifest.clone(),
        };
        artifacts.insert(
            key,
            Component {
                repo: raw_component.repo,
                release: tag,
                asset: raw_component.asset,
                source_sha: raw_component.source_sha,
                url: resolved,
                fallbacks,
                digest,
                authorized: source.authorized,
            },
        );
    }

    Ok(Composition {
        train: manifest.train,
        kernel_version: manifest.kernel_version,
        sdk_version: manifest.sdk_version,
        release: manifest.release,
        artifacts,
        notes: Vec::new(),
    })
}

/// `ATELIER_MANIFEST` 指定的清单：不再拼接多个源，地址按显式 `url` 或旧口径推导。
fn override_composition(raw: &str) -> Result<Composition, String> {
    let manifest: ManifestFile =
        serde_json::from_str(raw).map_err(|e| format!("组合配置解析失败：{e}"))?;
    let mut artifacts = BTreeMap::new();
    for (key, raw_component) in manifest.artifacts {
        let tag = component_tag(&raw_component, &manifest.train);
        let url = raw_component.url.clone().unwrap_or_else(|| {
            format!(
                "https://github.com/{}/releases/download/{tag}/{}",
                raw_component.repo, raw_component.asset
            )
        });
        artifacts.insert(
            key,
            Component {
                repo: raw_component.repo,
                release: tag,
                asset: raw_component.asset,
                source_sha: raw_component.source_sha,
                url,
                fallbacks: Vec::new(),
                digest: raw_component.digest,
                authorized: false,
            },
        );
    }
    Ok(Composition {
        train: manifest.train,
        kernel_version: manifest.kernel_version,
        sdk_version: manifest.sdk_version,
        release: manifest.release,
        artifacts,
        notes: vec!["使用 ATELIER_MANIFEST 指定的组合清单".to_string()],
    })
}

/// 拼接各源：后加载的源覆盖同名键（付费域可以替换免费域的实现）。
/// `skipped` 是已经判定为不可用的源，原样带进说明里。
fn merge_parts(
    parts: Vec<(&'static str, Composition)>,
    skipped: Vec<String>,
) -> Result<Composition, String> {
    let mut notes = skipped;
    let mut merged: BTreeMap<String, Component> = BTreeMap::new();
    let (mut train, mut kernel_version, mut sdk_version, mut release) =
        (String::new(), String::new(), String::new(), String::new());

    for (label, part) in parts {
        notes.extend(part.notes.iter().cloned());
        if train.is_empty() {
            train = part.train.clone();
            kernel_version = part.kernel_version.clone();
            sdk_version = part.sdk_version.clone();
            release = part.release.clone();
        } else if part.train != train {
            notes.push(format!(
                "{label}的 train 是 {}，与 {train} 不一致；按 {train} 安装",
                part.train
            ));
        }
        let mut count = 0usize;
        for (key, component) in part.artifacts {
            if merged.insert(key.clone(), component).is_some() {
                notes.push(format!("{key} 被{label}的同名产物覆盖"));
            }
            count += 1;
        }
        notes.push(format!("{label}：{count} 个组件"));
    }

    if merged.is_empty() {
        return Err("没有任何可用的产物来源，无法安装".to_string());
    }
    if kernel_version.trim().is_empty() {
        return Err("清单里没有 kernel_version，无法定位内核运行时".to_string());
    }
    Ok(Composition { train, kernel_version, sdk_version, release, artifacts: merged, notes })
}

/// 按可用性拼接各源的产物：免费域必须可用；授权域拿不到就跳过。
fn composition() -> Result<Composition, String> {
    // 1) 显式覆盖：`ATELIER_MANIFEST` 可以是本地文件路径，也可以是自定义 URL。
    if let Ok(raw_override) = std::env::var("ATELIER_MANIFEST") {
        let value = raw_override.trim();
        if !value.is_empty() {
            let raw = if value.starts_with("http://") || value.starts_with("https://") {
                http_text(value)?
            } else {
                fs::read_to_string(value).map_err(|e| format!("读取本地清单失败：{e}"))?
            };
            return override_composition(&raw);
        }
    }

    let mut parts: Vec<(&'static str, Composition)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for source in SOURCES {
        match load_source(&source) {
            Ok(part) => parts.push((source.label, part)),
            Err(error) => {
                if source.authorized {
                    // 授权域不可用不是安装失败：有多少装多少。
                    skipped.push(format!("{}不可用（{error}），本次只装免费域", source.label));
                } else {
                    return Err(error);
                }
            }
        }
    }

    merge_parts(parts, skipped)
}

fn plan(composition: &Composition) -> Result<Plan, String> {
    let mut items = Vec::new();
    // 清单里列了什么就装什么。这里不设白名单：少几个组件不算错，多出来的也不拒绝——
    // 能不能被内核接管，交给安装后的推导去判断（见 bootspec）。
    for (key, component) in &composition.artifacts {
        let name = crate::catalog::domain(key)
            .map(|meta| meta.label.to_string())
            .unwrap_or_else(|| key.clone());
        items.push(Item {
            key: key.clone(),
            name,
            size: http_size(&component.url, component.authorized).unwrap_or(0),
            url: component.url.clone(),
            fallbacks: component.fallbacks.clone(),
            asset: component.asset.clone(),
            placement: Placement::Tree,
            kernel: false,
            digest: component.expected_digest(),
            authorized: component.authorized,
        });
    }
    let (kernel_item, sdk_item) = kernel_items(composition);
    items.push(kernel_item);
    items.push(sdk_item);
    Ok(Plan { train: composition.train.clone(), items })
}

/// 内核运行时与 SDK：同一个 Kernel 项目的 release，公开可读。
fn kernel_items(composition: &Composition) -> (Item, Item) {
    let kernel_version = composition.kernel_version.trim().trim_start_matches('v').to_string();
    let sdk_version = composition.sdk_version.trim().trim_start_matches('v').to_string();
    let tag = format!("v{kernel_version}");
    let runtime = format!("Atelier-Kernel-runtime-windows-x86_64-v{kernel_version}.zip");
    let sdk = format!("atelier_sdk-{sdk_version}-py3-none-any.whl");
    // release 链接优先；查不到就按 generic 包路径拼（发布约定，见 sync.yml）。
    let links = release_links(KERNEL_PROJECT, &tag, None).unwrap_or_default();
    // 三条腿都摆上：CNB 优先 → GitLab 包 → Gitea release（镜像对齐后它们都有同一份资产）。
    // 以前只拼 GitLab 一条，GitLab 一抖就装不上内核。
    let gitlab_runtime = links
        .get(&runtime)
        .cloned()
        .unwrap_or_else(|| generic_kernel_url(KERNEL_PROJECT, &kernel_version, &runtime));
    let gitlab_sdk = links
        .get(&sdk)
        .cloned()
        .unwrap_or_else(|| generic_kernel_url(KERNEL_PROJECT, &sdk_version, &sdk));
    let runtime_url = cnb_release_url(KERNEL_CNB, &tag, &runtime);
    let runtime_fallbacks = vec![gitlab_runtime, gitea_release_url(KERNEL_GITEA, &tag, &runtime)];
    let sdk_url = cnb_release_url(KERNEL_CNB, &tag, &sdk);
    let sdk_fallbacks = vec![gitlab_sdk, gitea_release_url(KERNEL_GITEA, &tag, &sdk)];
    // 内核/SDK 没有清单字段，摘要只能问 GitLab 包仓库要。
    let digests = package_digests(KERNEL_PROJECT, None);
    let runtime_digest = digest_or_none(&digests, &runtime);
    let sdk_digest = digest_or_none(&digests, &sdk);
    (
        Item {
            key: "kernel".to_string(),
            name: "Kernel 运行时".to_string(),
            size: http_size(&runtime_url, false).unwrap_or(0),
            url: runtime_url,
            fallbacks: runtime_fallbacks,
            asset: runtime,
            placement: Placement::Kernel,
            kernel: true,
            digest: runtime_digest,
            authorized: false,
        },
        Item {
            key: "sdk".to_string(),
            name: "Python SDK".to_string(),
            size: http_size(&sdk_url, false).unwrap_or(0),
            url: sdk_url,
            fallbacks: sdk_fallbacks,
            asset: sdk,
            placement: Placement::Sdk,
            kernel: true,
            digest: sdk_digest,
            authorized: false,
        },
    )
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
    // 以前调 `certutil -hashfile`：控制台程序，每校验一个组件闪一次窗口。走 CNG 原生接口。
    #[cfg(windows)]
    {
        return crate::win::sha256_file(path);
    }
    #[cfg(not(windows))]
    {
        let output = hidden("sha256sum")
            .arg(path)
            .output()
            .map_err(|e| format!("无法计算 sha256：{e}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.split_whitespace().next()
            .map(|value| value.to_ascii_lowercase())
            .filter(|value| value.len() == 64)
            .ok_or_else(|| format!("sha256 输出异常：{}", path.display()))
    }
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
    // 镜像拓扑：CNB 优先 → GitLab → Gitea。逐个试，任一成功即停。
    // 只有 GitLab 那条腿需要令牌；把 `PRIVATE-TOKEN` 发给 CNB/Gitea 是无意义的凭据外泄。
    let mut urls: Vec<&str> = vec![item.url.as_str()];
    urls.extend(item.fallbacks.iter().map(|url| url.as_str()));
    let mut last = String::new();
    for (seen, url) in urls.iter().enumerate() {
        let host = url.split('/').nth(2).unwrap_or("");
        let needs_token = item.authorized && host.ends_with("gitlab.com");
        match fetch_from(app, item, dir, index, count, base, total_all, url, needs_token) {
            Ok(()) => {
                if seen > 0 {
                    emit_progress(app, Progress::phase("download", format!("{} 已切到备用镜像", item.name))
                        .log(format!("{} 从备用镜像取到（{}）", item.asset, host)));
                }
                return Ok(());
            }
            Err(error) => {
                last = format!("{host}: {error}");
                if seen + 1 < urls.len() {
                    emit_progress(app, Progress::phase("download", format!("{} 换下一条镜像", item.name))
                        .log(format!("{} 从 {host} 取失败（{error}），换下一条镜像", item.asset)));
                }
            }
        }
    }
    Err(last)
}

#[allow(clippy::too_many_arguments)]
fn fetch_from(
    app: &AppHandle, item: &Item, dir: &Path, index: usize, count: usize, base: u64, total_all: u64,
    url: &str, with_token: bool,
) -> Result<(), String> {
    let target = dir.join(&item.asset);
    // 上一次失败留下的半截文件会让第一帧读到满尺寸 → 进度先跳高再回退。每次重试都清掉。
    let _ = fs::remove_file(&target);
    let mut request = client()?.get(url);
    if with_token {
        if let Some(token) = token_for(item.authorized) {
            request = request.header("PRIVATE-TOKEN", token);
        }
    }
    let mut response = request.send()
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
                emit_progress(app, progress);
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
        emit_progress(app, Progress::phase("download", format!("{} 命中缓存", item.name))
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
            emit_progress(app, Progress::phase("download", format!("{} 重试中（{}）", item.name, attempt))
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

/// Compare the download against its checksum.
///
/// 优先用清单里带的 `sha256:...`（GitLab 的 generic package 没有 `.sha256` 旁挂）；
/// 清单没写才去找旁挂。两者都没有时：域包拒绝安装，内核运行时放行（发布侧还没有旁挂）。
fn verify(app: &AppHandle, item: &Item, file: &Path) -> Result<(), String> {
    if let Some(raw) = item.digest.clone() {
        let expected = normalize_digest(&raw)
            .ok_or_else(|| format!("{} 的摘要格式无效：{raw}", item.asset))?;
        let actual = sha256_of(file)?;
        if expected != actual {
            return Err(format!(
                "{} 校验失败（清单摘要）：{} ≠ {}",
                item.asset,
                &actual[..12.min(actual.len())],
                &expected[..12.min(expected.len())]
            ));
        }
        return Ok(());
    }
    // 旁挂就在同一个 release 里，地址是资产地址加 `.sha256`；同样重试三次，
    // 并区分「发布方没发」和「网络没拿到」——以前两者都报成前者，误导排查。
    let mut failure = String::new();
    let mut expected = None;
    for attempt in 0..3 {
        if attempt > 0 { thread::sleep(Duration::from_secs(1 << attempt)) }
        // 旁挂也按镜像顺序找：CNB 优先 → GitLab → Gitea。
        let mut urls: Vec<String> = vec![item.url.clone()];
        urls.extend(item.fallbacks.iter().cloned());
        let mut hit = None;
        for url in &urls {
            match http_text_auth(&format!("{url}.sha256"), token_for(item.authorized).as_deref()) {
                Ok(text) => {
                    let digest = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                    if digest.len() == 64 { hit = Some(digest); }
                    break;
                }
                Err(error) => failure = reason_of(&error),
            }
        }
        if hit.is_some() { expected = hit; break; }
    }
    let Some(expected) = expected else {
        // Domains publish `<asset>.sha256`; the kernel release does not (yet). Refuse to skip
        // verification for a domain, but do not brick the update on the kernel runtime — the
        // gap is tracked and will be closed by publishing sidecars there.
        // 清单没写摘要、GitLab 也没记（老包）：内核运行时放行并说明，域包仍拒绝安装。
        if item.kernel {
            emit_progress(app, Progress::phase("verify", format!("跳过校验 {}", item.name))
                .log(format!("{} 拿不到任何校验值（清单 digest / GitLab 包摘要 / .sha256 旁挂都没有），本次不做逐字节校验", item.asset)));
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

/// Unpack one zip into `scratch`.
///
/// 用 `zip` 直接读，不再调系统 `tar`：`tar.exe` 是控制台程序，一次更新拉起五个，
/// 总有环境会闪一下黑框。发行包里的 zip 全是 Deflate、没有 ZIP64，这个实现够用。
fn unzip(file: &Path, scratch: &Path) -> Result<(), String> {
    let handle = fs::File::open(file).map_err(|e| format!("无法打开 {}：{e}", file.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(handle))
        .map_err(|e| format!("解压 {} 失败：{e}", file.display()))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)
            .map_err(|e| format!("解压 {} 失败：{e}", file.display()))?;
        // 条目名来自外部压缩包：只接受相对路径，挡住 `..` 与绝对路径。
        let Some(relative) = entry.enclosed_name() else {
            let name = entry.name().map(|value| value.to_string())
                .unwrap_or_else(|_| "<无法解码的名字>".to_string());
            return Err(format!("压缩包里有越界路径：{name}"));
        };
        let target = scratch.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| format!("解压 {} 失败：{e}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("解压 {} 失败：{e}", parent.display()))?;
        }
        let mut out = fs::File::create(&target)
            .map_err(|e| format!("解压 {} 失败：{e}", target.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("解压 {} 失败：{e}", target.display()))?;
    }
    Ok(())
}

/// 包内 `artifact.json` 声明的域名。打包侧用这个文件说明"我是谁"，比按文件名猜可靠。
fn artifact_domain(scratch: &Path) -> Option<String> {
    let raw = fs::read_to_string(scratch.join("artifact.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("domain")
        .and_then(|domain| domain.as_str())
        .map(|domain| domain.trim().to_string())
        .filter(|domain| !domain.is_empty())
}

/// 一次解包的结果：字节速率 + 包内声明的域名。
struct ExtractOutcome {
    rate: u64,
    domain: Option<String>,
}

/// Unpack one component and move its payload into the assembled tree.
///
/// The tree built here is exactly what the shipped distribution zip contains, because
/// that is what `start.ps1`, the kernel configuration and the domain hosts all assume:
/// `atelier-kernel.exe` at the root, `kernel_boot.json`, `engine-root/`, `dist/`, …
fn extract(item: &Item, file: &Path, staging: &Path, tree: &Path) -> Result<ExtractOutcome, String> {
    let started = Instant::now();
    if matches!(item.placement, Placement::Sdk) {
        // A wheel is installed as a file, never expanded.
        let sdk = tree.join("sdk");
        fs::create_dir_all(&sdk).map_err(|e| e.to_string())?;
        let target = sdk.join(&item.asset);
        fs::copy(file, &target).map_err(|e| format!("无法放置 {}：{e}", target.display()))?;
        return Ok(ExtractOutcome { rate: item.size / started.elapsed().as_secs().max(1), domain: None });
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
        return Ok(ExtractOutcome { rate: item.size / started.elapsed().as_secs().max(1), domain: None });
    }
    let domain = artifact_domain(&scratch);
    merge_tree(&payload, tree)?;
    let _ = fs::remove_dir_all(&scratch);
    Ok(ExtractOutcome { rate: item.size / started.elapsed().as_secs().max(1), domain })
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
        emit_progress(app, Progress::phase("env", format!("预热 {id}"))
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
                    emit_progress(app, Progress::phase("env", format!("{id}.prewarm 被拒绝"))
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
                        emit_progress(app, Progress::phase("env", format!("{id} 预热已受理")).log(note));
                    }
                }
            }
            Err(error) => emit_progress(app, Progress::phase("env", format!("{id}.prewarm 无法连接"))
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
            emit_progress(app, Progress::phase("env", "构建运行环境")
                .log("运行环境构建超过 6 小时，先让它继续跑"));
            break;
        }
        match environment_view(port, &client) {
            Ok(fresh) => {
                failures = 0;
                if !fresh.supported {
                    emit_progress(app, Progress::phase("env", "构建运行环境")
                        .log("当前装配没有 /api/environments，跳过运行环境进度"));
                    break;
                }
                let next = format!("{}|{}|{}|{:.3}", fresh.stage, fresh.ready,
                    fresh.current.clone().unwrap_or_default(), fresh.updated.unwrap_or(0.0));
                if next != fingerprint {
                    fingerprint = next;
                    last_change = Instant::now();
                }
                emit_progress(app, Progress::phase("env", "构建运行环境")
                    .with_rate(fresh.rate)
                    .with_detail(fresh.detail())
                    .log(fresh.log.clone()));
                let stage = fresh.stage.clone();
                view = fresh;
                if stage == "done" || stage == "failed" { break }
                if stage == "idle" && started.elapsed() > Duration::from_secs(180) { break }
                if last_change.elapsed() > Duration::from_secs(360) {
                    emit_progress(app, Progress::phase("env", "构建运行环境")
                        .log("运行环境构建长时间没有进展，先让它继续跑"));
                    break;
                }
            }
            Err(error) => {
                failures += 1;
                trace(format!("env view failed ({failures}): {error}"));
                if failures >= 15 {
                    emit_progress(app, Progress::phase("env", "构建运行环境")
                        .log(format!("读不到运行环境进度：{error}")));
                    break;
                }
            }
        }
    }
    view
}

/// 目标盘剩余空间。原生 API 一次调用：以前起 PowerShell 查一次，窗口会闪。
fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        return crate::win::free_bytes(dir);
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}
/// The whole flow: download → verify → stage → swap, with rollback through a journal.
pub fn run(app: &AppHandle) -> Result<String, String> { run_with(app, true) }

/// start=false 只装不启动、不预热（首次安装用：装完先进卡片页，再单独预热）。
pub fn run_with(app: &AppHandle, start: bool) -> Result<String, String> {
    emit_progress(app, Progress::phase("resolve", "正在读取组合配置"));
    let composition = composition()?;
    // 哪些源拼进来了、哪些跳过了，逐条说清楚（授权域不可用不是错误）。
    for note in &composition.notes {
        emit_progress(app, Progress::phase("resolve", "组合配置").log(note.clone()));
    }
    // 版本号取自清单里的镜像 release（v0.1.20 → 0.1.20），组件也全部按这个 tag 取。
    let release = composition.release_tag();
    let version = release.trim_start_matches('v').to_string();
    if version.is_empty() {
        return Err("组合配置里没有 release 字段，拒绝安装".into());
    }
    let plan = plan(&composition)?;
    let total: u64 = plan.items.iter().map(|i| i.size).sum();
    emit_progress(app, Progress::phase("plan", format!("发行版 {}（{} 个组件）", plan.train, plan.items.len()))
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
            emit_progress(app, Progress::phase("fail", msg.clone()).log(msg.clone()));
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
    // (清单键, 资产名, 包内 artifact.json 声明的域名)
    let mut extracted: Vec<(String, String, Option<String>)> = Vec::new();
    for (index, (item, file)) in fetched.into_iter().enumerate() {
        // 解压没有文件级进度:先发一条不确定态(环转圈),别让数字停在那儿假装卡住。
        emit_progress(app, Progress::phase("stage", format!("正在解压 {}", item.name))
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name)));
        let outcome = extract(item, &file, &staging, &tree)?;
        if matches!(item.placement, Placement::Tree) {
            extracted.push((item.key.clone(), item.asset.clone(), outcome.domain.clone()));
        }
        emit_progress(app, Progress::phase("stage", format!("{} 已就位", item.name))
            .with_bytes(bytes, Some(total), None)
            .with_percent(((index as f32 + 1.0) / (count.max(1) as f32)) * 100.0)
            .with_rate(outcome.rate)
            .with_detail(format!("组件 {}/{} · {}", index + 1, count, item.name))
            .log(format!("{} 已就位", item.asset)));
    }
    // 启动配置由**已经摊进树的组件**推导：装了什么就写什么，配置不可能描述不存在的文件。
    // 这样"域下线了但配置还留着"这类脱节在结构上就不可能出现。
    let installed: Vec<crate::bootspec::ComponentRef> = extracted
        .iter()
        .map(|(key, asset, domain)| crate::bootspec::ComponentRef {
            key: key.as_str(),
            asset: asset.as_str(),
            domain: domain.as_deref(),
        })
        .collect();
    let derived = crate::bootspec::derive(&tree, &installed)?;
    emit_progress(app, Progress::phase("stage", "写入启动配置")
        .log(format!("由 {} 个已装组件推导出 {} 个域：{}",
            installed.len(), derived.domains.len(), derived.domains.join("、"))));
    if !derived.skipped.is_empty() {
        emit_progress(app, Progress::phase("stage", "文件类组件已就位")
            .log(format!("{} 没有进程入口，只安装不托管", derived.skipped.join("、"))));
    }
    fs::write(tree.join("kernel_boot.json"), derived.json)
        .map_err(|e| format!("无法写入 kernel_boot.json：{e}"))?;

    emit_progress(app, Progress::phase("swap", "正在换入发行文件"));
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
    emit_progress(app, Progress::phase("start", "启动实例"));
    match tauri::async_runtime::block_on(crate::variant_runtime::open_workbench(None, None)) {
        Ok(port) => {
            live = Some(port);
            emit_progress(app, Progress::phase("check", "实例已就绪").log(format!("四域 active，端口 {port}")));
            let targets = prewarm_domains(app, port);
            emit_progress(app, Progress::phase("env", format!("预热目标 {targets} 个")).log(format!("发现 {targets} 个 <domain>.prewarm")));
            env_view = poll_environment(app, port);
        }
        Err(error) => emit_progress(app, Progress::phase("fail", "实例启动失败".to_string())
            .log(format!("已更新完成，但启动实例失败：{error}"))),
    }
    }
    let env_note = if env_view.supported { env_view.detail() } else { "无域环境".to_string() };
    emit_progress(app, Progress::phase("done", format!("已更新到 {version}")).with_detail(env_note)
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
            emit_progress(&app, Progress::phase("fail", format!("更新失败：{error}")).log(error.clone()));
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
            emit_progress(&app, Progress::phase("fail", format!("安装失败：{error}")).log(error.clone()));
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 启动实例 + 预热环境的主体。命令和排障钩子共用这一份,避免两条路径行为不一致。
pub(crate) fn prewarm_blocking(app: &AppHandle) -> Result<Option<u16>, String> {
    trace("prewarm: begin");
    emit_progress(app, Progress::phase("start", "启动实例"));
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
    emit_progress(app, Progress::phase("done", label).with_detail(log.clone()).log(log));
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

    /// 镜像拓扑是**用户定的**规则：CNB 优先 → GitLab → Gitea。
    /// 这条顺序一旦被改回"GitLab 优先"，国内用户的安装速度会直接掉一个档，
    /// 而 Gitea 会开始花流量费。所以把它钉成断言，改的人必须先改这里。
    #[test]
    fn manifest_candidates_follow_cnb_gitlab_gitea_order() {
        let free: Vec<String> = manifest_candidates(&SOURCES[0]);
        assert_eq!(free.len(), 3, "免费源三条腿都要在：{free:?}");
        assert!(free[0].starts_with("https://cnb.cool/Atelier-2026/DomainFree/"), "{}", free[0]);
        assert!(free[1].starts_with("https://gitlab.com/api/v4/projects/AtelierAll%2FDomainFree/"), "{}", free[1]);
        assert!(free[2].starts_with("https://atelier.hurcaguari.top/Atelier/dist-free/"), "{}", free[2]);

        // 付费源私有：CNB/Gitea 那两份启动器读不了，只留 GitLab 一条腿。
        let paid: Vec<String> = manifest_candidates(&SOURCES[1]);
        assert_eq!(paid.len(), 1, "授权源只该有 GitLab 一条腿：{paid:?}");
        assert!(paid[0].starts_with("https://gitlab.com/"));
    }

    #[test]
    fn mirror_urls_keep_the_same_asset_name() {
        let asset = "AtelierWeb-v0.0.2-k0.0.1-s0.0.1-abc.zip";
        let cnb = cnb_release_url("Atelier-2026/DomainFree", "v0.0.1", asset);
        let gitea = gitea_release_url("Atelier/dist-free", "v0.0.1", asset);
        assert!(cnb.ends_with(&format!("/{asset}")), "{cnb}");
        assert!(gitea.ends_with(&format!("/{asset}")), "{gitea}");
        assert!(cnb.starts_with("https://cnb.cool/Atelier-2026/DomainFree/-/releases/download/v0.0.1/"));
        assert!(gitea.starts_with("https://atelier.hurcaguari.top/Atelier/dist-free/releases/download/v0.0.1/"));
    }

    /// 内核也要有三级：以前只拼 GitLab，GitLab 一抖内核就装不上。
    #[test]
    fn kernel_items_carry_cnb_then_gitlab_then_gitea() {
        let composition = Composition {
            train: "0.0.1".into(),
            kernel_version: "0.0.1".into(),
            sdk_version: "0.0.1".into(),
            release: String::new(),
            artifacts: BTreeMap::new(),
            notes: Vec::new(),
        };
        let (runtime, sdk) = kernel_items(&composition);
        assert!(runtime.url.starts_with("https://cnb.cool/Atelier-2026/Kernel/"), "{}", runtime.url);
        assert_eq!(runtime.fallbacks.len(), 2, "{:?}", runtime.fallbacks);
        assert!(runtime.fallbacks[0].starts_with("https://gitlab.com/"), "{}", runtime.fallbacks[0]);
        assert!(runtime.fallbacks[1].starts_with("https://atelier.hurcaguari.top/Atelier/core-kernel/"), "{}", runtime.fallbacks[1]);
        assert!(sdk.url.starts_with("https://cnb.cool/Atelier-2026/Kernel/"), "{}", sdk.url);
        assert_eq!(sdk.fallbacks.len(), 2);
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atelier-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 解压走进程内的 `zip`：以前调 `tar.exe`，一次更新五个控制台进程。
    #[test]
    fn unzip_restores_payload_files() {
        let dir = scratch_dir("unzip");
        let archive = dir.join("sample.zip");
        {
            let file = fs::File::create(&archive).expect("create zip");
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.add_directory("payload/", options).expect("dir entry");
            writer.start_file("payload/hello.txt", options).expect("file entry");
            std::io::Write::write_all(&mut writer, b"hello").expect("write");
            writer.finish().expect("finish");
        }
        let out = dir.join("out");
        fs::create_dir_all(&out).expect("out dir");
        unzip(&archive, &out).expect("unzip");
        assert_eq!(fs::read_to_string(out.join("payload/hello.txt")).expect("read"), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 真实发行包回归：`ATELIER_TEST_ZIP=<某个域 zip> cargo test -- --ignored unzip_real_artifact`
    #[test]
    #[ignore]
    fn unzip_real_artifact() {
        let path = std::env::var_os("ATELIER_TEST_ZIP")
            .map(PathBuf::from)
            .expect("ATELIER_TEST_ZIP 指向一个真实域 zip");
        let dir = scratch_dir("unzip-real");
        let out = dir.join("out");
        fs::create_dir_all(&out).expect("out dir");
        let started = Instant::now();
        unzip(&path, &out).expect("unzip");
        let mut files = 0usize;
        let mut bytes = 0u64;
        let mut stack = vec![out.clone()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).expect("walk").flatten() {
                let meta = entry.metadata().expect("meta");
                if meta.is_dir() { stack.push(entry.path()) } else { files += 1; bytes += meta.len() }
            }
        }
        assert!(files > 0, "解压后应该有文件");
        assert!(out.join("artifact.json").is_file() || out.join("payload").is_dir(),
            "域 zip 应该带 artifact.json 或 payload/");
        println!("解压 {files} 个文件 / {:.1} MB，用时 {:.1}s",
            bytes as f64 / 1e6, started.elapsed().as_secs_f64());
        if std::env::var_os("ATELIER_TEST_KEEP").is_none() {
            let _ = fs::remove_dir_all(&dir);
        } else {
            println!("保留解压目录：{}", out.display());
        }
    }

    #[test]
    fn unzip_refuses_paths_that_escape_the_target() {
        let dir = scratch_dir("unzip-slip");
        let archive = dir.join("slip.zip");
        {
            let file = fs::File::create(&archive).expect("create zip");
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            match writer.start_file("../escape.txt", options) {
                Ok(()) => {
                    std::io::Write::write_all(&mut writer, b"nope").expect("write");
                    writer.finish().expect("finish");
                }
                // 写入端自己就拒绝这种名字时，没有可测的样本。
                Err(_) => {
                    let _ = fs::remove_dir_all(&dir);
                    return;
                }
            }
        }
        let out = dir.join("out");
        fs::create_dir_all(&out).expect("out dir");
        assert!(unzip(&archive, &out).is_err());
        assert!(!dir.join("escape.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_forms_are_normalized_before_comparison() {
        let hex = "622e63c7b4ea1be8c5ebb10e6323b41bdb41af149af0c42c0baf1e78ce49f8fd";
        // 清单写法、GitLab 的裸 hex、大写、带空白——都要归一成同一个值
        assert_eq!(normalize_digest(&format!("sha256:{hex}")).as_deref(), Some(hex));
        assert_eq!(normalize_digest(hex).as_deref(), Some(hex));
        assert_eq!(normalize_digest(&format!("  {}  ", hex.to_ascii_uppercase())).as_deref(), Some(hex));
        // 长度/字符不对的一律不算有效摘要
        assert_eq!(normalize_digest("sha256:abc"), None);
        assert_eq!(normalize_digest(&"z".repeat(64)), None);
        assert_eq!(normalize_digest(""), None);
    }

    #[test]
    fn artifact_manifest_names_the_domain() {
        let dir = std::env::temp_dir().join(format!("atelier-artifact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("artifact.json"), r#"{"schema":1,"domain":"engines","version":"0.0.6"}"#).unwrap();
        assert_eq!(artifact_domain(&dir).as_deref(), Some("engines"));

        // 空白域名不算数；坏 JSON 不 panic。
        fs::write(dir.join("artifact.json"), r#"{"domain":"  "}"#).unwrap();
        assert_eq!(artifact_domain(&dir), None);
        fs::write(dir.join("artifact.json"), "{not json").unwrap();
        assert_eq!(artifact_domain(&dir), None);

        fs::remove_file(dir.join("artifact.json")).unwrap();
        assert_eq!(artifact_domain(&dir), None, "没有 artifact.json 时交给清单键兜底");
        let _ = fs::remove_dir_all(&dir);
    }

    fn fake_component(url: &str) -> Component {        Component {
            repo: "Atelier/dist-free".to_string(),
            release: "v0.0.1".to_string(),
            asset: "Asset-v0.0.1-k0.0.1-s0.0.1-abc.zip".to_string(),
            source_sha: "abc".to_string(),
            url: url.to_string(),
            fallbacks: Vec::new(),
            digest: None,
            authorized: false,
        }
    }

    fn fake_composition(entries: &[(&str, &str)]) -> Composition {
        Composition {
            train: "0.0.1".to_string(),
            kernel_version: "0.0.1".to_string(),
            sdk_version: "0.0.1".to_string(),
            release: "v0.0.1".to_string(),
            artifacts: entries
                .iter()
                .map(|(key, url)| (key.to_string(), fake_component(url)))
                .collect(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn package_urls_are_normalized_to_the_api_form() {
        // 发布脚本写进 release 的是界面形式；换成 API 形式才能带令牌、才有 Content-Length。
        assert_eq!(
            normalize_package_url(
                "https://gitlab.com/AtelierAll/DomainFree/-/packages/generic/atelier-web/0.0.1/AtelierWeb.zip"
            ),
            "https://gitlab.com/api/v4/projects/AtelierAll%2FDomainFree/packages/generic/atelier-web/0.0.1/AtelierWeb.zip"
        );
        let already = "https://gitlab.com/api/v4/projects/AtelierAll%2FKernel/packages/generic/atelier-kernel/0.0.1/x.zip";
        assert_eq!(normalize_package_url(already), already);
        assert_eq!(normalize_package_url("https://example.com/x.zip"), "https://example.com/x.zip");
    }

    #[test]
    fn generic_fallback_uses_the_version_inside_the_asset_name() {
        assert_eq!(
            asset_domain_version("AtelierEngines-v0.0.6-k0.0.1-s0.0.1-abc.zip").as_deref(),
            Some("0.0.6")
        );
        assert_eq!(asset_domain_version("AtelierWeb-v0.0.1-k0.0.1-s0.0.1-abc.zip").as_deref(), Some("0.0.1"));
        assert_eq!(asset_domain_version("no-version-here.zip"), None);
        assert_eq!(
            generic_asset_url("AtelierAll%2FDomainFree", "engines", "AtelierEngines-v0.0.6-k0.0.1-s0.0.1-abc.zip").as_deref(),
            Some("https://gitlab.com/api/v4/projects/AtelierAll%2FDomainFree/packages/generic/atelier-engines/0.0.6/AtelierEngines-v0.0.6-k0.0.1-s0.0.1-abc.zip")
        );
    }

    #[test]
    fn merging_keeps_everything_available_and_lets_the_later_source_win() {
        let free = fake_composition(&[("web", "https://free/web"), ("daemons", "https://free/daemons")]);
        let paid = fake_composition(&[("daemons", "https://paid/daemons"), ("publishers", "https://paid/publishers")]);
        let merged = merge_parts(
            vec![("免费域", free), ("授权域", paid)],
            vec!["授权域不可用（没有可用的授权令牌），本次只装免费域".to_string()],
        )
        .expect("两个源应当能拼起来");

        assert_eq!(merged.artifacts.len(), 3, "同名键只留一份");
        assert_eq!(merged.artifacts["web"].url, "https://free/web");
        assert_eq!(merged.artifacts["publishers"].url, "https://paid/publishers");
        assert_eq!(merged.artifacts["daemons"].url, "https://paid/daemons", "后加载的源覆盖同名键");
        assert!(merged.notes.iter().any(|note| note.contains("不可用")), "跳过原因要留痕：{:?}", merged.notes);
        assert!(
            merged.notes.iter().any(|note| note.contains("daemons") && note.contains("覆盖")),
            "覆盖也要留痕：{:?}",
            merged.notes
        );
        assert_eq!(merged.train, "0.0.1");
    }

    #[test]
    fn merging_refuses_an_empty_result_or_a_missing_kernel_version() {
        let empty = fake_composition(&[]);
        assert!(merge_parts(vec![("免费域", empty)], Vec::new())
            .unwrap_err()
            .contains("没有任何可用的产物来源"));

        let mut no_kernel = fake_composition(&[("web", "https://free/web")]);
        no_kernel.kernel_version = String::new();
        assert!(merge_parts(vec![("免费域", no_kernel)], Vec::new())
            .unwrap_err()
            .contains("kernel_version"));
    }

    /// 联网回归：确认真实的 GitLab 清单能匿名解析、地址可下载、摘要对得上。
    #[test]
    #[ignore]
    fn live_manifest_resolves_to_public_assets() {
        let composition = composition().expect("清单应当能匿名取到");
        let release = composition.release_tag();
        assert!(release.starts_with('v'), "清单缺少 release 字段，实际 {release:?}");
        println!("来源：{:?}", composition.notes);
        println!("组件：{:?}", composition.artifacts.keys().collect::<Vec<_>>());
        assert!(!composition.artifacts.is_empty(), "至少应有免费域");
        for (key, component) in &composition.artifacts {
            assert!(
                component.url.starts_with("https://gitlab.com/"),
                "{key} 的地址不在 GitLab 上：{}",
                component.url
            );
            assert!(component.expected_digest().is_some(), "{key} 清单里没有 sha256 摘要");
        }
        let plan = plan(&composition).expect("计划应当能算出来");
        for item in &plan.items {
            let size = http_size(&item.url, item.authorized);
            println!("  {} -> {:?} bytes  {}", item.asset, size, item.url);
            assert!(size.unwrap_or(0) > 0, "{} 取不到大小：{}", item.asset, item.url);
        }
        // 启动配置不再从 release 下载：安装期由已落盘的组件推导（见 bootspec 与其测试）。
        for item in plan.items.iter().filter(|item| matches!(item.placement, Placement::Tree)) {
            assert!(!crate::bootspec::asset_prefix(&item.asset).is_empty(), "{} 推不出资产前缀", item.asset);
        }

        // 真拉一个小组件（约 2 MB 的 video 插件），验证「地址 → 字节 → 清单摘要」这条链路。
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
        let expected = video.digest.clone().expect("清单摘要");
        let actual = sha256_of(&target).unwrap();
        assert_eq!(expected, actual, "下载内容与清单摘要不一致");
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
            key: crate::bootspec::kebab(&crate::bootspec::asset_prefix(&asset)),
            name: "test".to_string(),
            url: String::new(),
            fallbacks: Vec::new(),
            size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            asset,
            placement,
            kernel: false,
            digest: None,
            authorized: false,
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

        // 缓存里可能同时留着同一组件的多个版本（重出包之后就是这样）。生产里每个组件只有一个
        // 资产，这里按前缀取最高版本，避免把两版 web 都解进同一棵树。
        let mut newest: BTreeMap<String, (PathBuf, String)> = BTreeMap::new();
        for entry in fs::read_dir(&cache).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !(name.starts_with("Atelier") || name.ends_with(".whl")) || path.extension().is_none() {
                continue;
            }
            if path.extension().unwrap() != "zip" && !name.ends_with(".whl") { continue }
            let prefix = crate::bootspec::asset_prefix(&name);
            let version = asset_domain_version(&name)
                .map(|v| {
                    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
                    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
                })
                .unwrap_or((0, 0, 0));
            match newest.get(&prefix) {
                Some((_, existing)) => {
                    let existing_version = asset_domain_version(existing)
                        .map(|v| {
                            let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
                            (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
                        })
                        .unwrap_or((0, 0, 0));
                    if version > existing_version {
                        newest.insert(prefix, (path, name));
                    }
                }
                None => { newest.insert(prefix, (path, name)); }
            }
        }

        let mut seen = 0;
        let mut components: Vec<(String, String, Option<String>)> = Vec::new();
        for (prefix, (path, name)) in &newest {
            let item = item_for(path);
            let outcome = extract(&item, path, &staging, &tree).unwrap();
            if matches!(item.placement, Placement::Tree) {
                // 生产里这个键来自 artifacts.json（`web` / `video` …）；测试里用资产前缀近似。
                let key = prefix.trim_start_matches("Atelier").to_string();
                components.push((key, name.clone(), outcome.domain));
            }
            seen += 1;
        }
        assert!(seen >= 3, "缓存里应有多个 artifact，实际 {seen}");

        // 内核:包装层被摊平，二进制在根。
        assert!(tree.join("atelier-kernel.exe").is_file(), "atelier-kernel.exe 应该在根目录");
        assert!(!tree.join("kernel-runtime").exists(), "kernel-runtime/ 包装层不应保留");
        assert!(!tree.join("kernel_boot.template.json").exists(), "内核包自带的模板不属于安装目录");
        assert!(!tree.join("docs").exists(), "内核包自带的文档不属于安装目录");
        // 域本体各就各位。
        assert!(tree.join("atelier-scheduler").is_dir());
        assert!(tree.join("engine-root").join("domains").join("engines").is_dir(), "engine-root 必须装进来");
        assert!(tree.join("pipeline-plugins").join("video_translate").is_dir());
        // Web 宿主 exe 不是每个发布版本都有（0.0.1 的 web 包只有静态资源）——断言跟着产物流。
        let has_web_host = tree.join("atelier-web.exe").is_file();
        // SDK 是文件，不是被解开的包。
        let sdk = fs::read_dir(tree.join("sdk")).unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "whl"))
            .count();
        assert_eq!(sdk, 1, "sdk/ 里应当有一个 wheel");
        assert!(!tree.join("atelier_sdk").exists(), "wheel 不该被解开成 atelier_sdk/");

        // 在**真实产物**上推导启动配置：入口存在、插件不托管、配置里每条命令都真的在盘上。
        let refs: Vec<crate::bootspec::ComponentRef> = components
            .iter()
            .map(|(key, asset, domain)| crate::bootspec::ComponentRef {
                key: key.as_str(),
                asset: asset.as_str(),
                domain: domain.as_deref(),
            })
            .collect();
        let derived = crate::bootspec::derive(&tree, &refs).expect("真实产物应当能推导出启动配置");
        println!("推导出的启动配置：\n{}", derived.json);
        for expected in ["scheduler", "pipelines", "engines"] {
            assert!(derived.domains.iter().any(|id| id == expected), "缺少域 {expected}：{:?}", derived.domains);
        }
        assert_eq!(
            derived.domains.iter().any(|id| id == "web"),
            has_web_host,
            "web 有没有进 domains，必须和树里有没有 atelier-web.exe 一致（当前：{:?}）",
            derived.domains
        );
        assert!(derived.skipped.iter().any(|key| key.to_lowercase().contains("video")),
            "视频插件没有进程入口，应当只装不托管：{:?}", derived.skipped);
        let boot: serde_json::Value = serde_json::from_str(&derived.json).unwrap();
        let domains = boot["domains"].as_array().expect("domains 数组");
        if has_web_host {
            assert_eq!(domains[0]["domain_id"], "web", "Web 必须排在第一个");
            assert_eq!(domains[0]["ports"][0]["name"], "http");
        }
        for domain in domains {
            let command = domain["command"].as_str().expect("command 字符串");
            assert!(tree.join(command).is_file(), "推导出的入口在树里不存在：{command}");
            if domain["domain_id"] != "web" {
                assert!(domain.get("ports").is_none(), "只有 web 该带端口：{domain}");
            }
        }

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