//! 启动器自更新。
//!
//! 与发行包更新是**两条独立的流**:这里换的是启动器自己的 exe(`target/release/atelier.exe`),
//! 不是 `%LOCALAPPDATA%\AtelierFree` 里的发行包。版本源是公开仓 `NomadAnget/Atelier` 的 release,
//! 匿名可拉,不需要 token。
//!
//! 换文件不能在自身进程里做(Windows 会锁住正在运行的 exe),所以写一个 `apply-update.ps1`:
//! 等本进程退出 → 旧 exe 改名 `.old` → 新 exe 改名正式名 → 启动 → 失败还原。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "NomadAnget/Atelier";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfUpdate {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub notes: String,
}

fn hidden(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

/// 启动器自己的更新也全走匿名 HTTPS：用户机器上没有 `gh`，也不该要求他们装。
fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("Atelier-Launcher")
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .map_err(|e| format!("无法创建网络连接：{e}"))
}

fn http_text(url: &str) -> Result<String, String> {
    let response = client()?.get(url).send().map_err(|e| format!("无法连接更新服务：{e}"))?;
    let status = response.status();
    if !response.status().is_success() && status.as_u16() != 404 {
        return Err(format!("更新服务返回 {}", status.as_u16()));
    }
    response.text().map_err(|e| e.to_string())
}

fn download_to(url: &str, target: &Path) -> Result<(), String> {
    let mut response = client()?.get(url).send().map_err(|e| format!("下载失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!("下载失败：HTTP {}", response.status().as_u16()));
    }
    let mut file = fs::File::create(target).map_err(|e| format!("无法写入 {}：{e}", target.display()))?;
    std::io::copy(&mut response, &mut file).map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

/// 当前启动器版本:编译期注入的包版本。
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn updates_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .ok_or_else(|| "没有 LOCALAPPDATA".to_string())?;
    let dir = base.join("Atelier").join("updates");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// `1.2.10` > `1.2.9`:按数字段比较,前缀不同则按字典序兜底。
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x > y }
    }
    false
}

pub fn check() -> Result<SelfUpdate, String> {
    let current = current_version();
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    // 还没发过 release 时 API 返回 404:这不是错误,是"暂时没有新版本"。
    let body = match http_text(&api) {
        Ok(text) => text,
        Err(error) if error.contains("404") => {
            return Ok(SelfUpdate {
                current: current.clone(),
                latest: current,
                available: false,
                notes: "尚未发布任何版本".into(),
            })
        }
        Err(error) => return Err(error),
    };
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("无法解析版本信息：{e}"))?;
    let latest = value.get("tag_name").and_then(|tag| tag.as_str()).unwrap_or("")
        .trim().trim_start_matches('v').to_string();
    if latest.is_empty() {
        return Err("版本信息里没有 tag".into());
    }
    let notes = value.get("body").and_then(|body| body.as_str()).unwrap_or("");
    Ok(SelfUpdate {
        available: is_newer(&latest, &current),
        latest,
        current,
        notes: notes.chars().take(2000).collect(),
    })
}

/// 下载新 exe 到 `updates/`,并与同 release 的 `SHA256SUMS.txt` 比对。
pub fn download(tag: &str) -> Result<PathBuf, String> {
    let dir = updates_dir()?;
    let target = dir.join(format!("atelier-{tag}.new.exe"));
    let _ = fs::remove_file(&target);
    let release = format!("v{tag}");
    download_to(&format!("https://github.com/{REPO}/releases/download/{release}/atelier.exe"), &target)?;

    // 校验:release 里若有 SHA256SUMS.txt 就必须对得上。
    let sums_url = format!("https://github.com/{REPO}/releases/download/{release}/SHA256SUMS.txt");
    if let Ok(sums) = http_text(&sums_url) {
        let expected = sums.lines()
            .find(|line| line.contains("atelier.exe"))
            .and_then(|line| line.split_whitespace().next())
            .map(|hex| hex.to_ascii_lowercase());
        if let Some(expected) = expected {
            let actual = crate::installer::sha256_of(&target)?;
            if expected != actual {
                let _ = fs::remove_file(&target);
                return Err(format!("新版本校验失败：{} ≠ {}", &actual[..12], &expected[..12.min(expected.len())]));
            }
        }
    }
    Ok(target)
}

/// 写换文件脚本并让它在本进程退出后接手。`current` 是正在运行的 exe。
pub fn write_apply_script(new_exe: &Path, current: &Path) -> Result<PathBuf, String> {
    let dir = updates_dir()?;
    let script = dir.join("apply-update.ps1");
    let old = dir.join("atelier.old.exe");
    let body = format!(
        "$ErrorActionPreference='Stop'\r\n\
         $pid = {pid}\r\n\
         while (Get-Process -Id $pid -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 300 }}\r\n\
         Move-Item -LiteralPath '{current}' -Destination '{old}' -Force\r\n\
         try {{\r\n\
           Move-Item -LiteralPath '{new_exe}' -Destination '{current}' -Force\r\n\
           Start-Process -FilePath '{current}'\r\n\
         }} catch {{\r\n\
           Move-Item -LiteralPath '{old}' -Destination '{current}' -Force\r\n\
           throw\r\n\
         }}\r\n",
        pid = std::process::id(),
        current = current.display(),
        old = old.display(),
        new_exe = new_exe.display(),
    );
    fs::write(&script, body).map_err(|e| e.to_string())?;
    Ok(script)
}

/// 拉起换文件脚本(脱离本进程),调用方随后应退出程序。
pub fn spawn_apply(script: &Path) -> Result<(), String> {
    hidden("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &script.to_string_lossy()])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法启动更新脚本：{e}"))
}

#[tauri::command]
pub async fn check_self_update() -> Result<SelfUpdate, String> {
    tauri::async_runtime::spawn_blocking(check).await.map_err(|e| e.to_string())?
}

/// 下载 → 校验 → 写换文件脚本 → 拉起脚本。返回后调用方应退出,让脚本接手换文件。
#[tauri::command]
pub async fn apply_self_update(tag: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let new_exe = download(&tag)?;
        let current = std::env::current_exe().map_err(|e| e.to_string())?;
        let script = write_apply_script(&new_exe, &current)?;
        spawn_apply(&script)
    })
    .await
    .map_err(|e| e.to_string())?
}
