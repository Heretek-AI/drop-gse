//! Full patch orchestration: gate → locate → backup → replace → configure.
//!
//! Order matters and is deliberate:
//! 1. **Anticheat gate first** (fail-closed) — nothing is touched otherwise.
//! 2. **Extract interfaces from the ORIGINAL binary** — after replacement the
//!    emulator DLL no longer carries Valve's version strings
//!    (SteamRoll/GoldbergPatcher.cs scans before swapping for exactly this
//!    reason; required for Source/Unreal3 engine stability).
//! 3. Backup via the hash-manifested [`dll::backup_originals`] so re-runs can
//!    never clobber a verified original with patched content.
//! 4. Copy the fetched emulator binaries over the located targets.
//! 5. Write `steam_settings/` last, atomically.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::anticheat;
use crate::config::{self, SteamSettings};
use crate::dll;
use crate::error::EngineError;
use crate::PatchPlan;

/// Per-target outcome recorded in a [`PatchReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    /// Original backed up and replaced this run.
    Patched,
    /// Already replaced by a previous run; verified backup preserved.
    AlreadyPatched,
    /// Binary not present in this game layout.
    Skipped,
}

/// Structured result of a [`patch`] run.
#[derive(Debug, Serialize)]
pub struct PatchReport {
    pub app_id: u32,
    /// Relative target path → outcome.
    pub targets: std::collections::BTreeMap<String, TargetState>,
    /// Interface names extracted from the original binary.
    pub interfaces: Vec<String>,
    /// How many targets were replaced this run (0 on an idempotent re-run) —
    /// lets callers decide whether a relaunch is needed.
    pub replaced_count: usize,
}

/// Recursively locate files named in `names` under `root`, depth-limited.
/// Returns paths relative to `root`. The depth cap is the SAME constant the
/// anticheat gate uses (`anticheat::MAX_SCAN_DEPTH`) — a target we can reach
/// must always be inside scanned territory. Mirrors goldberg-patcher's
/// `find_file_path` but bounded (spec §3.1 scanner constraints).
fn locate_targets(root: &Path, names: &[&str], depth: usize) -> Result<Vec<PathBuf>, EngineError> {
    if depth > anticheat::MAX_SCAN_DEPTH {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    let entries = std::fs::read_dir(root)
        .map_err(|e| EngineError::ScanFailed(format!("{}: {e}", root.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| EngineError::ScanFailed(format!("{}: {e}", root.display())))?;
        let name = entry.file_name();
        let lower = name.to_string_lossy().to_lowercase();
        if names.contains(&lower.as_str()) {
            hits.push(entry.path());
        } else if entry
            .file_type()
            .map_err(|e| EngineError::ScanFailed(e.to_string()))?
            .is_dir()
        {
            hits.extend(locate_targets(&entry.path(), names, depth + 1)?);
        }
    }
    Ok(hits)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Execute the full patch pipeline against `game_dir`.
///
/// `emulator_payloads` maps each target binary name to the freshly-fetched
/// replacement file on disk (see [`crate::fetch`]). Targets absent from the
/// map are skipped rather than fatal — not every game ships every steam_api
/// variant.
pub fn patch(
    game_dir: &Path,
    plan: &PatchPlan,
    emulator_payloads: &std::collections::BTreeMap<String, PathBuf>,
) -> Result<PatchReport, EngineError> {
    // 1. Anticheat gate — fail closed; aborts everything when detection or
    //    the scan itself errors.
    if let Some(marker) = anticheat::detect(game_dir)? {
        return Err(EngineError::AntiCheatPresent(marker));
    }

    // 2. Locate targets relative to game_dir (recursive, depth-limited).
    let target_paths = locate_targets(game_dir, dll::TARGET_BINARIES, 0)?;
    if target_paths.is_empty() {
        return Err(EngineError::NoTargetsFound(game_dir.display().to_string()));
    }

    // 2b. Fail fast BEFORE any mutation: every locatable target must resolve
    //     to a payload, otherwise the swap loop would abort half-way through
    //     with a partially patched game and a mutated manifest.
    for path in &target_paths {
        let key = file_name(path).to_lowercase();
        if !emulator_payloads.contains_key(key.as_str()) {
            return Err(EngineError::Fetch(format!(
                "no emulator payload provided for {key} — refusing to start \
                 a partial patch"
            )));
        }
    }

    let mut targets = std::collections::BTreeMap::new();
    let mut interfaces: Vec<String> = Vec::new();

    for path in &target_paths {
        let rel = path
            .strip_prefix(game_dir)
            .map_err(|e| EngineError::ScanFailed(format!("relative path: {e}")))?
            .to_string_lossy()
            .to_string();

        // 3b-first: determine backup state BEFORE choosing the interface
        // source — on a re-run the live file is the emulator payload, so
        // interfaces must come from the `.orig` original.
        //
        // Pass the RELATIVE path (which may include subdirectories) so
        // backup, replacement and restore all address the same file; name-
        // only resolution would miss or mis-address nested targets. The
        // manifest keys and backup files are derived from this relative
        // name (`<rel>` → `<rel>.orig`).
        let outcome = dll::backup_originals(game_dir, &[rel.as_str()])?;
        let rel_backup = format!("{rel}.orig");

        // 3a. Interfaces must come from the ORIGINAL binary — prefer the
        //     verified `.orig` when present, else the not-yet-replaced file.
        //     Union across ALL targets: dual-ABI layouts (steam_api.dll +
        //     steam_api64.dll) can embed different interface families per
        //     binary; extracting only the first-sorted one silently drops
        //     hooks the other variant needs.
        {
            let orig_abs = game_dir.join(&rel_backup);
            let source = if orig_abs.exists() {
                orig_abs
            } else {
                path.clone()
            };
            interfaces.extend(crate::interfaces::extract_interfaces(&source)?);
        }

        match outcome.state(rel.as_str()) {
            Some(dll::BackupState::AlreadyPatched) => {
                targets.insert(rel.clone(), TargetState::AlreadyPatched);
            }
            Some(dll::BackupState::BackedUp) => {
                // 4. Replace with the fetched emulator payload. Lookup is by
                // LOWERCASED file name: locate_targets matches
                // case-insensitively but preserves on-disk casing, while the
                // payload map is keyed by canonical lowercase names — a
                // mixed-case target (STEAM_API64.DLL) must still resolve.
                let lookup_key = file_name(path).to_lowercase();
                let payload = emulator_payloads.get(lookup_key.as_str()).ok_or_else(|| {
                    EngineError::Fetch(format!("no emulator payload provided for {lookup_key}"))
                })?;
                std::fs::copy(payload, path)?;
                targets.insert(rel.clone(), TargetState::Patched);
            }
            None => {
                targets.insert(rel.clone(), TargetState::Skipped);
            }
        }
    }

    // 5. Write steam_settings (atomic per-file, idempotent). Interfaces are
    //    deduped+sorted across all targets; an EMPTY result is an error —
    //    extraction finding nothing recognizable means the original binary
    //    wasn't a steam_api payload, and an empty steam_interfaces.txt would
    //    silently disable every emulator hook.
    if interfaces.is_empty() {
        return Err(EngineError::NoTargetsFound(format!(
            "no Steam interface names found in any original binary under {}",
            game_dir.display()
        )));
    }
    interfaces.sort();
    interfaces.dedup();
    let settings = SteamSettings {
        app_id: plan.app_id,
        custom_broadcasts: plan.broadcast_peers.clone(),
        listen_port: plan.listen_port,
        interfaces,
    };
    config::write_steam_settings(game_dir, &settings)?;

    let replaced_count = targets
        .values()
        .filter(|s| **s == TargetState::Patched)
        .count();
    Ok(PatchReport {
        app_id: plan.app_id,
        targets,
        interfaces: settings.interfaces,
        replaced_count,
    })
}

/// Restore all tracked originals and remove engine-written config files.
///
/// Manifest entries are keyed by the on-disk relative path (any casing), so
/// restore enumerates the manifest itself rather than assuming canonical
/// lowercase names — a mixed-case target must not be silently skipped here.
pub fn restore(game_dir: &Path) -> Result<(), EngineError> {
    dll::restore_all_tracked(game_dir)?;
    config::remove_steam_settings(game_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a fake "original" steam_api64.dll carrying interface strings.
    fn fake_original(interfaces: &[&str]) -> Vec<u8> {
        let mut blob = vec![0x4du8, 0x5a]; // "MZ"
        for iface in interfaces {
            blob.extend_from_slice(iface.as_bytes());
            blob.push(0);
        }
        blob
    }

    fn write_payload(dir: &TempDir, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn full_patch_and_restore_roundtrip() {
        let game = TempDir::new().unwrap();
        let emu = TempDir::new().unwrap();
        let original = fake_original(&["SteamUser021", "SteamNetworkingSockets012"]);
        std::fs::write(game.path().join("steam_api64.dll"), &original).unwrap();

        let payload = write_payload(&emu, "steam_api64.dll", b"EMULATOR-BYTES");
        let mut payloads = std::collections::BTreeMap::new();
        payloads.insert("steam_api64.dll".to_string(), payload);

        let plan = PatchPlan {
            flavor: crate::EmulatorFlavor::GbeFork,
            app_id: 480,
            broadcast_peers: vec!["100.64.0.2".into()],
            listen_port: None,
        };

        let report = patch(game.path(), &plan, &payloads).unwrap();
        assert_eq!(
            report.targets.get("steam_api64.dll"),
            Some(&TargetState::Patched)
        );
        assert_eq!(report.app_id, 480);
        // Live file now holds emulator bytes; original safe as .orig.
        assert_eq!(
            std::fs::read(game.path().join("steam_api64.dll")).unwrap(),
            b"EMULATOR-BYTES"
        );
        assert_eq!(
            std::fs::read(game.path().join("steam_api64.dll.orig")).unwrap(),
            original
        );
        // Config set written from ORIGINAL-binary interfaces.
        let settings_dir = game.path().join("steam_settings");
        assert_eq!(
            std::fs::read_to_string(settings_dir.join("steam_appid.txt")).unwrap(),
            "480\n"
        );
        assert_eq!(
            std::fs::read_to_string(settings_dir.join("steam_interfaces.txt")).unwrap(),
            "SteamNetworkingSockets012\nSteamUser021\n",
            "interface list is sorted (SteamRoll parity)"
        );

        // Restore brings back the true original and removes our configs.
        restore(game.path()).unwrap();
        assert_eq!(
            std::fs::read(game.path().join("steam_api64.dll")).unwrap(),
            original
        );
        assert!(!game.path().join("steam_api64.dll.orig").exists());
        assert!(!settings_dir.join("steam_appid.txt").exists());
    }

    #[test]
    fn rerun_over_patched_game_preserves_original() {
        let game = TempDir::new().unwrap();
        let emu = TempDir::new().unwrap();
        let original = fake_original(&["SteamUser021"]);
        std::fs::write(game.path().join("steam_api64.dll"), &original).unwrap();

        let payload = write_payload(&emu, "steam_api64.dll", b"EMU");
        let mut payloads = std::collections::BTreeMap::new();
        payloads.insert("steam_api64.dll".to_string(), payload);

        let plan = PatchPlan {
            flavor: crate::EmulatorFlavor::GbeFork,
            app_id: 480,
            broadcast_peers: vec![],
            listen_port: None,
        };

        patch(game.path(), &plan, &payloads).unwrap();
        // Second run: detects already-patched, keeps the verified original.
        let report2 = patch(game.path(), &plan, &payloads).unwrap();
        assert_eq!(
            report2.targets.get("steam_api64.dll"),
            Some(&TargetState::AlreadyPatched)
        );
        assert_eq!(
            std::fs::read(game.path().join("steam_api64.dll.orig")).unwrap(),
            original,
            ".orig must survive re-runs"
        );
    }

    #[test]
    fn anticheat_detection_aborts_before_any_mutation() {
        let game = TempDir::new().unwrap();
        std::fs::write(game.path().join("steam_api64.dll"), b"ORIGINAL").unwrap();
        std::fs::write(game.path().join("EasyAntiCheat.exe"), b"x").unwrap();

        let emu = TempDir::new().unwrap();
        let payload = write_payload(&emu, "steam_api64.dll", b"EMU");
        let mut payloads = std::collections::BTreeMap::new();
        payloads.insert("steam_api64.dll".to_string(), payload);

        let plan = PatchPlan {
            flavor: crate::EmulatorFlavor::GbeFork,
            app_id: 480,
            broadcast_peers: vec![],
            listen_port: None,
        };

        let err = patch(game.path(), &plan, &payloads).unwrap_err();
        assert!(matches!(err, EngineError::AntiCheatPresent(_)));
        // Nothing happened: live file untouched, no backup, no settings dir.
        assert_eq!(
            std::fs::read(game.path().join("steam_api64.dll")).unwrap(),
            b"ORIGINAL"
        );
        assert!(!game.path().join("steam_api64.dll.orig").exists());
        assert!(!game.path().join("steam_settings").exists());
    }

    #[test]
    fn no_targets_is_error() {
        let game = TempDir::new().unwrap();
        let emu = TempDir::new().unwrap();
        let plan = PatchPlan {
            flavor: crate::EmulatorFlavor::GbeFork,
            app_id: 480,
            broadcast_peers: vec![],
            listen_port: None,
        };
        let payloads = std::collections::BTreeMap::new();
        let err = patch(game.path(), &plan, &payloads).unwrap_err();
        assert!(matches!(err, EngineError::NoTargetsFound(_)));
        drop(emu); // keep tempdir alive till end of test scope
    }
}
