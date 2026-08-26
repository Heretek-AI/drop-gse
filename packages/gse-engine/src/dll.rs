//! DLL / shared-library backup, swap and restore.
//!
//! Backup convention: original binaries are copied aside with a `.orig`
//! suffix before any replacement, and a per-directory manifest
//! (`.drop-gse-manifest.json`) records the SHA-256 digest of each original.
//! Reuse, restoration and deletion of a backup are only performed after its
//! recorded digest matches — an unverified or stale backup is never trusted.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::EngineError;

/// steam_api binaries we may replace, per platform.
pub const TARGET_BINARIES: &[&str] = &["steam_api.dll", "steam_api64.dll", "libsteam_api.so"];

/// Manifest file written next to the `.orig` backups.
pub const MANIFEST_FILE: &str = ".drop-gse-manifest.json";

#[derive(Debug, Serialize, Deserialize, Default)]
struct Manifest {
    /// Relative binary name → SHA-256 of the backed-up `.orig` contents.
    entries: std::collections::BTreeMap<String, String>,
}

/// Lowercase hex encoding of the SHA-256 digest. Written manually because
/// sha2 0.11's digest output (`Array<u8, _>`) no longer implements
/// `LowerHex` (it moved from `generic-array` to `hybrid-array`).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_of(path: &Path) -> Result<String, EngineError> {
    let data = std::fs::read(path)?;
    Ok(to_hex(&Sha256::digest(&data)))
}

#[cfg(test)]
fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

fn manifest_path(game_dir: &Path) -> std::path::PathBuf {
    game_dir.join(MANIFEST_FILE)
}

fn load_manifest(game_dir: &Path) -> Result<Manifest, EngineError> {
    let path = manifest_path(game_dir);
    if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        serde_json::from_str(&raw)
            .map_err(|e| EngineError::ScanFailed(format!("corrupt manifest: {e}")))
    } else {
        Ok(Manifest::default())
    }
}

fn save_manifest(game_dir: &Path, manifest: &Manifest) -> Result<(), EngineError> {
    let raw = serde_json::to_string_pretty(manifest)?;
    std::fs::write(manifest_path(game_dir), raw)?;
    Ok(())
}

/// Ensure `game_dir` exists and is accessible, failing closed otherwise.
fn ensure_game_dir(game_dir: &Path) -> Result<(), EngineError> {
    match std::fs::metadata(game_dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(EngineError::GameDirNotFound(game_dir.display().to_string())),
        Err(e) => Err(EngineError::GameDirNotFound(format!(
            "{}: {e}",
            game_dir.display()
        ))),
    }
}

/// Outcome of backing up a single binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupState {
    /// A fresh backup of the original was created (or a stale one replaced).
    BackedUp,
    /// The binary is already patched relative to its recorded original and
    /// the manifest-verified backup was left untouched — re-running the
    /// patcher must never overwrite the good backup with patched content.
    AlreadyPatched,
}

/// Result of [`backup_originals`] per binary name.
#[derive(Debug, Default)]
pub struct BackupOutcome {
    pub states: std::collections::BTreeMap<String, BackupState>,
}

impl BackupOutcome {
    pub fn state(&self, name: &str) -> Option<BackupState> {
        self.states.get(name).copied()
    }
}

/// Back up `binaries` inside `game_dir` as `<name>.orig`.
///
/// Semantics:
/// - No manifest entry → back up the live file.
/// - Manifest entry exists and the existing `.orig`'s digest matches it:
///   - live file also matches → nothing to do (`BackedUp`, unchanged).
///   - live file differs → the game is already patched; keep the verified
///     original backup intact (`AlreadyPatched`). **Never** copy the patched
///     binary over the good backup.
/// - Manifest entry exists but the `.orig` is corrupt/stale (digest mismatch)
///   → replace it with the live file.
pub fn backup_originals(game_dir: &Path, binaries: &[&str]) -> Result<BackupOutcome, EngineError> {
    ensure_game_dir(game_dir)?;
    let mut manifest = load_manifest(game_dir)?;
    let mut outcome = BackupOutcome::default();
    for name in binaries {
        let src = game_dir.join(name);
        if !std::fs::metadata(&src)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            continue; // target not present in this layout
        }
        let dst = game_dir.join(format!("{name}.orig"));
        let live_digest = sha256_of(&src)?;

        match manifest.entries.get(*name) {
            Some(recorded) => {
                let backup_digest = sha256_of(&dst);
                match backup_digest {
                    // Verified backup + unmodified live file: idempotent no-op.
                    Ok(d) if d == *recorded && live_digest == *recorded => {
                        outcome
                            .states
                            .insert(name.to_string(), BackupState::BackedUp);
                    }
                    // Verified backup + modified live file: already patched —
                    // preserve the original backup.
                    Ok(d) if d == *recorded => {
                        outcome
                            .states
                            .insert(name.to_string(), BackupState::AlreadyPatched);
                    }
                    // Corrupt/stale backup: replace it with the live file.
                    _ => {
                        std::fs::copy(&src, &dst)?;
                        let backup_digest = sha256_of(&dst)?;
                        manifest.entries.insert(name.to_string(), backup_digest);
                        outcome
                            .states
                            .insert(name.to_string(), BackupState::BackedUp);
                    }
                }
            }
            None => {
                std::fs::copy(&src, &dst)?;
                let backup_digest = sha256_of(&dst)?;
                manifest.entries.insert(name.to_string(), backup_digest);
                outcome
                    .states
                    .insert(name.to_string(), BackupState::BackedUp);
            }
        }
    }
    save_manifest(game_dir, &manifest)?;
    Ok(outcome)
}

/// Restore `*.orig` backups created by [`backup_originals`] and remove them.
///
/// Phases, ordered so an interruption at any point is recoverable:
/// 1. **Verify** every tracked backup's digest (nothing is modified).
/// 2. **Copy** all verified backups over their destinations. A failure here
///    leaves every `.orig` intact and the manifest unchanged.
/// 3. **Clean up**: delete each `.orig` and persist the manifest immediately
///    after each successful removal, so an interrupted cleanup resumes with
///    only the remaining entries and never requires already-deleted backups.
pub fn restore_originals(game_dir: &Path, binaries: &[&str]) -> Result<(), EngineError> {
    ensure_game_dir(game_dir)?;
    let mut manifest = load_manifest(game_dir)?;

    // Phase 1: verify all tracked backups before touching anything.
    for name in binaries {
        let Some(recorded) = manifest.entries.get(*name) else {
            continue; // never backed up by us in this directory
        };
        let src = game_dir.join(format!("{name}.orig"));
        let found = sha256_of(&src).map_err(|_| EngineError::ManifestMismatch {
            path: name.to_string(),
            expected: recorded.clone(),
            found: "<backup missing>".to_string(),
        })?;
        if found != *recorded {
            return Err(EngineError::ManifestMismatch {
                path: name.to_string(),
                expected: recorded.clone(),
                found,
            });
        }
    }

    // Phase 2: copy everything back. Backups are NOT removed yet.
    for name in binaries {
        if !manifest.entries.contains_key(*name) {
            continue;
        }
        let dst = game_dir.join(name);
        let src = game_dir.join(format!("{name}.orig"));
        std::fs::copy(&src, &dst)?;
    }

    // Phase 3: durable cleanup — remove one backup at a time and persist
    // progress after each success.
    for name in binaries {
        if !manifest.entries.contains_key(*name) {
            continue;
        }
        let src = game_dir.join(format!("{name}.orig"));
        std::fs::remove_file(&src)?;
        manifest.entries.remove(*name);
        save_manifest(game_dir, &manifest)?;
    }
    Ok(())
}

/// Restore EVERY entry recorded in the manifest, whatever its on-disk casing
/// or nesting. `restore_originals` filters by an explicit name list (canonical
/// lowercase), which would silently skip a manifest entry like
/// `bin/STEAM_API64.DLL`; this variant trusts the manifest itself — the same
/// keys backup wrote — so nothing patched is ever left unrestored.
pub fn restore_all_tracked(game_dir: &Path) -> Result<(), EngineError> {
    let manifest = load_manifest(game_dir)?;
    let tracked: Vec<&str> = manifest.entries.keys().map(String::as_str).collect();
    restore_originals(game_dir, &tracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gse-dlltest-{tag}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn backup_restore_roundtrip() {
        let dir = tmpdir("roundtrip");
        fs::write(dir.join("steam_api64.dll"), b"original-bytes").unwrap();

        let outcome = backup_originals(&dir, TARGET_BINARIES).unwrap();
        assert_eq!(
            outcome.state("steam_api64.dll"),
            Some(BackupState::BackedUp)
        );
        assert!(dir.join("steam_api64.dll.orig").exists());

        // Simulate patching.
        fs::write(dir.join("steam_api64.dll"), b"emulator-bytes").unwrap();

        restore_originals(&dir, TARGET_BINARIES).unwrap();
        assert_eq!(
            fs::read(dir.join("steam_api64.dll")).unwrap(),
            b"original-bytes"
        );
        assert!(!dir.join("steam_api64.dll.orig").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_run_preserves_backup_of_patched_binary() {
        let dir = tmpdir("rerun");
        fs::write(dir.join("steam_api64.dll"), b"original").unwrap();
        backup_originals(&dir, &["steam_api64.dll"]).unwrap();

        // Simulate a successful patch.
        fs::write(dir.join("steam_api64.dll"), b"patched").unwrap();

        // Re-running the backup must detect "already patched" and NOT clobber
        // the verified original backup with patched content.
        let outcome = backup_originals(&dir, &["steam_api64.dll"]).unwrap();
        assert_eq!(
            outcome.state("steam_api64.dll"),
            Some(BackupState::AlreadyPatched)
        );
        assert_eq!(
            fs::read(dir.join("steam_api64.dll.orig")).unwrap(),
            b"original",
            "verified backup must survive a re-run over a patched binary"
        );

        // And restore still recovers the true original.
        restore_originals(&dir, &["steam_api64.dll"]).unwrap();
        assert_eq!(fs::read(dir.join("steam_api64.dll")).unwrap(), b"original");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn idempotent_backup_when_unpatched() {
        let dir = tmpdir("idem");
        fs::write(dir.join("steam_api64.dll"), b"same").unwrap();
        let first = backup_originals(&dir, &["steam_api64.dll"]).unwrap();
        assert_eq!(first.state("steam_api64.dll"), Some(BackupState::BackedUp));
        let second = backup_originals(&dir, &["steam_api64.dll"]).unwrap();
        assert_eq!(
            second.state("steam_api64.dll"),
            Some(BackupState::BackedUp),
            "unmodified re-run stays in BackedUp state (no-op)"
        );
        assert_eq!(fs::read(dir.join("steam_api64.dll.orig")).unwrap(), b"same");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_backup_is_recreated_not_reused() {
        let dir = tmpdir("stale");
        fs::write(dir.join("steam_api.dll"), b"current-original").unwrap();
        // Plant a stale backup + stale manifest from an "older" run.
        fs::write(dir.join("steam_api.dll.orig"), b"ancient").unwrap();

        backup_originals(&dir, &["steam_api.dll"]).unwrap();
        assert_eq!(
            fs::read(dir.join("steam_api.dll.orig")).unwrap(),
            b"current-original",
            "stale backup must be replaced with the live original"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tampered_backup_aborts_restore() {
        let dir = tmpdir("tamper");
        fs::write(dir.join("steam_api64.dll"), b"original").unwrap();
        backup_originals(&dir, &["steam_api64.dll"]).unwrap();
        fs::write(dir.join("steam_api64.dll"), b"patched").unwrap();
        // Corrupt the backup after backing up.
        fs::write(dir.join("steam_api64.dll.orig"), b"tampered").unwrap();

        let err = restore_originals(&dir, &["steam_api64.dll"]).unwrap_err();
        assert!(matches!(err, EngineError::ManifestMismatch { .. }));
        // Live file untouched by the aborted restore.
        assert_eq!(fs::read(dir.join("steam_api64.dll")).unwrap(), b"patched");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interrupted_cleanup_is_recoverable() {
        let dir = tmpdir("partial-restore");

        // Two tracked binaries; simulate a crash after only steam_api.dll's
        // cleanup completed (its .orig is gone AND its manifest entry was
        // persisted as removed), while steam_api64.dll remains pending with
        // its backup intact.
        fs::write(dir.join("steam_api.dll"), b"patched-a").unwrap();
        fs::write(dir.join("steam_api64.dll"), b"patched-b").unwrap();
        fs::write(dir.join("steam_api64.dll.orig"), b"original-b").unwrap();
        let mut manifest = Manifest::default();
        manifest
            .entries
            .insert("steam_api64.dll".to_string(), sha256_hex(b"original-b"));
        save_manifest(&dir, &manifest).unwrap();

        // A retry of restore completes for the remaining binary.
        restore_originals(&dir, &["steam_api.dll", "steam_api64.dll"]).unwrap();
        assert_eq!(
            fs::read(dir.join("steam_api64.dll")).unwrap(),
            b"original-b",
            "remaining pending backup must be restored on retry"
        );
        assert!(!dir.join("steam_api64.dll.orig").exists());
        // steam_api.dll had no pending entry — untouched.
        assert_eq!(fs::read(dir.join("steam_api.dll")).unwrap(), b"patched-a");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_copy_keeps_all_backups_for_retry() {
        let dir = tmpdir("copy-fail");
        fs::write(dir.join("steam_api.dll"), b"orig-a").unwrap();
        fs::write(dir.join("steam_api64.dll"), b"orig-b").unwrap();
        backup_originals(&dir, &["steam_api.dll", "steam_api64.dll"]).unwrap();

        // Simulate patching both, then make one destination a directory so
        // the copy phase fails mid-way.
        fs::write(dir.join("steam_api.dll"), b"new-a").unwrap();
        fs::write(dir.join("steam_api64.dll"), b"new-b").unwrap();
        fs::remove_file(dir.join("steam_api.dll")).unwrap();
        fs::create_dir_all(dir.join("steam_api.dll")).unwrap();

        let result = restore_originals(&dir, &["steam_api.dll", "steam_api64.dll"]);
        assert!(result.is_err());

        // Both backups must still exist — nothing was cleaned up.
        assert!(dir.join("steam_api.dll.orig").exists());
        assert!(dir.join("steam_api64.dll.orig").exists());

        // Remove the obstruction and retry: full restore succeeds.
        fs::remove_dir_all(dir.join("steam_api.dll")).unwrap();
        restore_originals(&dir, &["steam_api.dll", "steam_api64.dll"]).unwrap();
        assert_eq!(fs::read(dir.join("steam_api.dll")).unwrap(), b"orig-a");
        assert_eq!(fs::read(dir.join("steam_api64.dll")).unwrap(), b"orig-b");
        assert!(!dir.join("steam_api.dll.orig").exists());
        assert!(!dir.join("steam_api64.dll.orig").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inaccessible_directory_fails_closed() {
        let missing = std::env::temp_dir().join("gse-dlltest-missing-dir");
        assert!(matches!(
            backup_originals(&missing, TARGET_BINARIES),
            Err(EngineError::GameDirNotFound(_))
        ));
        assert!(matches!(
            restore_originals(&missing, TARGET_BINARIES),
            Err(EngineError::GameDirNotFound(_))
        ));
    }
}
