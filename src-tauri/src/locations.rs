//! Filesystem work the launcher has to do for the two user-chosen directories.
//!
//! The data directory has no environment contract in the distribution, so the only
//! way to honour a user choice while keeping `<ATELIER_ROOT>/data` authoritative is
//! a directory junction at that path. Model weights do have a contract
//! (`GCP_MODELS_DIR`), so they are redirected by environment and never linked.

use std::{
    fs,
    os::windows::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// A junction is a reparse point; this is the only reliable way to tell a link
/// apart from a real directory that happens to be empty.
pub fn is_link(path: &Path) -> bool {
    let attributes = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
    attributes != INVALID_FILE_ATTRIBUTES && attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn canonical(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

fn same(a: &Path, b: &Path) -> bool {
    match (canonical(a), canonical(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Case-insensitive comparison that does not need either path to exist yet.
/// `fs::canonicalize` cannot answer for a directory the wizard is about to create.
fn lexical_eq(a: &Path, b: &Path) -> bool {
    let normalise = |path: &Path| {
        std::path::absolute(path)
            .map(|path| path.as_os_str().to_string_lossy().trim_end_matches(['\\', '/']).to_string())
            .unwrap_or_else(|_| path.as_os_str().to_string_lossy().to_string())
    };
    normalise(a).eq_ignore_ascii_case(&normalise(b))
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).map(|mut entries| entries.next().is_none()).unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpace {
    pub free_gb: f64,
    pub total_gb: f64,
}

/// Free space of the volume that will hold `path`. Works for a directory that does
/// not exist yet by walking up to the nearest existing ancestor.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    let mut probe = path;
    let owned = loop {
        if let Some(existing) = canonical(probe) {
            break existing;
        }
        if probe.is_dir() {
            break probe.to_path_buf();
        }
        probe = probe.parent()?;
    };
    let mut free = 0u64;
    let mut total = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide(&owned).as_ptr(), &mut free, &mut total, std::ptr::null_mut())
    };
    (ok != 0).then_some(DiskSpace { free_gb: free as f64 / GIB, total_gb: total as f64 / GIB })
}

/// Why a candidate directory is a bad idea, phrased for a person who just picked
/// the folder and needs to know whether to keep it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryCheck {
    pub path: String,
    pub exists: bool,
    pub writable: bool,
    pub on_system_drive: bool,
    pub removable_or_network: bool,
    pub free_gb: Option<f64>,
    pub total_gb: Option<f64>,
    /// Blocking problems. The wizard refuses to continue while this is non-empty.
    pub blockers: Vec<String>,
    /// Non-blocking advice shown next to the field.
    pub advice: Vec<String>,
}

fn system_drive() -> Option<PathBuf> {
    std::env::var_os("SystemDrive").map(|drive| PathBuf::from(format!("{}\\", drive.to_string_lossy())))
}

fn is_on_system_drive(path: &Path) -> bool {
    let Some(drive) = system_drive() else { return false };
    let mut components = path.components();
    match (components.next(), drive.components().next()) {
        (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => {
            a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
        }
        _ => false,
    }
}

fn is_network_or_removable(path: &Path) -> bool {
    use windows_sys::Win32::{
        Storage::FileSystem::GetDriveTypeW,
        System::WindowsProgramming::{DRIVE_REMOTE, DRIVE_REMOVABLE},
    };
    let Some(Component::Prefix(prefix)) = path.components().next() else { return path.to_string_lossy().starts_with("\\\\") };
    let root = format!("{}\\", prefix.as_os_str().to_string_lossy().trim_end_matches(['\\', '/']));
    let kind = unsafe { GetDriveTypeW(wide(Path::new(&root)).as_ptr()) };
    kind == DRIVE_REMOTE || kind == DRIVE_REMOVABLE
}

fn writable(path: &Path) -> bool {
    let mut probe = path;
    while !probe.is_dir() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return false,
        }
    }
    let test = probe.join(format!(".atelier-write-test-{}", std::process::id()));
    match fs::write(&test, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&test);
            true
        }
        Err(_) => false,
    }
}

pub fn inspect(kind: &str, path: &Path) -> DirectoryCheck {
    let exists = path.is_dir();
    let free = disk_space(path);
    let on_system = is_on_system_drive(path);
    let network = is_network_or_removable(path);
    let can_write = writable(path);
    let mut blockers = Vec::new();
    let mut advice = Vec::new();

    if path.as_os_str().is_empty() {
        blockers.push("还没有选择目录。".into());
    }
    if exists && is_link(path) {
        advice.push("这个目录本身就是一个联接；如果它已经是当前数据目录，保持原样即可。".into());
    }
    if !can_write {
        blockers.push("这个位置无法写入，请换一个目录或检查权限。".into());
    }
    if on_system {
        advice.push(match kind {
            "models" => "它位于系统盘。权重通常几十 GB，建议放到独立数据盘，避免拖慢系统盘并挤占更新空间。".into(),
            _ => "它位于系统盘。数据库和管线产物会持续增长，建议放到有备份的数据盘。".into(),
        });
    }
    if network {
        advice.push("它是网络或可移动磁盘。掉线或拔盘会让正在运行的任务直接失败。".into());
    }
    DirectoryCheck {
        path: path.display().to_string(),
        exists,
        writable: can_write,
        on_system_drive: on_system,
        removable_or_network: network,
        free_gb: free.map(|space| (space.free_gb * 10.0).round() / 10.0),
        total_gb: free.map(|space| (space.total_gb * 10.0).round() / 10.0),
        blockers,
        advice,
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkOutcome {
    /// True when the launcher created or repointed the junction in this call.
    pub changed: bool,
    /// True when the root directory now resolves to the user's choice.
    pub linked: bool,
    pub target: String,
}

/// Make `<root>/data` resolve to `chosen`.
///
/// Refuses to move aside a directory that already holds data: the launcher does not
/// silently relocate or delete a user's database. It only converts an empty
/// placeholder, creates the link when absent, or confirms an existing correct link.
pub fn ensure_data_link(root_data: &Path, chosen: &Path) -> Result<LinkOutcome, String> {
    let chosen_text = chosen.display().to_string();
    if !chosen.is_absolute() {
        return Err("数据目录必须是绝对路径。".into());
    }

    // The user kept `<root>/data` itself: it is already authoritative, so there is
    // nothing to link and no reason to reject an existing link back to it.
    if lexical_eq(root_data, chosen) {
        fs::create_dir_all(root_data).map_err(|e| format!("无法创建数据目录：{e}"))?;
        return Ok(LinkOutcome { changed: false, linked: is_link(root_data), target: chosen_text });
    }

    // The user kept the distribution default: no link is needed, and any existing
    // real directory is already the right place.
    if !is_link(root_data) && root_data.is_dir() && same(root_data, chosen) {
        return Ok(LinkOutcome { changed: false, linked: false, target: chosen_text });
    }

    fs::create_dir_all(chosen).map_err(|e| format!("无法创建数据目录：{e}"))?;

    if is_link(root_data) {
        if same(root_data, chosen) {
            return Ok(LinkOutcome { changed: false, linked: true, target: chosen_text });
        }
        // Repoint only an empty link target would still lose nothing; but a link
        // usually means an older choice with data, so ask instead of guessing.
        return Err(format!(
            "数据目录已经指向 {}。要换成新位置，请先把旧目录里的内容移过去，或继续使用旧位置。",
            fs::canonicalize(root_data).map(|p| p.display().to_string()).unwrap_or_else(|_| "其它目录".into())
        ));
    }

    if root_data.is_dir() {
        if !is_empty_dir(root_data) {
            return Err("发行包目录里已经有数据。启动器不会自动搬移或删除数据；请先备份并清空该目录，或继续使用它。".into());
        }
        fs::remove_dir(root_data).map_err(|e| format!("无法替换空的数据目录：{e}"))?;
    }

    if let Some(parent) = root_data.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法准备根目录：{e}"))?;
    }
    create_junction(root_data, chosen)?;
    if !same(root_data, chosen) {
        return Err("数据目录联接创建后未能解析到所选位置。".into());
    }
    Ok(LinkOutcome { changed: true, linked: true, target: chosen_text })
}

pub(crate) fn create_junction(link: &Path, target: &Path) -> Result<(), String> {
    let output = Command::new("cmd.exe")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("无法创建目录联接：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(if detail.is_empty() {
        "无法创建目录联接，目标位置可能不可写。".to_string()
    } else {
        format!("无法创建目录联接：{detail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_equality_ignores_case_and_trailing_separator() {
        assert!(lexical_eq(Path::new(r"C:\Atelier\Data"), Path::new(r"c:\atelier\data\")));
        assert!(lexical_eq(Path::new(r"C:\Atelier\Data"), Path::new(r"C:\Atelier\Data")));
        assert!(!lexical_eq(Path::new(r"C:\Atelier\Data"), Path::new(r"D:\Atelier\Data")));
    }

    #[test]
    fn disk_space_answers_for_an_existing_volume() {
        let Some(space) = disk_space(Path::new(r"C:\")) else { return };
        assert!(space.total_gb > 0.0);
        assert!(space.free_gb <= space.total_gb);
    }

    #[test]
    fn inspection_reports_a_writable_system_folder() {
        let check = inspect("models", &std::env::temp_dir());
        assert!(check.exists);
        assert!(check.writable);
        assert!(check.blockers.is_empty());
    }

    #[test]
    fn system_drive_advice_matches_the_directory_purpose() {
        let data = inspect("data", Path::new(r"C:\AtelierFree\data"));
        let models = inspect("models", Path::new(r"C:\AtelierFree\models"));
        let data_note = data.advice.join(" ");
        let models_note = models.advice.join(" ");
        assert!(data_note.contains("备份"));
        assert!(models_note.contains("GB"));
    }

    /// The relocation path that cannot be unit-tested by reading code: it creates a
    /// real junction, so writes through `<root>/data` must land in the chosen folder.
    #[test]
    fn ensure_data_link_redirects_the_root_directory() {
        let base = std::env::temp_dir().join(format!("atelier-link-{}", std::process::id()));
        let root = base.join("root");
        let chosen = base.join("chosen");
        fs::create_dir_all(root.join("data")).expect("scratch root");
        fs::create_dir_all(&chosen).expect("scratch target");

        let outcome = ensure_data_link(&root.join("data"), &chosen).expect("link created");
        assert!(outcome.changed);
        assert!(outcome.linked);
        assert!(same(&root.join("data"), &chosen), "root data must resolve to the choice");

        fs::write(root.join("data").join("probe.txt"), b"x").expect("write through link");
        assert!(chosen.join("probe.txt").is_file(), "write must land in the chosen folder");

        // Removing the link must not delete the user's data behind it.
        fs::remove_dir(root.join("data")).expect("remove link");
        assert!(!root.join("data").exists());
        assert!(chosen.join("probe.txt").is_file(), "target survives link removal");
        let _ = fs::remove_dir_all(&base);
    }

    /// Refusing is the important half: the launcher must never move or delete data
    /// that is already sitting in `<root>/data`.
    #[test]
    fn ensure_data_link_refuses_to_replace_a_directory_with_data() {
        let base = std::env::temp_dir().join(format!("atelier-link-guard-{}", std::process::id()));
        let root = base.join("root");
        let chosen = base.join("chosen");
        fs::create_dir_all(root.join("data")).expect("scratch root");
        fs::create_dir_all(&chosen).expect("scratch target");
        fs::write(root.join("data").join("atelier_kernel.db"), b"keep").expect("seed data");

        let refused = ensure_data_link(&root.join("data"), &chosen);
        assert!(refused.is_err(), "must refuse to relocate an occupied data directory");
        assert!(root.join("data").join("atelier_kernel.db").is_file(), "data must be untouched");
        assert!(!is_link(&root.join("data")));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn keeping_the_default_directory_needs_no_link() {
        let base = std::env::temp_dir().join(format!("atelier-link-default-{}", std::process::id()));
        let data = base.join("data");
        fs::create_dir_all(&data).expect("scratch root");
        fs::write(data.join("atelier_kernel.db"), b"existing").expect("seed data");

        let outcome = ensure_data_link(&data, &data).expect("default keeps working");
        assert!(!outcome.changed);
        assert!(!is_link(&data));
        assert!(data.join("atelier_kernel.db").is_file());
        let _ = fs::remove_dir_all(&base);
    }
}
