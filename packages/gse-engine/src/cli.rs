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

/// Download used by the CLI (the library API is async; the CLI wraps it in a
/// current-thread runtime). Streams the body to disk — release archives are
/// tens of MB and must not be buffered whole in memory — with explicit
/// connect/request timeouts so a stalled server cannot hang the process, and
/// writes to a UNIQUE staging file that is atomically renamed into place so a
/// crash mid-download never leaves a partial artifact at the final path (and
/// concurrent invocations cannot interleave or clobber each other's writes).
async fn download(url: String, dest: PathBuf) -> Result<(), EngineError> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(15 * 60))
        .build()
        .map_err(|e| EngineError::Fetch(format!("client: {e}")))?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| EngineError::Fetch(format!("{url}: {e}")))?;
    if !response.status().is_success() {
        return Err(EngineError::Fetch(format!(
            "{url}: HTTP {}",
            response.status()
        )));
    }

    // Unique staging name: same directory (same filesystem → atomic rename),
    // pid + monotonic counter suffix. Removed on ANY failure so failed or
    // interrupted attempts don't accumulate partial artifacts.
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
    let staging = dest.with_extension(format!(
        "part-{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // Scope the file handle so it's closed before rename; flush once after
    // the loop (per-chunk flushing adds an await with no durability gain).
    let download_result: Result<(), EngineError> = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&staging)
            .await
            .map_err(EngineError::from)?;
        let mut stream = response;
        while let Some(chunk) = stream
            .chunk()
            .await
            .map_err(|e| EngineError::Fetch(format!("{url}: {e}")))?
        {
            file.write_all(&chunk).await.map_err(EngineError::from)?;
        }
        file.flush().await.map_err(EngineError::from)?;
        Ok(())
    }
    .await;

    if let Err(err) = download_result {
        let _ = tokio::fs::remove_file(&staging).await; // no partial leftovers
        return Err(err);
    }
    tokio::fs::rename(&staging, &dest)
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
            crate::patch::restore(&game_dir)?;
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

            let spec = fetch::ReleaseSpec::new(tag, asset, digest.to_lowercase())?;
            let cache = cache_dir.unwrap_or_else(default_cache_dir);
            let archive = rt.block_on(fetch::ensure_cached(&cache, &spec, download))?;

            // Extract only the payload binaries we need. Archive format is
            // inferred from the asset name (.7z vs .tar.bz2), matching the
            // gbe_fork release assets.
            let extract_root = cache.join(format!("extracted-{}", spec.sha256));
            extract_archive(&archive, &extract_root, &spec.asset_name)?;

            // Map target names → extracted payload paths via the fetch helper
            // (single source of truth for the archive layout). Resolution is
            // per-TARGET ABI; absent payloads are omitted — patch() itself
            // prevalidates and refuses to start if any located target would
            // lack one.
            let mut payloads = BTreeMap::new();
            for (lib, path) in fetch::emulator_payload_paths(
                &extract_root,
                target_abi_folder,
                dll::TARGET_BINARIES,
            ) {
                if path.exists() {
                    payloads.insert(lib, path);
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

/// Target binary → gbe_fork archive ABI folder. Resolution is by the
/// TARGET's ABI, not the host: `steam_api.dll` / `steamclient.dll` are 32-bit
/// and live under `x32` even when running on an x86_64 host; the `64`-suffixed
/// names live under `x64`. Returns `None` for binaries whose ABI has no
/// folder in gbe_fork archives (e.g. an aarch64 build), letting the caller
/// skip them explicitly instead of resolving a wrong path.
fn target_abi_folder(binary_name: &str) -> Option<&'static str> {
    match binary_name {
        "steam_api.dll" | "steamclient.dll" => Some("x32"),
        "steam_api64.dll" | "steamclient64.dll" => Some("x64"),
        // libsteam_api.so exists in both x32/x64 archive folders; pick by
        // host so Linux native patching grabs the matching bitness.
        "libsteam_api.so" => {
            if cfg!(target_arch = "x86") {
                Some("x32")
            } else {
                Some("x64")
            }
        }
        _ if cfg!(target_arch = "aarch64") => None, // no aarch64 folder upstream
        _ => None,
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
