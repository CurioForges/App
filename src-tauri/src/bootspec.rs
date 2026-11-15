//! 从"实际装进去的组件"推导内核启动配置（`kernel_boot.json`）。
//!
//! 以前启动器是把发行版自己的 `kernel_boot.free.json` 原样下载落盘。这样做能保证和
//! 发行侧一致，但配置与磁盘上的东西可以脱节：某个域下线后，只要那份 JSON 没跟着改，
//! 启动器就会拉起一个已经不存在的域（`media` 就是这么留下来的）。
//!
//! 推导把顺序反过来：先看清树里到底有什么，再写配置。于是配置只可能描述存在的文件。
//!
//! 这里**没有白名单**。清单里列了什么就装什么；装进去的组件只要能定位到进程入口，
//! 就成为一个域；定位不到的（插件、数据、静态资源）留在磁盘上，只是不被内核托管。
//! 缺组件不算错，多组件也不算错——内核本来就愿意接管任何符合规范的域。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::contract::WORKBENCH_DOMAIN;

/// Web 域 HTTP 端口的占位值。
///
/// `variant_runtime::assign_ports` 每次启动都会把它替换成真正的空闲端口；这里只需要是
/// 一个合法的 socket 地址。之所以要声明，是因为启动器靠它找到工作台。
const WEB_LISTEN_PLACEHOLDER: &str = "127.0.0.1:8655";

/// 一个已安装的组件：清单键 + 资产文件名 + 包内 `artifact.json` 声明的域名。
pub struct ComponentRef<'a> {
    /// 组合清单里的键，例如 `web`。包内没有 artifact.json 时用它当域 id。
    pub key: &'a str,
    /// 资产文件名，例如 `AtelierWeb-v0.0.1-k0.0.1-s0.0.1-<sha>.zip`。
    pub asset: &'a str,
    /// 包内 `artifact.json` 的 `domain` 字段——打包侧声明的权威域名；没有就是 `None`。
    pub domain: Option<&'a str>,
}

/// 推导结果。
#[derive(Debug)]
pub struct Derived {
    /// 可直接写入 `kernel_boot.json` 的 JSON 文本。
    pub json: String,
    /// 成为域的组件键（按写入顺序）。
    pub domains: Vec<String>,
    /// 没有进程入口、按文件组件安装的键。
    pub skipped: Vec<String>,
}

/// 从资产名取出打包配方里的 `asset_prefix`。
///
/// `AtelierWeb-v0.0.1-k0.0.1-s0.0.1-<sha>.zip` → `AtelierWeb`。
/// 版本段按 `-v<数字>` 识别；没有版本段时退化为第一个 `-` 之前的部分。
pub fn asset_prefix(asset: &str) -> String {
    let name = asset.rsplit(['/', '\\']).next().unwrap_or(asset);
    let name = name.strip_suffix(".zip").unwrap_or(name);
    let bytes = name.as_bytes();
    for index in 0..bytes.len().saturating_sub(2) {
        if bytes[index] == b'-' && bytes[index + 1] == b'v' && bytes[index + 2].is_ascii_digit() {
            return name[..index].to_string();
        }
    }
    name.split('-').next().unwrap_or(name).to_string()
}

/// `AtelierWeb` → `atelier-web`；`AtelierPipelines` → `atelier-pipelines`。
///
/// 打包产物用的就是这套 kebab 命名（`atelier-web.exe`、`atelier-pipelines-host/`），
/// 所以这里必须和它严格一致。
pub fn kebab(prefix: &str) -> String {
    let chars: Vec<char> = prefix.chars().collect();
    let mut out = String::with_capacity(prefix.len() + 4);
    for (index, &c) in chars.iter().enumerate() {
        if c == '_' || c == ' ' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            continue;
        }
        if c.is_ascii_uppercase() {
            let previous = index.checked_sub(1).map(|i| chars[i]);
            let next = chars.get(index + 1).copied();
            let boundary = match previous {
                Some(prev) if prev.is_ascii_lowercase() || prev.is_ascii_digit() => true,
                // 连续大写段（`WebUI`）只在最后一个大写前断，`AtelierWebUI` → `atelier-web-ui`
                Some(prev) if prev.is_ascii_uppercase() => {
                    next.map(|n| n.is_ascii_lowercase()).unwrap_or(false)
                }
                _ => false,
            };
            if boundary && !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// 组件键 → 合法域 id。内核 M1 要求非空 ASCII 字母数字下划线，这里把其他字符折成 `_`。
pub fn domain_id(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// 在已装好的树里找某个组件的进程入口，返回相对安装根的路径（统一用 `/`）。
///
/// 依次尝试：
/// 1. 根下 `<kebab>.exe`（`atelier-web.exe`）
/// 2. 同名目录 `<kebab>/<kebab>.exe`
/// 3. 以 `<kebab>` 开头的目录，且目录里有一个与目录同名的 exe
///    （`atelier-pipelines-host/atelier-pipelines-host.exe`）
///
/// 第 3 步命中多个时明确报错，而不是随便挑一个。
pub fn find_entry(tree: &Path, kebab: &str) -> Result<Option<String>, String> {
    if kebab.is_empty() {
        return Ok(None);
    }
    let root_exe = format!("{kebab}.exe");
    if tree.join(&root_exe).is_file() {
        return Ok(Some(root_exe));
    }
    let nested = format!("{kebab}/{kebab}.exe");
    if tree.join(&nested).is_file() {
        return Ok(Some(nested));
    }
    let Ok(entries) = fs::read_dir(tree) else {
        return Ok(None);
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().starts_with(&kebab.to_ascii_lowercase()) {
            continue;
        }
        let candidate = format!("{name}/{name}.exe");
        if tree.join(&candidate).is_file() {
            matches.push(candidate);
        }
    }
    matches.sort();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(format!(
            "{kebab} 的进程入口不唯一：{}；拒绝猜测",
            matches.join("、")
        )),
    }
}

/// 推导整份启动配置。
///
/// `components` 只包含摊进树的组件（域包、插件包），不含内核运行时与 SDK。
/// 一个入口都没找到时返回错误：空的 `domains` 对用户没有意义，不如明确失败。
pub fn derive(tree: &Path, components: &[ComponentRef<'_>]) -> Result<Derived, String> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for component in components {
        // 打包侧的 `artifact.json` 是权威（域名叫什么由产物自己说），
        // 清单键只是它在没带 artifact.json 时的兜底。
        let declared = component.domain.map(domain_id).filter(|id| !id.is_empty());
        let id = declared.unwrap_or_else(|| domain_id(component.key));
        if id.is_empty() {
            return Err(format!("组件 '{}' 推导不出合法的域 id", component.key));
        }
        let entry = find_entry(tree, &kebab(&asset_prefix(component.asset)))?;
        match entry {
            Some(command) => {
                if !seen.insert(id.clone()) {
                    return Err(format!("两个组件推导出同一个域 id：{id}"));
                }
                rows.push((id, command));
            }
            None => skipped.push(component.key.to_string()),
        }
    }

    if rows.is_empty() {
        return Err("已安装组件里没有找到任何进程入口，拒绝写入空的启动配置".to_string());
    }

    // 稳定顺序：Web 在前（启动器和人都先找它），其余按 id 排。
    rows.sort_by(|left, right| {
        let rank = |id: &str| if id == WORKBENCH_DOMAIN { 0 } else { 1 };
        rank(&left.0).cmp(&rank(&right.0)).then_with(|| left.0.cmp(&right.0))
    });

    let domains: Vec<Value> = rows
        .iter()
        .map(|(id, command)| {
            let mut spec = json!({ "domain_id": id, "command": command, "args": [] });
            if id == WORKBENCH_DOMAIN {
                spec["ports"] = json!([{ "name": "http", "listen": WEB_LISTEN_PLACEHOLDER }]);
            }
            spec
        })
        .collect();

    let boot = json!({
        "db_path": "data/atelier_kernel.db",
        "web_ui_enabled": false,
        "domains": domains,
    });

    Ok(Derived {
        json: serde_json::to_string_pretty(&boot).map_err(|e| e.to_string())?,
        domains: rows.into_iter().map(|(id, _)| id).collect(),
        skipped,
    })
}

/// 读一份启动配置里声明的域 id。启动器用它判断"要等哪些域起来"。
pub fn domain_ids(config: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(config) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    value
        .get("domains")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("domain_id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atelier-bootspec-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, b"stub").expect("stub file");
    }

    #[test]
    fn asset_prefix_ignores_the_version_segments() {
        assert_eq!(asset_prefix("AtelierWeb-v0.0.1-k0.0.1-s0.0.1-cccfcee.zip"), "AtelierWeb");
        assert_eq!(asset_prefix("AtelierEngines-v0.0.6-k0.0.1-s0.0.1-7663fe5.zip"), "AtelierEngines");
        assert_eq!(asset_prefix("AtelierVideoPlugin-v0.0.2-k0.0.1-s0.0.1-5685ac0.zip"), "AtelierVideoPlugin");
        // 没有版本段时退化为第一段
        assert_eq!(asset_prefix("AtelierWeb.zip"), "AtelierWeb");
    }

    #[test]
    fn kebab_matches_the_packaging_names() {
        assert_eq!(kebab("AtelierWeb"), "atelier-web");
        assert_eq!(kebab("AtelierPipelines"), "atelier-pipelines");
        assert_eq!(kebab("AtelierScheduler"), "atelier-scheduler");
        assert_eq!(kebab("AtelierEngines"), "atelier-engines");
        assert_eq!(kebab("AtelierWebUI"), "atelier-web-ui");
    }

    #[test]
    fn domain_id_stays_inside_the_kernel_identifier_rules() {
        assert_eq!(domain_id("web"), "web");
        assert_eq!(domain_id("Cargo-Index"), "cargo_index");
        assert_eq!(domain_id("a.b"), "a_b");
        assert_eq!(domain_id("--"), "");
    }

    #[test]
    fn entry_is_found_in_the_root_nested_and_host_directory_layouts() {
        let tree = scratch("layouts");
        touch(&tree.join("atelier-web.exe"));
        touch(&tree.join("atelier-scheduler").join("atelier-scheduler.exe"));
        touch(&tree.join("atelier-pipelines-host").join("atelier-pipelines-host.exe"));

        assert_eq!(find_entry(&tree, "atelier-web").unwrap().as_deref(), Some("atelier-web.exe"));
        assert_eq!(
            find_entry(&tree, "atelier-scheduler").unwrap().as_deref(),
            Some("atelier-scheduler/atelier-scheduler.exe")
        );
        // asset_prefix 只给到 atelier-pipelines，真实目录多一个 -host 后缀
        assert_eq!(
            find_entry(&tree, "atelier-pipelines").unwrap().as_deref(),
            Some("atelier-pipelines-host/atelier-pipelines-host.exe")
        );
        assert_eq!(find_entry(&tree, "atelier-missing").unwrap(), None);
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn ambiguous_entries_are_refused_instead_of_guessed() {
        let tree = scratch("ambiguous");
        touch(&tree.join("atelier-domain-a").join("atelier-domain-a.exe"));
        touch(&tree.join("atelier-domain-b").join("atelier-domain-b.exe"));
        let error = find_entry(&tree, "atelier-domain").unwrap_err();
        assert!(error.contains("不唯一"), "unexpected error: {error}");
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn derivation_puts_web_first_and_only_web_gets_a_port() {
        let tree = scratch("derive");
        touch(&tree.join("atelier-web.exe"));
        touch(&tree.join("atelier-scheduler").join("atelier-scheduler.exe"));
        touch(&tree.join("pipeline-plugins").join("video_translate").join("pipeline.py"));

        let components = vec![
            ComponentRef { key: "scheduler", asset: "AtelierScheduler-v0.0.4-k0.0.1-s0.0.1-4c9273.zip", domain: None },
            ComponentRef { key: "web", asset: "AtelierWeb-v0.0.1-k0.0.1-s0.0.1-cccfce.zip", domain: None },
            ComponentRef { key: "video", asset: "AtelierVideoPlugin-v0.0.2-k0.0.1-s0.0.1-5685ac.zip", domain: None },
        ];
        let derived = derive(&tree, &components).expect("derive");

        assert_eq!(derived.domains, vec!["web".to_string(), "scheduler".to_string()]);
        assert_eq!(derived.skipped, vec!["video".to_string()], "插件没有进程入口，只装不托管");

        let boot: Value = serde_json::from_str(&derived.json).expect("json");
        let domains = boot["domains"].as_array().expect("domains");
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0]["domain_id"], "web");
        assert_eq!(domains[0]["command"], "atelier-web.exe");
        assert_eq!(domains[0]["ports"][0]["name"], "http");
        assert!(domains[1].get("ports").is_none(), "非 web 域不该凭空多出端口");
        assert_eq!(boot["web_ui_enabled"], false);
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn extra_components_are_installed_and_not_refused() {
        // 清单里出现内核不认识的东西：有入口就当域，没入口就只落盘。
        let tree = scratch("extra");
        touch(&tree.join("atelier-web.exe"));
        touch(&tree.join("atelier-media").join("atelier-media.exe"));
        let components = vec![
            ComponentRef { key: "web", asset: "AtelierWeb-v0.0.1-k0.0.1-s0.0.1-aaaa.zip", domain: None },
            ComponentRef { key: "media", asset: "AtelierMedia-v0.0.1-k0.0.1-s0.0.1-bbbb.zip", domain: None },
            ComponentRef { key: "extras", asset: "AtelierExtras-v0.0.1-k0.0.1-s0.0.1-cccc.zip", domain: None },
        ];
        let derived = derive(&tree, &components).expect("derive");
        assert_eq!(derived.domains, vec!["web".to_string(), "media".to_string()]);
        assert_eq!(derived.skipped, vec!["extras".to_string()]);
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn no_entry_at_all_is_an_error_not_an_empty_config() {
        let tree = scratch("empty");
        touch(&tree.join("pipeline-plugins").join("video_translate").join("pipeline.py"));
        let components = vec![
            ComponentRef { key: "video", asset: "AtelierVideoPlugin-v0.0.2-k0.0.1-s0.0.1-dddd.zip", domain: None },
        ];
        let error = derive(&tree, &components).unwrap_err();
        assert!(error.contains("没有找到任何进程入口"), "unexpected error: {error}");
        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn domain_ids_reads_the_declared_list_back() {
        let tree = scratch("ids");
        fs::write(
            tree.join("kernel_boot.json"),
            r#"{"domains":[{"domain_id":"web"},{"domain_id":"engines"}]}"#,
        )
        .expect("write");
        assert_eq!(domain_ids(&tree.join("kernel_boot.json")), vec!["web".to_string(), "engines".to_string()]);
        assert!(domain_ids(&tree.join("missing.json")).is_empty());
        let _ = fs::remove_dir_all(&tree);
    }
}
