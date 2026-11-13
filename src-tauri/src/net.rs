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

/// 读 `HKCU\...\Internet Settings`。走原生注册表 API：`reg.exe` 是控制台程序，
/// 每次建客户端都拉一个进程，更新一次发行版能闪十几次黑框。
#[cfg(windows)]
fn windows_proxy() -> Option<String> {
    if crate::win::internet_setting_dword("ProxyEnable")? == 0 {
        return None;
    }
    let raw = crate::win::internet_setting_string("ProxyServer")?;
    pick_server(&raw)
}

/// 从 `ProxyServer` 的值里挑一个：`http=host:port;https=host:port` 或直接 `host:port`。
#[cfg(windows)]
fn pick_server(raw: &str) -> Option<String> {
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
    fn proxy_value_prefers_https_and_tolerates_bare_hosts() {
        // `ProxyServer` 的真实取值：单地址、按协议列表、带协议前缀。
        assert_eq!(pick_server("127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(pick_server("http=10.0.0.2:8080;https=10.0.0.2:8443").as_deref(),
                   Some("http://10.0.0.2:8443"));
        assert_eq!(pick_server("https://proxy.corp:443").as_deref(),
                   Some("https://proxy.corp:443"));
        assert_eq!(pick_server("socks=10.0.0.2:1080"), Some("http://10.0.0.2:1080".to_string()));
        assert_eq!(pick_server(""), None);
    }

    #[cfg(windows)]
    #[test]
    fn disabled_proxy_means_direct_connection() {
        // 机器上没开代理时 registry 里就是 0（或根本没有这个值）。
        let enabled = crate::win::internet_setting_dword("ProxyEnable");
        if enabled == Some(0) {
            assert_eq!(system_proxy(), None);
        }
    }

    #[test]
    fn scheme_is_added_once() {
        assert_eq!(with_scheme("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(with_scheme("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
    }
}
