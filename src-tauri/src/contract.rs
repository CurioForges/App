//! The single place the launcher's understanding of the distribution contract lives.
//!
//! Everything here is taken from the AtelierFree handoff document and verified
//! against the shipped sources, not invented:
//!
//! * `ATELIER_ROOT` is the only root the distribution accepts. `start.ps1` sets it
//!   and the kernel derives every domain working directory from the configuration
//!   file's parent directory.
//! * The model root has its own first-class variable. `domains/engines/base/weights.py`
//!   resolves `models_dir()` as `GCP_MODELS_DIR` and only falls back to
//!   `<ATELIER_ROOT>/models`. `domains/engines/config/runtime_settings.py` derives
//!   every HF / torch / NeMo / ModelScope / Paddle cache from that same directory.
//! * Weights need a token for gated repositories. The engines domain reads
//!   `HUGGINGFACE_TOKEN` and `HF_TOKEN` from the inherited environment
//!   (`runtime_settings.worker_environment`), so setting them on the kernel process
//!   is the supported injection point.
//! * There is no `ATELIER_DATA_DIR`. Domain code hardcodes `<ATELIER_ROOT>/data`
//!   (for example `domains/scheduler/impl/publish_service.py`), so a user-chosen
//!   data directory can only be honoured inside that contract by making
//!   `<ATELIER_ROOT>/data` a directory junction.
//!
//! Keeping these names here means a future contract change is a one-line edit.

/// Root the distribution derives every runtime path from.
pub const ENV_ROOT: &str = "ATELIER_ROOT";

/// Model root override read by the engines domain.
///
/// Precedence, straight from `runtime_settings.worker_environment`: a `models_dir`
/// saved in the app's own engine settings wins, then this inherited variable, then
/// `<ATELIER_ROOT>/models`. The launcher sets the middle one, so a choice made in
/// the app later still takes effect without the launcher fighting it.
pub const ENV_MODELS_DIR: &str = "GCP_MODELS_DIR";

/// Both aliases the engines domain accepts for the same credential.
pub const ENV_HF_TOKEN: [&str; 2] = ["HUGGINGFACE_TOKEN", "HF_TOKEN"];

/// Directories the distribution creates under the root; the launcher must never
/// clobber or relocate these on its own beyond what the user chose explicitly.
pub const PRESERVED: [&str; 3] = ["data", "engine-root", "models"];

/// Directory under the root that hosts user data. Relocated through a junction.
pub const DATA: &str = "data";

/// Directory under the root that hosts model weights when no override is set.
pub const MODELS: &str = "models";

/// Health endpoint of the web domain used to decide the workbench is really up.
pub fn assembly_path() -> &'static str {
    "/api/assembly"
}

/// 工作台界面所在的域。启动器靠它找到 HTTP 端口，不代表域清单是固定的：
/// 要等哪些域起来，读的是实际写下的 `kernel_boot.json`（见 `bootspec::domain_ids`）。
pub const WORKBENCH_DOMAIN: &str = "web";
