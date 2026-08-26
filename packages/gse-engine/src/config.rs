//! Goldberg-family `steam_settings/` config generation.
//!
//! File names follow the layouts parsed by `settings_parser.cpp` in both
//! gbe_fork and gse_fork (see docs/research/COMPARATIVE_ANALYSIS.md). The
//! minimal LAN-multiplayer set is written: `steam_appid.txt`,
//! `custom_broadcasts.txt`, `steam_interfaces.txt`, and a minimal
//! `configs.main.ini`. Writes go through a staging directory with atomic
//! rename so an interrupted write never leaves a half-populated config.

use std::path::Path;

use crate::dll;
use crate::error::EngineError;
use crate::EmulatorFlavor;

/// Default emulator announce/listen port (gbe_fork `DEFAULT_PORT`).
pub const DEFAULT_LISTEN_PORT: u16 = 47584;

/// Directory name created inside `game_dir`.
pub const STEAM_SETTINGS_DIR: &str = "steam_settings";

/// Runtime configuration written into `<game_dir>/steam_settings/`.
#[derive(Debug, Clone)]
pub struct SteamSettings {
    pub app_id: u32,
    /// One address per line → `custom_broadcasts.txt`.
    pub custom_broadcasts: Vec<String>,
    /// Interface list → `steam_interfaces.txt` (extracted from the ORIGINAL
    /// steam_api binary before replacement).
    pub interfaces: Vec<String>,
    /// Emulator listen port; `None` keeps the default (no INI override).
    pub listen_port: Option<u16>,
}

impl SteamSettings {
    /// `None` when there are no peers — omitting the file entirely beats
    /// writing an empty one.
    pub fn render_custom_broadcasts(&self) -> Option<String> {
        if self.custom_broadcasts.is_empty() {
            None
        } else {
            Some(format!("{}\n", self.custom_broadcasts.join("\n")))
        }
    }

    pub fn render_steam_appid(&self) -> String {
        format!("{}\n", self.app_id)
    }

    /// `None` when no interfaces were found: an empty steam_interfaces.txt
    /// would silently disable every emulator hook. (patch() also errors on
    /// empty before reaching here; this guards direct library callers.)
    pub fn render_steam_interfaces(&self) -> Option<String> {
        if self.interfaces.is_empty() {
            None
        } else {
            Some(format!("{}\n", self.interfaces.join("\n")))
        }
    }

    /// Minimal `[main::connectivity]` INI — only emitted when a non-default
    /// port is requested; otherwise gbe_fork's defaults apply untouched.
    pub fn render_configs_main(&self) -> Option<String> {
        let port = self.listen_port?;
        Some(format!(
            "[main::connectivity]\n# drop-gse room configuration\nlisten_port={port}\n"
        ))
    }

    pub fn flavor(&self) -> EmulatorFlavor {
        // Placeholder until per-flavor INI templating lands (configs.app.ini).
        EmulatorFlavor::GbeFork
    }
}

/// Atomic durable per-file write.
///
/// Content goes to a UNIQUE staging name (pid + monotonic counter) so
/// concurrent invocations can't interleave on a shared `.tmp`; the content is
/// fsynced BEFORE the rename and the parent directory is fsynced after it
/// (best-effort — opening a directory fails on some platforms such as
/// Windows), so neither process death NOR power loss leaves a truncated file
/// at the final path. Failures carry the relevant path for triage, and a
/// failed rename cleans up the staged file.
fn atomic_write(path: &Path, contents: &str) -> Result<(), EngineError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?; // durable content before the rename makes it visible
        Ok(())
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(EngineError::Fetch(format!("{}: {e}", tmp.display())));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        // Rename failure would leak the fully-written staging file otherwise.
        let _ = std::fs::remove_file(&tmp);
        EngineError::Fetch(format!("{} -> {}: {e}", tmp.display(), path.display()))
    })?;
    // Make the RENAME itself durable; without this, power loss on some
    // filesystems can persist the directory entry without the data blocks.
    // Best-effort: opening a directory fails on Windows.
    if let Some(parent) = path.parent() {
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Snapshot a pre-existing settings file that WE did not author (no manifest
/// entry records it) before our first overwrite, as `<name>.orig` alongside
/// the binaries' backups. Goldberg users legitimately ship/edit
/// steam_appid.txt and configs.main.ini; teardown can then restore their
/// content instead of silently discarding it.
/// Manifest key prefix for config-file ownership entries. The single canonical
/// declaration lives in [`crate::dll`] (namespaced so binary restore never
/// mistakes them for `<path>` → `<path>.orig` backup pairs); this helper just
/// wraps the prefix.
fn config_key(name: &str) -> String {
    format!("{}{name}", crate::dll::CONFIG_KEY_PREFIX)
}

/// Snapshot a pre-existing settings file that WE did not author before our
/// first overwrite, as `<name>.orig` next to it. Goldberg users legitimately
/// ship/edit steam_appid.txt and configs.main.ini; teardown restores their
/// content instead of silently discarding it.
fn snapshot_user_file_if_untracked(
    dir: &Path,
    name: &str,
    manifest: &mut dll::Manifest,
    manifest_dirty: &mut bool,
) -> Result<(), EngineError> {
    let key = config_key(name);
    let target = dir.join(name);
    if !target.exists() || manifest.entries.contains_key(&key) {
        return Ok(()); // nothing to snapshot, or already ours
    }
    let backup = target.with_file_name(format!(
        "{}.orig",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    if !backup.exists() {
        std::fs::copy(&target, &backup)?;
    }
    // Record the digest of whatever snapshot is on disk — including a
    // leftover from an earlier run whose manifest was lost. Without this the
    // recorded digest would describe OUR managed content and teardown would
    // treat the genuine user snapshot as tampered and delete it.
    let digest = dll::sha256_hex_of(&backup)?;
    manifest.entries.insert(key, digest);
    *manifest_dirty = true;
    Ok(())
}

/// Restore a snapshotted user file at `<name>.orig` to its live path if the
/// recorded digest matches. A mismatch is treated as tampering (the snapshot
/// is removed; nothing resurrected). Any read/hash error propagates rather
/// than silently dropping the snapshot.
fn restore_snapshot_if_present(
    dir: &Path,
    name: &str,
    manifest: &dll::Manifest,
) -> Result<(), EngineError> {
    let key = config_key(name);
    let Some(recorded) = manifest.entries.get(&key) else {
        return Ok(()); // never snapshotted — nothing to restore
    };
    let path = dir.join(name);
    let backup = path.with_file_name(format!(
        "{}.orig",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if !backup.exists() {
        return Ok(());
    }
    let digest = dll::sha256_hex_of(&backup)?;
    if digest == *recorded {
        std::fs::rename(&backup, &path)?; // restore user content
    } else {
        // Snapshot tampered: refuse to resurrect possibly-wrong content.
        std::fs::remove_file(&backup)?;
    }
    Ok(())
}

/// Write the full `steam_settings/` set into `game_dir`, atomically per file
/// and idempotently across runs.
///
/// Files are only rewritten when their content differs, so re-running a patch
/// does not churn mtimes or risk partial writes. A file whose setting became
/// empty/default this run is REMOVED if we authored it (manifest-tracked) —
/// leaving stale broadcasts/port overrides active across a downgrade would
/// be worse than absence. Pre-existing files we did NOT author are snapshotted
/// to `<name>.orig` before the first overwrite and left alone at teardown of
/// fields we never claimed.
pub fn write_steam_settings(
    game_dir: &Path,
    settings: &SteamSettings,
) -> Result<Vec<std::path::PathBuf>, EngineError> {
    if !game_dir.is_dir() {
        return Err(EngineError::GameDirNotFound(game_dir.display().to_string()));
    }
    let dir = game_dir.join(STEAM_SETTINGS_DIR);
    std::fs::create_dir_all(&dir)?;

    // The patch manifest doubles as ownership ledger for config files so
    // snapshot/removal decisions distinguish "ours" from "user-authored".
    let mut manifest = dll::load_manifest(game_dir)?;
    let mut manifest_dirty = false;

    let mut written = Vec::new();
    let entries: [(&str, Option<String>); 4] = [
        ("steam_appid.txt", Some(settings.render_steam_appid())),
        ("custom_broadcasts.txt", settings.render_custom_broadcasts()),
        ("steam_interfaces.txt", settings.render_steam_interfaces()),
        ("configs.main.ini", settings.render_configs_main()),
    ];

    for (name, content_opt) in entries {
        let path = dir.join(name);
        match content_opt {
            None => {
                // Setting reverted to empty/default: drop OUR previous output
                // rather than leaving stale broadcasts/port overrides active.
                // A snapshot means the user authored this file before we
                // overwrote it — RESTORE their content instead of deleting
                // it (the downgrade path must match the teardown guarantee).
                let key = config_key(name);
                if path.exists() && manifest.entries.contains_key(&key) {
                    std::fs::remove_file(&path)?;
                    restore_snapshot_if_present(&dir, name, &manifest)?;
                    manifest.entries.remove(&key);
                    manifest_dirty = true;
                }
            }
            Some(content) => {
                snapshot_user_file_if_untracked(&dir, name, &mut manifest, &mut manifest_dirty)?;
                match std::fs::read_to_string(&path) {
                    Ok(existing) if existing == content => {} // idempotent no-op
                    _ => {
                        atomic_write(&path, &content)?;
                        // We own this file from here on (snapshot above ran).
                        let key = config_key(name);
                        if !manifest.entries.contains_key(&key) {
                            let digest = dll::sha256_hex_of(&path)?;
                            manifest.entries.insert(key.clone(), digest);
                            manifest_dirty = true;
                        }
                        written.push(path);
                    }
                }
            }
        }
    }

    // Reuse the binary-manifest persistence path (same file, shared ledger).
    // Config digests here track that we OWN the entry, not exact content —
    // content equality is checked directly above before overwriting.
    if manifest_dirty {
        dll::save_manifest(game_dir, &manifest)?;
    }
    Ok(written)
}

/// Remove files written by [`write_steam_settings`] that we own and restore
/// snapshotted user-authored content. Only the four known filenames inside
/// `steam_settings/` are considered; unknown user content (achievements,
/// items.json, …) is never touched.
///
/// For each managed name:
/// - We authored it AND a user snapshot exists (`<name>.orig`, recorded via
///   the `config:` manifest namespace) → restore the snapshot (digest-verified
///   against the recorded pre-overwrite content) instead of deleting.
/// - We authored it, no snapshot → delete (we created the file outright).
/// - No manifest entry → not ours; leave untouched.
pub fn remove_steam_settings(game_dir: &Path) -> Result<(), EngineError> {
    let dir = game_dir.join(STEAM_SETTINGS_DIR);
    let mut manifest = dll::load_manifest(game_dir)?;
    let mut dirty = false;
    for name in [
        "steam_appid.txt",
        "custom_broadcasts.txt",
        "steam_interfaces.txt",
        "configs.main.ini",
    ] {
        let key = config_key(name);
        if !manifest.entries.contains_key(&key) {
            continue; // never authored/snapshotted by us — leave untouched
        }
        let path = dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        // Restore the user's pre-patch file if we snapshotted one.
        restore_snapshot_if_present(&dir, name, &manifest)?;
        manifest.entries.remove(&key);
        dirty = true;
    }
    if dirty {
        dll::save_manifest(game_dir, &manifest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> tempfile::TempDir {
        tempfile::TempDir::with_prefix(format!("gse-cfgtest-{tag}-")).expect("unique temp dir")
    }

    fn sample_settings() -> SteamSettings {
        SteamSettings {
            app_id: 480,
            custom_broadcasts: vec!["100.64.0.2".into(), "100.64.0.3".into()],
            interfaces: vec!["SteamUser021".into(), "SteamNetworking005".into()],
            listen_port: None,
        }
    }

    #[test]
    fn writes_full_set() {
        let dir = tmpdir("write");
        write_steam_settings(dir.path(), &sample_settings()).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join(STEAM_SETTINGS_DIR).join("steam_appid.txt"))
                .unwrap(),
            "480\n"
        );
        assert_eq!(
            fs::read_to_string(
                dir.path()
                    .join(STEAM_SETTINGS_DIR)
                    .join("custom_broadcasts.txt")
            )
            .unwrap(),
            "100.64.0.2\n100.64.0.3\n"
        );
        assert_eq!(
            fs::read_to_string(
                dir.path()
                    .join(STEAM_SETTINGS_DIR)
                    .join("steam_interfaces.txt")
            )
            .unwrap(),
            "SteamUser021\nSteamNetworking005\n"
        );
        // Default port → no INI written at all.
        assert!(!dir
            .path()
            .join(STEAM_SETTINGS_DIR)
            .join("configs.main.ini")
            .exists());
    }

    #[test]
    fn nondefault_port_writes_ini() {
        let dir = tmpdir("port");
        let mut s = sample_settings();
        s.listen_port = Some(47585);
        write_steam_settings(dir.path(), &s).unwrap();
        let ini = fs::read_to_string(dir.path().join(STEAM_SETTINGS_DIR).join("configs.main.ini"))
            .unwrap();
        assert!(ini.contains("[main::connectivity]"));
        assert!(ini.contains("listen_port=47585"));
    }

    #[test]
    fn rewrite_is_idempotent() {
        let dir = tmpdir("idem");
        let s = sample_settings();
        let first = write_steam_settings(dir.path(), &s).unwrap();
        assert_eq!(first.len(), 3);
        let second = write_steam_settings(dir.path(), &s).unwrap();
        assert!(second.is_empty(), "identical content must not rewrite");
    }

    #[test]
    fn remove_only_touches_owned_files() {
        let dir = tmpdir("remove");
        write_steam_settings(dir.path(), &sample_settings()).unwrap();
        let keep = dir.path().join(STEAM_SETTINGS_DIR).join("items.json");
        fs::write(&keep, b"{}").unwrap();

        remove_steam_settings(dir.path()).unwrap();

        assert!(!dir
            .path()
            .join(STEAM_SETTINGS_DIR)
            .join("steam_appid.txt")
            .exists());
        assert!(keep.exists(), "user content must survive teardown");
    }

    #[test]
    fn missing_game_dir_is_error() {
        let tmp = tmpdir("missing");
        let missing = tmp.path().join("does-not-exist");
        assert!(matches!(
            write_steam_settings(&missing, &sample_settings()),
            Err(EngineError::GameDirNotFound(_))
        ));
    }

    #[test]
    fn user_authored_file_is_snapshotted_and_restored() {
        let dir = tmpdir("user-snap");
        let settings_dir = dir.path().join(STEAM_SETTINGS_DIR);
        fs::create_dir_all(&settings_dir).unwrap();
        // Game ships with a user-edited appid config.
        fs::write(settings_dir.join("steam_appid.txt"), b"1234\n").unwrap();

        let mut s = sample_settings();
        s.custom_broadcasts.clear(); // keep this test focused on appid
        write_steam_settings(dir.path(), &s).unwrap();

        // Overwritten live, but the user's content is snapshotted.
        assert_eq!(
            fs::read_to_string(settings_dir.join("steam_appid.txt")).unwrap(),
            "480\n"
        );
        assert_eq!(
            fs::read_to_string(settings_dir.join("steam_appid.txt.orig")).unwrap(),
            "1234\n"
        );

        // Teardown restores the user's file rather than deleting it.
        remove_steam_settings(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(settings_dir.join("steam_appid.txt")).unwrap(),
            "1234\n"
        );
        assert!(!settings_dir.join("steam_appid.txt.orig").exists());
    }

    #[test]
    fn downgrade_removes_stale_managed_file_but_not_user_file() {
        let dir = tmpdir("downgrade");
        // First run: broadcasts set → custom_broadcasts.txt written (owned).
        let mut s1 = sample_settings();
        s1.interfaces.clear(); // focus on broadcasts
        write_steam_settings(dir.path(), &s1).unwrap();
        assert!(dir
            .path()
            .join(STEAM_SETTINGS_DIR)
            .join("custom_broadcasts.txt")
            .exists());

        // Second run: field empty → our stale output must be removed.
        let mut s2 = sample_settings();
        s2.interfaces.clear();
        s2.custom_broadcasts.clear();
        write_steam_settings(dir.path(), &s2).unwrap();
        assert!(
            !dir.path()
                .join(STEAM_SETTINGS_DIR)
                .join("custom_broadcasts.txt")
                .exists(),
            "stale managed broadcast file must not survive a downgrade"
        );
    }

    #[test]
    fn unauthored_empty_field_file_is_left_alone() {
        let dir = tmpdir("unauthored");
        let settings_dir = dir.path().join(STEAM_SETTINGS_DIR);
        fs::create_dir_all(&settings_dir).unwrap();
        // A user-authored custom_broadcasts.txt that we never wrote.
        fs::write(settings_dir.join("custom_broadcasts.txt"), b"10.0.0.1\n").unwrap();

        let mut s = sample_settings();
        s.interfaces.clear();
        s.custom_broadcasts.clear(); // would render None
        write_steam_settings(dir.path(), &s).unwrap();

        assert_eq!(
            fs::read_to_string(settings_dir.join("custom_broadcasts.txt")).unwrap(),
            "10.0.0.1\n",
            "files we never authored must survive even on downgrade"
        );
    }

    #[test]
    fn downgrade_restores_snapshot_not_deletes() {
        let dir = tmpdir("downgrade-restore");
        let settings_dir = dir.path().join(STEAM_SETTINGS_DIR);
        fs::create_dir_all(&settings_dir).unwrap();
        // User authored broadcasts file.
        fs::write(settings_dir.join("custom_broadcasts.txt"), b"9.9.9.9\n").unwrap();

        let mut s1 = sample_settings();
        s1.interfaces.clear();
        write_steam_settings(dir.path(), &s1).unwrap(); // snapshot + overwrite
        assert_eq!(
            fs::read_to_string(settings_dir.join("custom_broadcasts.txt")).unwrap(),
            "100.64.0.2\n100.64.0.3\n"
        );
        assert!(settings_dir.join("custom_broadcasts.txt.orig").exists());

        // Downgrade: field empty, our file tracked, snapshot present →
        // the snapshot must be restored (the user content is preserved),
        // NOT deleted.
        let mut s2 = sample_settings();
        s2.interfaces.clear();
        s2.custom_broadcasts.clear();
        write_steam_settings(dir.path(), &s2).unwrap();

        assert_eq!(
            fs::read_to_string(settings_dir.join("custom_broadcasts.txt")).unwrap(),
            "9.9.9.9\n",
            "downgrade must restore the user snapshot, not delete it"
        );
        assert!(!settings_dir.join("custom_broadcasts.txt.orig").exists());
    }

    #[test]
    fn leftover_snapshot_is_recorded_in_manifest() {
        // Simulate an existing `.orig` from a prior run whose manifest was
        // lost: we must record the digest of the leftover (not of our new
        // managed content), otherwise teardown treats the user file as tampered
        // and drops it.
        let dir = tmpdir("leftover");
        let settings_dir = dir.path().join(STEAM_SETTINGS_DIR);
        fs::create_dir_all(&settings_dir).unwrap();
        // Live user file + a pre-existing snapshot with the same content.
        fs::write(settings_dir.join("steam_appid.txt"), b"9999\n").unwrap();
        fs::write(settings_dir.join("steam_appid.txt.orig"), b"9999\n").unwrap();

        let mut s = sample_settings();
        s.custom_broadcasts.clear();
        s.interfaces.clear();
        write_steam_settings(dir.path(), &s).unwrap();

        // Teardown restores the (genuine) user content via digest match.
        remove_steam_settings(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(settings_dir.join("steam_appid.txt")).unwrap(),
            "9999\n"
        );
    }
}
