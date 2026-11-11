use std::{env, fs, io::{BufRead, BufReader}, net::{SocketAddr, TcpListener, TcpStream}, path::{Path, PathBuf}, process::{Command, Stdio}, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex}, thread, time::{Duration, Instant}};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use serde::Serialize;
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::{AppHandle, Emitter, State};

pub fn install_dir() -> Result<PathBuf, String> {
    env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("AtelierFree")).ok_or("LOCALAPPDATA 未设置".into())
}
#[derive(Debug)]
struct RunningKernel { pid: u32, port: Option<u16> }

/// Every kernel process launched from this installation, newest first.
///
/// A crashed or manually started instance used to leave an orphan that kept its
/// port bound; reporting only the first match made the launcher read a stale port.
/// Ordering by start time makes the reported instance the one actually serving, and
/// lets shutdown reclaim all of them.
fn running_kernels(dir: &Path) -> Result<Vec<RunningKernel>, String> {
    // A single native process snapshot replaces launching PowerShell and querying WMI.
    let mut system = System::new().map_err(|e| format!("无法读取进程信息：{e}"))?;
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet).with_cmd(UpdateKind::OnlyIfNotSet));
    let target = dir.join("atelier-kernel.exe");
    let target = target.as_os_str().to_string_lossy();
    let mut owners: Vec<_> = system
        .processes()
        .values()
        .filter(|process| {
            process.exe().is_some_and(|p| p.as_os_str().to_string_lossy().eq_ignore_ascii_case(&target))
                && !process.cmd().iter().any(|arg| arg == "--port-relay")
        })
        .collect();
    owners.sort_by_key(|process| std::cmp::Reverse((process.start_time(), process.pid().as_u32())));
    Ok(owners
        .into_iter()
        .map(|owner| {
            let port = system.processes().values().filter(|process| process.parent() == Some(owner.pid())).find_map(|process| {
                let args = process.cmd();
                let index = args.iter().position(|arg| arg == "--port-relay")?;
                args.get(index + 1)?.to_string_lossy().parse::<SocketAddr>().ok().map(|address| address.port())
            });
            RunningKernel { pid: owner.pid().as_u32(), port }
        })
        .collect())
}

fn running_kernel(dir: &Path) -> Result<Option<RunningKernel>, String> {
    Ok(running_kernels(dir)?.into_iter().next())
}
/// HTTP port of the newest live instance, if any.
pub fn live_port(dir: &Path) -> Result<Option<u16>, String> {
    Ok(running_kernel(dir)?.and_then(|kernel| kernel.port))
}

/// Kernels belonging to a *different* installation that are still running.
///
/// Only a real installation counts: a source checkout keeps just the kernel binary
/// in its runtime directory, and that is not a product the user installed.
fn other_install_kernels(dir: &Path) -> Result<Vec<(u32, PathBuf)>, String> {
    let mut system = System::new().map_err(|e| format!("无法读取进程信息：{e}"))?;
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet).with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let own = dir.as_os_str().to_string_lossy().to_ascii_lowercase();
    let mut found = Vec::new();
    for process in system.processes().values() {
        let Some(exe) = process.exe() else { continue };
        let text = exe.as_os_str().to_string_lossy().to_string();
        if !text.to_ascii_lowercase().ends_with("atelier-kernel.exe") {
            continue;
        }
        if process.cmd().iter().any(|arg| arg == "--port-relay") {
            continue;
        }
        if text.to_ascii_lowercase().starts_with(&own) {
            continue;
        }
        let Some(root) = exe.parent() else { continue };
        found.push((process.pid().as_u32(), root.to_path_buf()));
    }
    Ok(found)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignInstall {
    pub pid: u32,
    pub path: String,
}

/// Installations that must be stopped before this one may start. The launcher asks
/// the user before acting on this; it never stops another product on its own.
#[tauri::command]
pub async fn foreign_installs() -> Result<Vec<ForeignInstall>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let dir = install_dir()?;
        Ok(other_install_kernels(&dir)?
            .into_iter()
            .map(|(pid, path)| ForeignInstall { pid, path: path.display().to_string() })
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}
pub fn kernel_running(dir: &Path) -> Result<bool, String> { Ok(running_kernel(dir)?.is_some()) }

/// Ask one kernel to shut down, then wait for the process to exit. Waiting matters:
/// the caller usually starts a replacement straight away.
///
/// Returns `true` when the kernel accepted the named stop event, and `false` when
/// the launcher had to terminate the process tree instead. The event only exists in
/// kernels from Atelier Kernel `0.5.2` onwards; the shipped AtelierFree `v0.1.3`
/// binary predates it, so a launcher that only knows the polite path cannot stop
/// its own kernel at all. Terminating the tree is what the distribution's own
/// installer does, and the kernel is the process tree root, so nothing is orphaned.
#[cfg(windows)]
fn stop_one(pid: u32) -> Result<bool, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, GetLastError, WAIT_OBJECT_0},
        System::Threading::{EVENT_MODIFY_STATE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject},
    };
    let name: Vec<u16> = format!("Local\\AtelierKernelShutdown-{pid}").encode_utf16().chain(Some(0)).collect();
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        // The process disappeared between the snapshot and this call; nothing to stop.
        return Ok(true);
    }
    let mut graceful = true;
    let mut failure = None;
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if event.is_null() {
            let code = GetLastError();
            if code == ERROR_FILE_NOT_FOUND {
                graceful = false;
            } else {
                failure = Some(format!("无法发送停止请求：{}", std::io::Error::from_raw_os_error(code as i32)));
            }
        } else {
            if SetEvent(event) == 0 {
                failure = Some(format!("无法发送停止请求：{}", std::io::Error::last_os_error()));
            }
            CloseHandle(event);
        }
    }
    // A kernel that publishes the stop event shuts down on its own; one that does
    // not (the shipped v0.1.3 release) has to be ended as a process tree instead.
    if failure.is_none() && !graceful {
        if let Err(error) = force_stop_tree(pid) {
            failure = Some(error);
        }
    }
    unsafe {
        // Bounded wait: a wedged process must not pin the launcher UI forever.
        let waited = WaitForSingleObject(process, 30_000);
        let wait_error = (waited != WAIT_OBJECT_0).then(|| match waited {
            0x0000_0102 => "内核在 30 秒内没有退出。".to_string(),
            _ => format!("等待内核退出失败：{}", std::io::Error::last_os_error()),
        });
        CloseHandle(process);
        if let Some(error) = failure {
            return Err(error);
        }
        if let Some(error) = wait_error {
            return Err(error);
        }
    }
    Ok(graceful)
}

/// End the kernel and every domain it supervises. `taskkill /T` walks the child
/// tree, which is exactly the supervision relationship the kernel owns.
#[cfg(windows)]
fn force_stop_tree(pid: u32) -> Result<(), String> {
    let output = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("无法结束内核进程：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stdout);
    let detail = detail.trim();
    Err(if detail.is_empty() {
        "无法结束内核进程。".to_string()
    } else {
        format!("无法结束内核进程：{detail}")
    })
}

/// Stop every kernel launched from this installation. Reclaims orphans left behind
/// by a crash too, because they hold the same ports and the same data.
/// `Ok(true)` means all of them stopped through the kernel's own stop event.
pub fn stop_sync() -> Result<bool, String> {
    let dir = install_dir()?;
    let kernels = running_kernels(&dir)?;
    let mut failure = None;
    let mut graceful = true;
    for kernel in kernels {
        #[cfg(windows)]
        {
            match stop_one(kernel.pid) {
                Ok(polite) => graceful = graceful && polite,
                Err(error) => failure = Some(error),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = kernel;
            failure = Some("当前平台尚不支持停止变体。".to_string());
        }
    }
    failure.map_or(Ok(graceful), Err)
}
#[tauri::command]
pub async fn stop_variant() -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(stop_sync).await.map_err(|e| e.to_string())?
}

fn web_port(config: &Value) -> Option<u16> {
    config.get("domains")?.as_array()?.iter().find(|d| d.get("domain_id").and_then(Value::as_str) == Some("web"))?.get("ports")?.as_array()?.iter().find(|p| p.get("name").and_then(Value::as_str) == Some("http"))?.get("listen")?.as_str()?.parse::<SocketAddr>().ok().map(|a| a.port())
}
/// The complete set of contract variables the kernel process receives.
///
/// Kept separate from process spawning so the contract itself is testable: these
/// are the only three things the launcher tells the distribution about user
/// choices, and every one of them is a variable the distribution already reads.
fn variant_environment(root: &Path, models: &Path, token: Option<&str>) -> Vec<(String, std::ffi::OsString)> {
    let mut values = vec![
        (crate::contract::ENV_ROOT.to_string(), root.as_os_str().to_os_string()),
        (crate::contract::ENV_MODELS_DIR.to_string(), models.as_os_str().to_os_string()),
    ];
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        for name in crate::contract::ENV_HF_TOKEN {
            values.push((name.to_string(), std::ffi::OsString::from(token)));
        }
    }
    values
}
fn configured_port(dir: &Path) -> Option<u16> {
    let managed = dir.join("kernel_boot.atelier.json");
    let config = if managed.exists() { managed } else { dir.join("kernel_boot.json") };
    fs::read(config).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).and_then(|v| web_port(&v))
}
#[derive(Serialize)]
pub struct VariantInfo { installed: bool, path: String, port: Option<u16> }
#[tauri::command]
pub async fn variant_info() -> Result<VariantInfo, String> {
    tauri::async_runtime::spawn_blocking(|| {
    let path = install_dir()?;
    let installed = path.join("start.ps1").is_file() && path.join("atelier-kernel.exe").is_file();
    let port = if installed { match running_kernel(&path)? { Some(kernel) => kernel.port.or_else(|| configured_port(&path)), None => configured_port(&path) } } else { None };
    Ok(VariantInfo { installed, path: path.display().to_string(), port })
    }).await.map_err(|e| e.to_string())?
}

// Keep chosen ports reserved until all bindings have been assigned.
fn assign_ports(config: &mut Value) -> Result<(u16, Vec<TcpListener>), String> {
    let domains = config.get_mut("domains").and_then(Value::as_array_mut).ok_or("变体配置缺少 domains")?;
    let mut reservations = Vec::new();
    let mut web = None;
    for domain in domains {
        let is_web = domain.get("domain_id").and_then(Value::as_str) == Some("web");
        if let Some(ports) = domain.get_mut("ports").and_then(Value::as_array_mut) {
            for port in ports {
                let name = port.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                let listen = port.get("listen").and_then(Value::as_str).ok_or("端口配置缺少 listen")?;
                let address: SocketAddr = listen.parse().map_err(|e| format!("端口地址无效：{e}"))?;
                let listener = TcpListener::bind(SocketAddr::new(address.ip(), 0)).map_err(|e| format!("无法分配端口：{e}"))?;
                let allocated = listener.local_addr().map_err(|e| e.to_string())?;
                if is_web && name == "http" { web = Some(allocated.port()); }
                port["listen"] = Value::String(allocated.to_string());
                reservations.push(listener);
            }
        }
    }
    Ok((web.ok_or("变体未声明 Web HTTP 端口")?, reservations))
}
fn prepare_binding(dir: &Path) -> Result<(PathBuf, u16, Vec<TcpListener>), String> {
    let source = fs::read(dir.join("kernel_boot.json")).map_err(|e| format!("无法读取变体配置：{e}"))?;
    let mut config: Value = serde_json::from_slice(&source).map_err(|e| format!("变体配置无效：{e}"))?;
    let (port, reservations) = assign_ports(&mut config)?;
    let target = dir.join("kernel_boot.atelier.json");
    fs::write(&target, serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?).map_err(|e| format!("无法保存端口绑定：{e}"))?;
    Ok((target, port, reservations))
}
#[tauri::command]
pub async fn open_workbench(online_port: Option<u16>, stop_foreign: Option<bool>) -> Result<u16, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(port) = online_port { return Ok(port); }
        let dir = install_dir()?;
        let port = match running_kernel(&dir)? {
            Some(kernel) => return kernel.port.ok_or("变体正在启动，暂时无法读取端口绑定".into()),
            None => {
                // One instance at a time. Stopping another installation is the user's
                // decision, so this refuses unless the caller explicitly consented.
                let foreign = other_install_kernels(&dir)?;
                if !foreign.is_empty() {
                    if stop_foreign != Some(true) {
                        let paths: Vec<String> = foreign.iter().map(|(_, path)| path.display().to_string()).collect();
                        return Err(format!("另一个安装正在运行：{}。需要先停止它。", paths.join("；")));
                    }
                    for (pid, root) in &foreign {
                        stop_one(*pid).map_err(|e| format!("无法停止 {}：{e}", root.display()))?;
                    }
                }
                let settings = crate::settings::load();
                let (config, port, reservations) = prepare_binding(&dir)?;
                // The three user choices are applied here, and only here: `data/` is a
                // junction inside the root, models are an environment override the
                // engines domain documents, and the credential is injected through the
                // two aliases that domain already reads. No domain config is rewritten.
                crate::locations::ensure_data_link(&dir.join(crate::contract::DATA), &settings.data_path(&dir, crate::catalog::INSTALL_ID))?;
                let models = settings.models_path(&dir);
                fs::create_dir_all(&models).map_err(|e| format!("无法创建模型目录：{e}"))?;
                fs::create_dir_all(dir.join("engine-root")).map_err(|e| e.to_string())?;
                let mut paths = vec![dir.join("engine-root")];
                if let Some(path) = env::var_os("PATH") { paths.extend(env::split_paths(&path)); }
                let path = env::join_paths(paths).map_err(|e| e.to_string())?;
                let mut command = Command::new(dir.join("atelier-kernel.exe"));
                #[cfg(windows)]
                command.creation_flags(0x08000000);
                command
                    .args(["--headless", "--config"])
                    .arg(config)
                    .current_dir(&dir)
                    .env("PATH", path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                for (key, value) in variant_environment(&dir, &models, crate::credentials::load().as_deref()) {
                    command.env(key, value);
                }
                drop(reservations); // The kernel must bind the selected ports itself.
                command.spawn().map_err(|e| format!("无法启动 Atelier Free：{e}"))?;
                port
            }
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut reachable = false;
        for _ in 0..120 {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
                reachable = true;
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        if !reachable {
            return Err("工作台启动超时，请检查变体运行状态。".to_string());
        }
        // A listening port is not the same as a usable workbench: wait for the four
        // domains to report active on the distribution's own health endpoint. This is
        // the contract the handoff document names, and it is what the acceptance
        // checklist means by "四域 active".
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if all_domains_active(port) {
                break;
            }
            thread::sleep(Duration::from_millis(750));
        }
        Ok(port)
    }).await.map_err(|e| e.to_string())?
}

/// True once every domain the distribution declares reports `active`.
fn all_domains_active(port: u16) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let url = format!("http://127.0.0.1:{port}{}", crate::contract::assembly_path());
    let Ok(response) = client.get(&url).send() else { return false };
    if !response.status().is_success() {
        return false;
    }
    let Ok(body) = response.json::<Value>() else { return false };
    let Some(domains) = body.get("domains").and_then(Value::as_array) else { return false };
    crate::contract::DOMAINS.iter().all(|id| {
        domains.iter().any(|domain| {
            domain.get("domain_id").and_then(Value::as_str) == Some(id)
                && domain.get("status").and_then(Value::as_str).map(|status| status.eq_ignore_ascii_case("active")) == Some(true)
        })
    })
}

#[derive(Clone, Serialize)]
struct StatusEvent { id: &'static str, port: u16, status: &'static str }
struct Watch { port: u16, stopped: Arc<AtomicBool> }
#[derive(Default)]
pub struct StatusWatch(Mutex<Option<Watch>>);
fn send_status(app: &AppHandle, stop: &AtomicBool, last: &mut Option<&'static str>, port: u16, status: &'static str) {
    if !stop.load(Ordering::Relaxed) && *last != Some(status) {
        let _ = app.emit("variant-status", StatusEvent { id: "free", port, status });
        *last = Some(status);
    }
}
fn watch_events(app: AppHandle, port: u16, stopped: Arc<AtomicBool>) {
    let client = match reqwest::blocking::Client::builder().connect_timeout(Duration::from_secs(2)).no_proxy().build() { Ok(client) => client, Err(_) => return };
    let url = format!("http://127.0.0.1:{port}/api/events?topics=runtime");
    let mut last = None;
    while !stopped.load(Ordering::Relaxed) {
        if let Ok(response) = client.get(&url).send() {
            if response.status().is_success() {
                let mut runtime_event = false;
                for line in BufReader::new(response).lines() {
                    if stopped.load(Ordering::Relaxed) { return; }
                    match line {
                        Ok(line) if line.starts_with("event:") => runtime_event = line[6..].trim() == "runtime",
                        Ok(line) if runtime_event && line.starts_with("data:") => {
                            if let Ok(snapshot) = serde_json::from_str::<Value>(line[5..].trim()) {
                                let active = snapshot.get("domains").and_then(Value::as_array).map(|domains| domains.iter().any(|d| d.get("domain_id").and_then(Value::as_str) == Some("web") && d.get("status").and_then(Value::as_str).map(|s| s.eq_ignore_ascii_case("active")) == Some(true))).unwrap_or(false);
                                send_status(&app, &stopped, &mut last, port, if active { "online" } else { "offline" });
                            }
                        }
                        Ok(line) if line.is_empty() => runtime_event = false,
                        Ok(_) => {},
                        Err(_) => break,
                    }
                }
            }
        }
        send_status(&app, &stopped, &mut last, port, "offline");
        // Reconnect the event stream; no status query loop is used.
        for _ in 0..6 { if stopped.load(Ordering::Relaxed) { return; } thread::sleep(Duration::from_millis(500)); }
    }
}
#[tauri::command]
pub fn watch_variant(app: AppHandle, state: State<'_, StatusWatch>, port: Option<u16>) -> Result<(), String> {
    let mut current = state.0.lock().map_err(|e| e.to_string())?;
    if current.as_ref().map(|w| w.port) == port { return Ok(()); }
    if let Some(old) = current.take() { old.stopped.store(true, Ordering::Relaxed); }
    if let Some(port) = port {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        thread::spawn(move || watch_events(app, port, flag));
        *current = Some(Watch { port, stopped });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocates_distinct_ports_and_preserves_other_settings() {
        let mut config = serde_json::json!({"db_path":"data/db", "domains":[{"domain_id":"web","command":"web.exe","ports":[{"name":"http","listen":"127.0.0.1:8655"}]},{"domain_id":"other","command":"other.exe","ports":[{"name":"api","listen":"127.0.0.1:9000"}]}]});
        let (port, reservations) = assign_ports(&mut config).unwrap();
        assert_ne!(reservations[0].local_addr().unwrap().port(), reservations[1].local_addr().unwrap().port());
        assert_eq!(web_port(&config), Some(port));
        assert_eq!(config["db_path"], "data/db");
        assert_eq!(config["domains"][0]["command"], "web.exe");
        assert!(TcpListener::bind(SocketAddr::from(([127,0,0,1], port))).is_err());
    }

    #[test]
    fn kernel_environment_carries_the_documented_contract_variables() {
        let values = variant_environment(Path::new(r"C:\root"), Path::new(r"D:\weights"), Some("hf_fixture"));
        let read = |name: &str| values.iter().find(|(key, _)| key == name).map(|(_, value)| value.to_string_lossy().into_owned());
        assert_eq!(read("ATELIER_ROOT").as_deref(), Some(r"C:\root"));
        assert_eq!(read("GCP_MODELS_DIR").as_deref(), Some(r"D:\weights"));
        assert_eq!(read("HUGGINGFACE_TOKEN").as_deref(), Some("hf_fixture"));
        assert_eq!(read("HF_TOKEN").as_deref(), Some("hf_fixture"));
    }

    #[test]
    fn no_stored_token_means_no_credential_variable() {
        let values = variant_environment(Path::new(r"C:\root"), Path::new(r"C:\root\models"), None);
        assert!(values.iter().all(|(key, _)| key != "HUGGINGFACE_TOKEN" && key != "HF_TOKEN"));
        let empty = variant_environment(Path::new(r"C:\root"), Path::new(r"C:\root\models"), Some("   "));
        assert!(empty.iter().all(|(key, _)| key != "HUGGINGFACE_TOKEN" && key != "HF_TOKEN"));
    }
}
