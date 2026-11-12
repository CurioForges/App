//! 出网客户端的唯一入口。
//!
//! 为什么需要这一层：reqwest 默认只读 `HTTPS_PROXY` 这类环境变量，**不读 Windows 的
//! 系统代理**（WinINET 设置）。国内机器普遍靠系统代理/VPN 出网，于是启动器报
//! `error sending request`，而同一个 URL 在浏览器里打得开——就是这里丢的。
//!
//! 取值顺序：环境变量 → 注册表 `Internet Settings`（ProxyEnable / ProxyServer）。

use std::time::Duration;

const UA: &str = "Atelier-Launcher";

/// 出网客户端。`timeout_secs` 是整体超时（下载要长、查版本要短）。
pub(crate) fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(timeout_secs));
    if let Some(proxy) = system_proxy() {
        match reqwest::Proxy::all(&proxy) {
            Ok(p) => builder = builder.proxy(p),
            Err(error) => crate::installer::trace(format!("proxy ignored ({proxy}): {error}")),
        }
    }
    builder.build().map_err(|e| format!("无法创建网络连接：{e}"))
}

/// 当前生效的代理，`None` 表示直连。仅用于诊断展示（不含口令）。
pub(crate) fn describe_proxy() -> String {
    match system_proxy() {
        Some(proxy) => proxy,
        None => "直连".to_string(),
    }
}

fn system_proxy() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(with_scheme(value));
            }
        }
    }
    #[cfg(windows)]
    {
        return windows_proxy();
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 读 `HKCU\...\Internet Settings`。`reg.exe` 是系统自带，不引入新依赖。
#[cfg(windows)]
fn windows_proxy() -> Option<String> {
    use std::process::Command;
    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_registry(&String::from_utf8_lossy(&output.stdout))
}

/// 解析 `reg query` 的输出。中文 Windows 的 `REG_SZ` 值可能带本地编码，
/// 但代理地址本身是 ASCII，所以按行切就够了。
#[cfg(windows)]
fn parse_registry(text: &str) -> Option<String> {
    let mut enabled = false;
    let mut server: Option<String> = None;
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        match parts[0] {
            "ProxyEnable" => enabled = parts[2].trim_start_matches("0x").trim() != "0",
            "ProxyServer" => {
                let value = parts[2..].join("");
                if !value.is_empty() {
                    server = Some(value);
                }
            }
            _ => {}
        }
    }
    if !enabled {
        return None;
    }
    let raw = server?;
    // `http=host:port;https=host:port` 或直接 `host:port`
    let mut found: Option<String> = None;
    for entry in raw.split(';') {
        let (scheme, address) = match entry.split_once('=') {
            Some((scheme, address)) => (scheme.trim(), address.trim()),
            None => ("", entry.trim()),
        };
        if address.is_empty() {
            continue;
        }
        if scheme.eq_ignore_ascii_case("https") {
            return Some(with_scheme(address));
        }
        if found.is_none() {
            found = Some(with_scheme(address));
        }
    }
    found
}

fn with_scheme(value: &str) -> String {
    if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn registry_proxy_is_read_only_when_enabled() {
        let enabled = "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    127.0.0.1:7890\r\n";
        assert_eq!(parse_registry(enabled).as_deref(), Some("http://127.0.0.1:7890"));

        let disabled = "    ProxyEnable    REG_DWORD    0x0\r\n    ProxyServer    REG_SZ    127.0.0.1:7890\r\n";
        assert_eq!(parse_registry(disabled), None);

        let per_scheme = "    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    http=10.0.0.2:8080;https=10.0.0.2:8443\r\n";
        assert_eq!(parse_registry(per_scheme).as_deref(), Some("http://10.0.0.2:8443"));

        let bare_https = "    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    https://proxy.corp:443\r\n";
        assert_eq!(parse_registry(bare_https).as_deref(), Some("https://proxy.corp:443"));
    }

    #[test]
    fn scheme_is_added_once() {
        assert_eq!(with_scheme("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(with_scheme("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
    }
}
