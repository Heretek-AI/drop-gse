//! gse-engine — core emulator patcher, DLL manager & config generator.
//!
//! Module layout per `docs/architecture/SPECIFICATION.md` §3.1:
//! - `anticheat` — fail-closed pre-flight gate
//! - `dll`       — hash-manifested backup/restore
//! - `interfaces`— Steam interface extraction from the ORIGINAL binary
//! - `fetch`     — pinned-release download + digest verification
//! - `config`    — `steam_settings/` generation
//! - `patch`     — orchestration of the full pipeline

pub mod anticheat;
pub mod cli;
pub mod config;
pub mod dll;
pub mod error;
pub mod fetch;
pub mod interfaces;
pub mod patch;

/// Target emulator distributions the engine can deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmulatorFlavor {
    /// Detanup01/gbe_fork — drop-in replacement build.
    GbeFork,
    /// alex47exe/gse_fork — gbe_fork fork with overlay/HDR work.
    GseFork,
}

/// High-level patch operation performed against an installed game directory.
#[derive(Debug, Clone)]
pub struct PatchPlan {
    pub flavor: EmulatorFlavor,
    pub app_id: u32,
    /// Mesh peer addresses to write into `custom_broadcasts.txt`.
    pub broadcast_peers: Vec<String>,
    /// Emulator listen port override; `None` = default 47584.
    pub listen_port: Option<u16>,
}
