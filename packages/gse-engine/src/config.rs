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
use crate::interfaces;
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
    /// Extract interfaces from `original_dll` and build settings around them.
    pub fn from_original_dll(
        app_id: u32,
        original_dll: &Path,
        custom_broadcasts: Vec<String>,
    ) -> Result<Self, EngineError> {
        Ok(Self {
            app_id,
            interfaces: interfaces::extract_interfaces(original_dll)?,
            custom_broadcasts,
            listen_port: None,
        })
    }

    pub fn render_custom_broadcasts(&self) -> String {
        if self.custom_broadcasts.is_empty() {
            String::new()
        } else {
            let mut s = self.custom_broadcasts.join("\n");
            s.push('\n');
            s
        }
    }

    pub fn render_steam_appid(&self) -> String {
        format!("{}\n", self.app_id)
    }

    pub fn render_steam_interfaces(&self) -> String {
        if self.interfaces.is_empty() {
            String::new()
        } else {
            let mut s = self.interfaces.join("\n");
            s.push('\n');
            s
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

fn atomic_write(path: &Path, contents: &str) -> Result<(), EngineError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
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
        (
            "custom_broadcasts.txt",
            Some(settings.render_custom_broadcasts()),
        ),
        (
            "steam_interfaces.txt",
            Some(settings.render_steam_interfaces()),
        ),
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

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gse-cfgtest-{tag}"));
        fs::create_dir_all(&dir).unwrap();
        dir
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
        write_steam_settings(&dir, &sample_settings()).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join(STEAM_SETTINGS_DIR).join("steam_appid.txt")).unwrap(),
            "480\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join(STEAM_SETTINGS_DIR).join("custom_broadcasts.txt")).unwrap(),
            "100.64.0.2\n100.64.0.3\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join(STEAM_SETTINGS_DIR).join("steam_interfaces.txt")).unwrap(),
            "SteamUser021\nSteamNetworking005\n"
        );
        // Default port → no INI written at all.
        assert!(!dir
            .join(STEAM_SETTINGS_DIR)
            .join("configs.main.ini")
            .exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nondefault_port_writes_ini() {
        let dir = tmpdir("port");
        let mut s = sample_settings();
        s.listen_port = Some(47585);
        write_steam_settings(&dir, &s).unwrap();
        let ini =
            fs::read_to_string(dir.join(STEAM_SETTINGS_DIR).join("configs.main.ini")).unwrap();
        assert!(ini.contains("[main::connectivity]"));
        assert!(ini.contains("listen_port=47585"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewrite_is_idempotent() {
        let dir = tmpdir("idem");
        let s = sample_settings();
        let first = write_steam_settings(&dir, &s).unwrap();
        assert_eq!(first.len(), 3);
        let second = write_steam_settings(&dir, &s).unwrap();
        assert!(second.is_empty(), "identical content must not rewrite");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_only_touches_owned_files() {
        let dir = tmpdir("remove");
        write_steam_settings(&dir, &sample_settings()).unwrap();
        let keep = dir.join(STEAM_SETTINGS_DIR).join("items.json");
        fs::write(&keep, b"{}").unwrap();

        remove_steam_settings(&dir).unwrap();

        assert!(!dir
            .join(STEAM_SETTINGS_DIR)
            .join("steam_appid.txt")
            .exists());
        assert!(keep.exists(), "user content must survive teardown");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_game_dir_is_error() {
        let missing = std::env::temp_dir().join("gse-cfgtest-missing-dir");
        assert!(matches!(
            write_steam_settings(&missing, &sample_settings()),
            Err(EngineError::GameDirNotFound(_))
        ));
    }
}
