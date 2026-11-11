//! Weight-download credential storage.
//!
//! The token is encrypted with DPAPI under the signed-in user's profile, so it is
//! readable only by that Windows account on that machine. It never goes into a
//! plain-text config file, never into the distribution's `kernel_boot.json`, and
//! never into a log line: the only trace the launcher writes is the last four
//! characters, which is not enough to reconstruct it.

use std::{fs, path::PathBuf};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB},
};

/// DPAPI refuses to raise UI; the launcher is the UI.
const UI_FORBIDDEN: u32 = 0x1;

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 }
}

fn credential_path() -> Result<PathBuf, String> {
    Ok(crate::settings::state_dir()?.join("credential.bin"))
}

fn encrypt(plain: &str) -> Result<Vec<u8>, String> {
    let input = blob(plain.as_bytes());
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), UI_FORBIDDEN, &mut output)
    };
    if ok == 0 {
        return Err(format!("无法加密凭据：{}", std::io::Error::last_os_error()));
    }
    let result = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut std::ffi::c_void) };
    Ok(result)
}

fn decrypt(cipher: &[u8]) -> Result<String, String> {
    let input = blob(cipher);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), UI_FORBIDDEN, &mut output)
    };
    if ok == 0 {
        return Err(format!("无法解密凭据：{}", std::io::Error::last_os_error()));
    }
    let result = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut std::ffi::c_void) };
    String::from_utf8(result).map_err(|_| "凭据内容损坏".to_string())
}

/// Encrypt and persist the token. An empty value clears the stored credential.
pub fn store(token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        clear();
        return Ok(());
    }
    let path = credential_path()?;
    let parent = path.parent().ok_or("凭据路径无效")?;
    fs::create_dir_all(parent).map_err(|e| format!("无法创建凭据目录：{e}"))?;
    let cipher = encrypt(trimmed)?;
    let temp = path.with_extension("bin.tmp");
    fs::write(&temp, cipher).map_err(|e| format!("无法写入凭据：{e}"))?;
    fs::rename(&temp, &path).map_err(|e| format!("无法保存凭据：{e}"))
}

/// Read the token back for injection into the kernel environment.
pub fn load() -> Option<String> {
    let path = credential_path().ok()?;
    let cipher = fs::read(path).ok()?;
    decrypt(&cipher).ok().filter(|token| !token.trim().is_empty())
}

pub fn clear() {
    if let Ok(path) = credential_path() {
        let _ = fs::remove_file(path);
    }
}

/// Non-reversible reminder for the UI. Never returns the token itself.
pub fn hint() -> Option<String> {
    let token = load()?;
    let trimmed = token.trim();
    let tail: String = trimmed.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    Some(format!("hf_••••••••{tail}"))
}
