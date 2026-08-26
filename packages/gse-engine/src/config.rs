//! Goldberg-family `steam_settings/` config generation.
//!
//! File names follow the layouts parsed by `settings_parser.cpp` in both
//! gbe_fork and gse_fork (see docs/research/COMPARATIVE_ANALYSIS.md). The
//! minimal LAN-multiplayer set is written: `steam_appid.txt`,
//! `custom_broadcasts.txt`, `steam_interfaces.txt`, and a minimal
//! `configs.main.ini`. Writes go through a staging directory with atomic
//! rename so an interrupted write never leaves a half-populated config.

use std::path::Path;

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

/// Atomic per-file write: content goes to a UNIQUE staging name (pid +
/// monotonic counter) so concurrent invocations can't interleave on a shared
/// `.tmp`, is flushed to disk before rename so an interrupted write never
/// leaves a truncated file at the final path.
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
        return Err(e.into());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write the full `steam_settings/` set into `game_dir`, atomically per file
/// and idempotently across runs.
///
/// Files are only rewritten when their content differs, so re-running a patch
/// does not churn mtimes or risk partial writes.
pub fn write_steam_settings(
    game_dir: &Path,
    settings: &SteamSettings,
) -> Result<Vec<std::path::PathBuf>, EngineError> {
    if !game_dir.is_dir() {
        return Err(EngineError::GameDirNotFound(game_dir.display().to_string()));
    }
    let dir = game_dir.join(STEAM_SETTINGS_DIR);
    std::fs::create_dir_all(&dir)?;

    let mut written = Vec::new();
    let entries: [(&str, Option<String>); 4] = [
        ("steam_appid.txt", Some(settings.render_steam_appid())),
        ("custom_broadcasts.txt", settings.render_custom_broadcasts()),
        ("steam_interfaces.txt", settings.render_steam_interfaces()),
        ("configs.main.ini", settings.render_configs_main()),
    ];

    for (name, content_opt) in entries {
        let Some(content) = content_opt else { continue };
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == content => {} // idempotent no-op
            _ => {
                atomic_write(&path, &content)?;
                written.push(path);
            }
        }
    }
    Ok(written)
}

/// Remove files written by [`write_steam_settings`] that we own. Only deletes
/// the four known filenames inside `steam_settings/`; never touches unknown
/// user content (achievements, items.json, etc.).
pub fn remove_steam_settings(game_dir: &Path) -> Result<(), EngineError> {
    let dir = game_dir.join(STEAM_SETTINGS_DIR);
    for name in [
        "steam_appid.txt",
        "custom_broadcasts.txt",
        "steam_interfaces.txt",
        "configs.main.ini",
    ] {
        let path = dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
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
}
