//! `gse-engine` CLI — standalone driver for the patch pipeline.
//!
//! Subcommands:
//! - `patch`      — full pipeline (anticheat gate → backup → replace → configure)
//! - `restore`    — restore verified originals + remove engine configs
//! - `interfaces` — debug helper: list interfaces found in a binary
//!
//! The CLI is a thin wrapper over library APIs so behavior stays identical
//! when embedded in the Drop client addon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::error::EngineError;
use crate::{dll, fetch, patch, PatchPlan};

#[derive(Parser)]
#[command(
    name = "gse-engine",
    version,
    about = "Goldberg emulator patcher for drop-gse"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Patch a game directory with a fetched emulator distribution.
    Patch {
        /// Installed game directory.
        #[arg(long)]
        game_dir: PathBuf,
        /// Steam AppID to pin in steam_appid.txt.
        #[arg(long)]
        app_id: u32,
        /// Mesh peer addresses for custom_broadcasts.txt (repeatable).
        #[arg(long = "broadcast")]
        broadcasts: Vec<String>,
        /// Emulator listen port override (default 47584).
        #[arg(long)]
        listen_port: Option<u16>,
        /// Release tag of the emulator distribution to fetch.
        #[arg(long)]
        tag: String,
        /// Asset filename inside that release.
        #[arg(long)]
        asset: String,
        /// Expected SHA-256 of the asset (hex). Download is aborted on mismatch.
        #[arg(long)]
        digest: String,
        /// Cache directory for fetched archives [default: system cache dir].
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Restore original binaries and remove engine-written config files.
    Restore {
        /// Installed game directory.
        #[arg(long)]
        game_dir: PathBuf,
    },
    /// List Steam interface names embedded in a binary (debug helper).
    Interfaces {
        /// Path to the steam_api binary to scan.
        dll: PathBuf,
    },
}

fn default_cache_dir() -> PathBuf {
    if let Some(dirs) = dirs::cache_dir() {
        dirs.join("drop-gse")
    } else {
        std::env::temp_dir().join("drop-gse-cache")
    }
}

/// Blocking download used by the CLI (the library API is async; the CLI wraps
/// it in a current-thread runtime).
async fn download(url: String, dest: PathBuf) -> Result<(), EngineError> {
    let response = reqwest::get(&url)
        .await
        .map_err(|e| EngineError::Fetch(format!("{url}: {e}")))?;
    if !response.status().is_success() {
        return Err(EngineError::Fetch(format!(
            "{url}: HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| EngineError::Fetch(format!("{url}: {e}")))?;
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(EngineError::from)?;
    Ok(())
}

pub fn run(cli: Cli) -> Result<String, EngineError> {
    match cli.command {
        Command::Interfaces { dll: path } => {
            let found = crate::interfaces::extract_interfaces(&path)?;
            Ok(found.join("\n"))
        }
        Command::Restore { game_dir } => {
            dll::restore_originals(&game_dir, dll::TARGET_BINARIES)?;
            crate::config::remove_steam_settings(&game_dir)?;
            Ok(format!("restored originals in {}", game_dir.display()))
        }
        Command::Patch {
            game_dir,
            app_id,
            broadcasts,
            listen_port,
            tag,
            asset,
            digest,
            cache_dir,
        } => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| EngineError::Fetch(format!("runtime: {e}")))?;

            let spec = fetch::ReleaseSpec {
                tag,
                asset_name: asset,
                sha256: digest.to_lowercase(),
            };
            let cache = cache_dir.unwrap_or_else(default_cache_dir);
            let archive = rt.block_on(fetch::ensure_cached(&cache, &spec, download))?;

            // Extract only the payload binaries we need. Archive format is
            // inferred from the asset name (.7z vs .tar.bz2), matching the
            // gbe_fork release assets.
            let extract_root = cache.join(format!("extracted-{}", spec.sha256));
            extract_archive(&archive, &extract_root, &spec.asset_name)?;

            // Map target names → extracted payload paths (release/regular/<arch>).
            let arch = target_arch_folder();
            let wanted: Vec<&str> = dll::TARGET_BINARIES.to_vec();
            let mut payloads = BTreeMap::new();
            for lib in &wanted {
                let p = fetch::emulator_payload_paths(&extract_root, arch, &[lib])
                    .pop()
                    .expect("one path per lib");
                if p.exists() {
                    payloads.insert(lib.to_string(), p);
                }
            }

            let plan = PatchPlan {
                flavor: crate::EmulatorFlavor::GbeFork,
                app_id,
                broadcast_peers: broadcasts,
                listen_port,
            };
            let report = patch::patch(&game_dir, &plan, &payloads)?;
            Ok(serde_json::to_string_pretty(&report)?)
        }
    }
}

/// Host architecture → gbe_fork archive folder name.
fn target_arch_folder() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "x86") {
        "x32"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    }
}

/// Extract the downloaded release archive into `dest`, dispatching on format.
pub fn extract_archive(archive: &Path, dest: &Path, asset_name: &str) -> Result<(), EngineError> {
    std::fs::create_dir_all(dest).map_err(EngineError::from)?;
    let lower = asset_name.to_lowercase();
    if lower.ends_with(".tar.bz2") {
        let file = std::fs::File::open(archive).map_err(EngineError::from)?;
        let decompressed = bzip2::read::MultiBzDecoder::new(std::io::BufReader::new(file));
        let mut tar = tar::Archive::new(decompressed);
        tar.unpack(dest)
            .map_err(|e| EngineError::Archive(e.to_string()))?;
        Ok(())
    } else if lower.ends_with(".7z") {
        sevenz_rust2::decompress_file(
            archive
                .to_str()
                .ok_or_else(|| EngineError::Archive("non-utf8 archive path".into()))?,
            dest.to_str()
                .ok_or_else(|| EngineError::Archive("non-utf8 dest path".into()))?,
        )
        .map_err(|e| EngineError::Archive(e.to_string()))
    } else {
        Err(EngineError::Archive(format!(
            "unsupported archive format: {asset_name}"
        )))
    }
}
