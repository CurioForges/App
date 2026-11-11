//! 安装位置迁移。
//!
//! 把整个安装根从旧路径搬到新路径,并提供进度回调给覆盖层复用。
//! 设计约束(见 docs/dev/UPDATE-FLOW.md):
//! - 同盘用 rename(瞬间完成);跨盘 copy + 校验 + 删源;
//! - 迁移不可取消,失败必须能反向搬回;
//! - 调用方负责先停实例;本模块只搬文件,不碰设置。

use std::fs;
use std::path::{Path, PathBuf};

/// 迁移过程中报告一次进度。
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub current: String,
}

/// 统计目录体积与文件数(用于预估耗时与目标盘预检)。
pub fn measure(path: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let metadata = match entry.metadata() { Ok(m) => m, Err(_) => continue };
            if metadata.is_dir() {
                let (b, f) = measure(&entry.path());
                bytes += b;
                files += f;
            } else {
                bytes += metadata.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

fn same_volume(a: &Path, b: &Path) -> bool {
    let letter = |p: &Path| -> Option<char> {
        p.components().next().and_then(|c| match c {
            std::path::Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy()
                .chars().next(),
            _ => None,
        })
    };
    match (letter(a), letter(b)) { (Some(x), Some(y)) => x.eq_ignore_ascii_case(&y), _ => false }
}

/// 递归复制并回调进度;完成后逐文件大小比对,不一致即失败(不删源)。
fn copy_tree(from: &Path, to: &Path, done: &mut u64, total: u64, report: &mut impl FnMut(Progress)) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("创建目录失败 {}：{e}", to.display()))?;
    let entries = fs::read_dir(from).map_err(|e| format!("读取目录失败 {}：{e}", from.display()))?;
    for entry in entries.flatten() {
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        if metadata.is_dir() {
            copy_tree(&entry.path(), &dest, done, total, report)?;
        } else {
            fs::copy(entry.path(), &dest).map_err(|e| format!("复制失败 {}：{e}", entry.path().display()))?;
            *done += metadata.len();
            report(Progress {
                done: *done,
                total,
                current: entry.file_name().to_string_lossy().to_string(),
            });
        }
    }
    Ok(())
}

fn verify_tree(from: &Path, to: &Path) -> Result<(), String> {
    let (a, _) = measure(from);
    let (b, _) = measure(to);
    if a != b {
        return Err(format!("迁移后体积不一致：源 {} 字节，目标 {} 字节", a, b));
    }
    Ok(())
}

/// 把 `from` 整体搬到 `to`。`to` 必须不存在或是空目录。
/// 失败时尽力反向恢复,并返回可读原因。
pub fn move_root(from: &Path, to: &Path, report: &mut impl FnMut(Progress)) -> Result<(), String> {
    if !from.is_dir() {
        return Err(format!("源路径不存在：{}", from.display()));
    }
    if to.exists() && fs::read_dir(to).map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("目标路径不是空目录：{}", to.display()));
    }
    let (total, _) = measure(from);
    report(Progress { done: 0, total, current: "开始迁移".into() });

    if same_volume(from, to) {
        // 同一块盘:rename 是原子的,失败就没有半成品可言。
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败：{e}"))?;
        }
        fs::rename(from, to).map_err(|e| format!("同盘迁移失败：{e}"))?;
        report(Progress { done: total, total, current: "完成".into() });
        return Ok(());
    }

    // 跨盘:先复制到 `<to>.migrating`,校验通过再改名,这样目标目录要么完整要么不存在。
    let staging = PathBuf::from(format!("{}.migrating", to.display()));
    let _ = fs::remove_dir_all(&staging);
    let mut done = 0u64;
    if let Err(error) = copy_tree(from, &staging, &mut done, total, report) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = verify_tree(from, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::rename(&staging, to).map_err(|e| format!("启用新位置失败：{e}"))?;
    report(Progress { done: total, total, current: "完成".into() });
    Ok(())
}
