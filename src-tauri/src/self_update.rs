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
///
/// 三个坑都在这里改掉了：
/// * 旧脚本把进程号存进 `$pid` —— 那是 PowerShell 的**只读自动变量**，赋值直接抛错；
/// * 脚本里有中文却没有 BOM，PowerShell 5.1 按 ANSI 解析，整个文件语法错误、一启动就死，
///   而且死得悄无声息（和 `install.ps1` 当年同一个坑）。现在日志全用 ASCII + 写 BOM；
/// * 每一步都写 `apply-update.log`，出错不再是无痕失败。
pub fn write_apply_script(new_exe: &Path, current: &Path) -> Result<PathBuf, String> {
    let dir = updates_dir()?;
    let script = dir.join("apply-update.ps1");
    let log = dir.join("apply-update.log");
    let old = dir.join("atelier.old.exe");
    let body = format!(
        "$ErrorActionPreference='Stop'\r\n\
         $log = '{log}'\r\n\
         function Say($m) {{ \"$(Get-Date -Format 'HH:mm:ss') $m\" | Out-File -FilePath $log -Append -Encoding ascii }}\r\n\
         $launcherPid = {pid}\r\n\
         Say \"waiting for launcher pid $launcherPid to exit\"\r\n\
         $deadline = (Get-Date).AddSeconds(60)\r\n\
         while (Get-Process -Id $launcherPid -ErrorAction SilentlyContinue) {{\r\n\
           if ((Get-Date) -gt $deadline) {{ Say 'launcher still alive after 60s, giving up'; exit 1 }}\r\n\
           Start-Sleep -Milliseconds 300\r\n\
         }}\r\n\
         $tries = 0\r\n\
         while ($true) {{\r\n\
           try {{ Move-Item -LiteralPath '{current}' -Destination '{old}' -Force; break }}\r\n\
           catch {{ $tries++; if ($tries -ge 10) {{ Say \"cannot move old exe: $_\"; exit 1 }}; Start-Sleep -Milliseconds 500 }}\r\n\
         }}\r\n\
         Say 'old exe moved aside'\r\n\
         try {{\r\n\
           Move-Item -LiteralPath '{new_exe}' -Destination '{current}' -Force\r\n\
           Say 'new exe in place, starting it'\r\n\
           Start-Process -FilePath '{current}'\r\n\
         }} catch {{\r\n\
           Say \"swap failed: $_\"\r\n\
           Move-Item -LiteralPath '{old}' -Destination '{current}' -Force\r\n\
           Start-Process -FilePath '{current}'\r\n\
           exit 1\r\n\
         }}\r\n",
        log = log.display(),
        pid = std::process::id(),
        current = current.display(),
        old = old.display(),
        new_exe = new_exe.display(),
    );
    // UTF-8 **with BOM**：PowerShell 5.1 靠 BOM 判断编码，没有就按 ANSI 读。
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(body.as_bytes());
    fs::write(&script, bytes).map_err(|e| format!("无法写入更新脚本：{e}"))?;
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

/// 下载 → 校验 → 写换文件脚本 → 拉起脚本 → **退出自己**。
///
/// 以前少最后一步：界面提示「即将退出」，可进程一直活着，脚本在 `while (Get-Process)` 里
/// 等一个永远不退的 pid，于是什么都不会发生 —— 这就是「更新启动器不起作用」。
#[tauri::command]
pub async fn apply_self_update(app: tauri::AppHandle, tag: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let new_exe = download(&tag)?;
        let current = std::env::current_exe().map_err(|e| e.to_string())?;
        let script = write_apply_script(&new_exe, &current)?;
        spawn_apply(&script)?;
        // 给脚本一点启动时间再退出，避免它一醒来就发现进程已消失（也能走到等待循环）。
        std::thread::sleep(std::time::Duration::from_millis(400));
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 换文件脚本必须是 **UTF-8 with BOM 且正文纯 ASCII**：
    /// PowerShell 5.1 没有 BOM 就按 ANSI 读，中文注释会把整个脚本变成语法错误；
    /// 它是在启动器退出之后跑的，失败时用户只看到"更新没反应"。
    #[test]
    fn apply_script_is_bom_ascii_and_avoids_reserved_pid() {
        let dir = std::env::temp_dir().join("atelier-selfupdate-script-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let new_exe = dir.join("atelier-new.exe");
        let current = dir.join("atelier.exe");
        fs::write(&new_exe, b"x").unwrap();
        fs::write(&current, b"x").unwrap();

        let script = write_apply_script(&new_exe, &current).expect("应当能写出脚本");
        let bytes = fs::read(&script).unwrap();
        assert_eq!(&bytes[0..3], &[0xEF, 0xBB, 0xBF], "脚本必须带 UTF-8 BOM");
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert!(text.is_ascii(), "脚本正文必须是纯 ASCII，否则 PS 5.1 会按 ANSI 读错");
        assert!(!text.contains("$pid "), "不能用 PowerShell 的只读自动变量 $pid");
        assert!(text.contains("$launcherPid"));
        assert!(text.contains(&current.display().to_string()));
        assert!(text.contains(&new_exe.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }
}
