//! What capabilities a distribution can offer, and which are sold separately.
//!
//! This catalog is the launcher's own knowledge, not something read from the
//! installation. That matters: a paid capability must be *visible* to a user who
//! does not own it, otherwise there is no place to buy it. What the installation
//! declares only decides which entries are live.
//!
//! Two levels of sellable unit are modelled here:
//!
//! * a **domain** — a supervised process; installing it needs the binary and a
//!   kernel restart;
//! * a **pipeline** — a plugin package inside the pipelines domain; installing it
//!   needs a directory under `pipeline-plugins/` and a pipelines restart.
//!
//! Nothing in this file grants anything. `licensed` is the single hook where a real
//! entitlement check will land; today it only distinguishes free from paid.

/// The installation this launcher currently manages. One per distribution root.
pub const INSTALL_ID: &str = "free";

#[derive(Debug, Clone, Copy)]
pub struct DomainMeta {
    pub id: &'static str,
    pub label: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Icon name understood by the shared design-system icon set.
    pub icon: &'static str,
    /// Sold separately; absent from a default installation.
    pub paid: bool,
    /// The control plane; the distribution refuses to stop it.
    pub always_on: bool,
}

pub const DOMAINS: [DomainMeta; 6] = [
    DomainMeta {
        id: "web",
        label: "Web",
        title: "界面与工作台",
        description: "总览、任务、日志和设置页面；所有 HTTP 接口都由它提供。",
        icon: "grid",
        paid: false,
        always_on: true,
    },
    DomainMeta {
        id: "scheduler",
        label: "Scheduler",
        title: "任务调度",
        description: "任务的排队、执行与状态机，发布链路也在这里。",
        icon: "clock",
        paid: false,
        always_on: false,
    },
    DomainMeta {
        id: "pipelines",
        label: "Pipelines",
        title: "创作管线",
        description: "把步骤编排成可运行的创作流程；具体管线以插件包形式加载。",
        icon: "layers",
        paid: false,
        always_on: false,
    },
    DomainMeta {
        id: "engines",
        label: "Engines",
        title: "模型能力",
        description: "语音识别与分离、声纹、翻译、图像与 LLM 的本地执行入口。",
        icon: "sliders",
        paid: false,
        always_on: false,
    },
    DomainMeta {
        id: "daemons",
        label: "Daemons",
        title: "频道监控",
        description: "频道接入、热点采集与闲时排产，为创作提供选题来源。",
        icon: "bell",
        paid: true,
        always_on: false,
    },
    DomainMeta {
        id: "publishers",
        label: "Publishers",
        title: "平台发布",
        description: "各平台账号登录态、发布任务与稿件回收。",
        icon: "upload",
        paid: true,
        always_on: false,
    },
];

pub fn domain(id: &str) -> Option<&'static DomainMeta> {
    DOMAINS.iter().find(|entry| entry.id.eq_ignore_ascii_case(id))
}

/// Entitlement hook. Free domains are always usable; paid ones are never licensed
/// until the licensing flow exists, so they render as purchasable but inert.
pub fn licensed(id: &str) -> bool {
    domain(id).map(|entry| !entry.paid).unwrap_or(false)
}

/// Built-in pipelines that are worth naming in the UI even before the plugin is
/// installed. Unknown ids fall back to the runtime declaration.
pub fn pipeline_label(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "video_translate" => Some(("视频翻译", "视频翻译、配音、字幕与封面的完整链路")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_plane_cannot_be_switched_off() {
        let web = domain("web").expect("web is part of the catalog");
        assert!(web.always_on);
        assert!(!web.paid);
    }

    #[test]
    fn the_two_sellable_domains_are_paid_and_unlicensed() {
        for id in ["daemons", "publishers"] {
            let entry = domain(id).expect("sellable domain is in the catalog");
            assert!(entry.paid, "{id} must be marked paid");
            assert!(!licensed(id), "{id} must not look licensed before the flow exists");
        }
        assert!(licensed("engines"));
    }

    #[test]
    fn catalog_ids_are_unique_and_lowercase() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in DOMAINS {
            assert!(seen.insert(entry.id), "duplicate domain id {}", entry.id);
            assert_eq!(entry.id, entry.id.to_ascii_lowercase());
        }
    }
}
