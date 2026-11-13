//! Windows 原生调用：能不启动外壳进程就别启动。
//!
//! 为什么单独一层：`reg.exe`、`powershell.exe`、`certutil.exe` 都是控制台程序，
//! 从 GUI 进程里拉起来即使加了 `CREATE_NO_WINDOW` 也容易在各种安全软件/终端环境下
//! 闪一下窗口。装一次发行版要"读代理 + 算哈希 + 查剩余空间"，一次更新闪十几次黑框，
//! 用户看到的就是"一直弹窗"。这三个都改成直接调 API：不起进程、无窗口、还更快。

use std::io::Read;
use std::path::Path;

use windows_sys::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
    BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
    BCRYPT_SHA256_ALGORITHM,
};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_DWORD, RRF_RT_REG_SZ,
};

const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
const SHA256_BYTES: usize = 32;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 目标盘剩余空间（字节）。`None` 表示取不到，调用方不应因此拒绝安装。
pub(crate) fn free_bytes(dir: &Path) -> Option<u64> {
    let path = wide(&dir.to_string_lossy());
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(path.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(free)
}

/// `HKCU\...\Internet Settings` 下的 DWORD 值。
pub(crate) fn internet_setting_dword(name: &str) -> Option<u32> {
    let subkey = wide(INTERNET_SETTINGS);
    let value = wide(name);
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let mut kind: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_DWORD,
            &mut kind,
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

/// `HKCU\...\Internet Settings` 下的字符串值。
pub(crate) fn internet_setting_string(name: &str) -> Option<String> {
    let subkey = wide(INTERNET_SETTINGS);
    let value = wide(name);
    let mut buffer = [0u16; 1024];
    let mut size: u32 = (buffer.len() * std::mem::size_of::<u16>()) as u32;
    let mut kind: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            &mut kind,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

/// 文件 SHA-256（小写十六进制）。走 CNG，不启动 `certutil`。
pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    unsafe {
        let mut algorithm: BCRYPT_ALG_HANDLE = std::ptr::null_mut();
        if BCryptOpenAlgorithmProvider(
            &mut algorithm,
            BCRYPT_SHA256_ALGORITHM,
            std::ptr::null(),
            0,
        ) != 0
        {
            return Err("系统没有可用的 SHA-256 实现".into());
        }
        let mut hash: BCRYPT_HASH_HANDLE = std::ptr::null_mut();
        if BCryptCreateHash(algorithm, &mut hash, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0) != 0 {
            BCryptCloseAlgorithmProvider(algorithm, 0);
            return Err("无法创建 SHA-256 上下文".into());
        }
        let mut buffer = vec![0u8; 1 << 20];
        let mut failure: Option<String> = None;
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if BCryptHashData(hash, buffer.as_ptr(), read as u32, 0) != 0 {
                        failure = Some("SHA-256 计算失败".into());
                        break;
                    }
                }
                Err(error) => {
                    failure = Some(format!("读取 {} 失败：{error}", path.display()));
                    break;
                }
            }
        }
        let mut digest = [0u8; SHA256_BYTES];
        let finished = BCryptFinishHash(hash, digest.as_mut_ptr(), SHA256_BYTES as u32, 0);
        BCryptDestroyHash(hash);
        BCryptCloseAlgorithmProvider(algorithm, 0);
        if let Some(message) = failure {
            return Err(message);
        }
        if finished != 0 {
            return Err("SHA-256 计算失败".into());
        }
        Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_known_digest_of_empty_input() {
        let path = std::env::temp_dir().join("atelier-empty-hash.txt");
        std::fs::write(&path, b"").expect("write temp file");
        let digest = sha256_file(&path).expect("hash");
        let _ = std::fs::remove_file(&path);
        assert_eq!(digest, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn sha256_matches_a_known_digest_of_text() {
        let path = std::env::temp_dir().join("atelier-abc-hash.txt");
        std::fs::write(&path, b"abc").expect("write temp file");
        let digest = sha256_file(&path).expect("hash");
        let _ = std::fs::remove_file(&path);
        assert_eq!(digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn free_space_is_reported_for_an_existing_drive() {
        let root = std::env::temp_dir();
        let free = free_bytes(&root).expect("free space");
        assert!(free > 0, "剩余空间不该是 0");
    }
}
