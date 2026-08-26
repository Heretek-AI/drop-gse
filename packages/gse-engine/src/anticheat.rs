//! Pre-flight anti-cheat detection.
//!
//! Refuses to patch a game directory when known anti-cheat payloads are
//! present — modified `steam_api` binaries are rejected by EAC/BattlEye and
//! can trip account bans. The scan is **fail-closed**: any traversal error
//! aborts patching rather than reporting a clear directory.

use std::path::Path;

use crate::error::EngineError;

/// Well-known anti-cheat module filenames (case-insensitive match).
pub const ANTICHEAT_MARKERS: &[&str] = &[
    "easyanticheat.exe",
    "easyanticheat_x64.dll",
    "easyanticheat_x86.dll",
    "easyanticheat.so",
    "beservice.exe",
    "bedaisy.sys",
    "battleye.dll",
];

/// Maximum recursion depth below `game_dir`. Shared with target location so
/// the "anticheat gate covers everything we touch" invariant holds — a
/// replaceable binary must never sit deeper than the scan reaches.
pub const MAX_SCAN_DEPTH: usize = 4;

/// Recursively scan `game_dir` for anti-cheat markers.
///
/// Returns `Ok(Some(marker))` on detection, `Ok(None)` when the scanned tree
/// is clear, and `Err` when the directory cannot be inspected — an incomplete
/// scan must never be reported as safe.
///
/// TODO(phase-3): consult a per-game compatibility database from the server.
pub fn detect(game_dir: &Path) -> Result<Option<String>, EngineError> {
    if !game_dir.is_dir() {
        return Err(EngineError::GameDirNotFound(game_dir.display().to_string()));
    }
    scan_dir(game_dir, 0)
}

fn scan_dir(dir: &Path, depth: usize) -> Result<Option<String>, EngineError> {
    if depth > MAX_SCAN_DEPTH {
        return Ok(None);
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| EngineError::ScanFailed(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| EngineError::ScanFailed(format!("{}: {e}", dir.display())))?;
        let name = entry.file_name();
        let name_lower = name.to_string_lossy().to_lowercase();
        if ANTICHEAT_MARKERS.contains(&name_lower.as_str()) {
            return Ok(Some(name_lower));
        }
        if entry
            .file_type()
            .map_err(|e| EngineError::ScanFailed(e.to_string()))?
            .is_dir()
        {
            if let Some(found) = scan_dir(&entry.path(), depth + 1)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_marker_at_top_level() {
        let tmp = std::env::temp_dir().join("gse-antictest-top");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("EasyAntiCheat.exe"), b"x").unwrap();
        assert_eq!(detect(&tmp).unwrap(), Some("easyanticheat.exe".to_string()));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detects_marker_nested_within_depth_limit() {
        let tmp = std::env::temp_dir().join("gse-antictest-nested");
        let nested = tmp.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("beservice.exe"), b"x").unwrap();
        assert_eq!(detect(&tmp).unwrap(), Some("beservice.exe".to_string()));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn clear_directory_returns_none() {
        let tmp = std::env::temp_dir().join("gse-antictest-clear");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("game.exe"), b"x").unwrap();
        assert_eq!(detect(&tmp).unwrap(), None);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_directory_is_error_not_clear() {
        let missing = std::env::temp_dir().join("gse-antictest-missing-does-not-exist");
        assert!(matches!(
            detect(&missing),
            Err(EngineError::GameDirNotFound(_))
        ));
    }
}
