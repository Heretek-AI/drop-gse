//! Steam interface name extraction from steam_api binaries.
//!
//! Ports the scanning approach of SteamRoll's `GoldbergScanner.cs`: the
//! original DLL embeds its exported interface version strings (e.g.
//! `SteamUser021`) as plain ASCII; a chunked scan with overlap recovers them
//! so the emulator's `steam_interfaces.txt` can be generated. The scan must
//! happen on the ORIGINAL binary before replacement.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;

use crate::error::EngineError;

/// 64 KiB scan windows, matching GoldbergScanner.cs.
const BUFFER_SIZE: usize = 64 * 1024;
/// Bytes carried between consecutive windows so strings split across chunk
/// boundaries are still matched, matching GoldbergScanner.cs.
const OVERLAP: usize = 1024;

/// Interface-family prefixes scanned for, in the same order as
/// GoldbergScanner.cs. Each is matched as `<Prefix>\d+`.
///
/// Note the quirks preserved from the reference: `SteamMatchMaking` has a
/// capital "M", and plain `SteamNetworking` is distinct from the
/// `SteamNetworking{Messages,Sockets,Utils}` family.
const INTERFACE_PREFIXES: &[&str] = &[
    "SteamClient",
    "SteamUser",
    "SteamFriends",
    "SteamUtils",
    "SteamMatchMaking",
    "SteamUserStats",
    "SteamApps",
    "SteamNetworking",
    "SteamRemoteStorage",
    "SteamScreenshots",
    "SteamHTTP",
    "SteamController",
    "SteamUGC",
    "SteamAppList",
    "SteamMusic",
    "SteamMusicRemote",
    "SteamHTMLSurface",
    "SteamInventory",
    "SteamVideo",
    "SteamParentalSettings",
    "SteamInput",
    "SteamParties",
    "SteamRemotePlay",
    "SteamNetworkingMessages",
    "SteamNetworkingSockets",
    "SteamNetworkingUtils",
    "SteamGameServer",
    "SteamGameServerStats",
];

/// Compile one regex per interface family: `^(Prefix)\d+$` applied to
/// whitespace-bounded tokens, or simply `Prefix\d+` over raw chunks. We use
/// plain `Prefix\d+` like the C# original — interface strings are unique
/// enough that false positives are not a practical concern.
fn compile_patterns() -> Vec<Regex> {
    INTERFACE_PREFIXES
        .iter()
        .map(|p| Regex::new(&format!(r"{p}\d+")).expect("static pattern"))
        .collect()
}

/// Extract all Steam interface version names embedded in `dll_path`.
///
/// Returns a sorted, deduplicated list. Scanning is lossy-ASCII tolerant:
/// non-ASCII bytes simply never match the patterns.
pub fn extract_interfaces(dll_path: &Path) -> Result<Vec<String>, EngineError> {
    let data = std::fs::read(dll_path)
        .map_err(|e| EngineError::ScanFailed(format!("{}: {e}", dll_path.display())))?;
    Ok(extract_interfaces_from_bytes(&data))
}

/// Byte-slice variant of [`extract_interfaces`], used by tests and callers
/// that already hold the payload.
pub fn extract_interfaces_from_bytes(data: &[u8]) -> Vec<String> {
    let patterns = compile_patterns();
    let mut found = BTreeSet::new();

    if data.is_empty() {
        return Vec::new();
    }

    let mut offset = 0usize;
    loop {
        let end = (offset + BUFFER_SIZE).min(data.len());
        // Lossy conversion keeps window alignment simple; invalid UTF-8 bytes
        // become U+FFFD which cannot match any pattern.
        let chunk = String::from_utf8_lossy(&data[offset..end]);
        for re in &patterns {
            for m in re.find_iter(&chunk) {
                found.insert(m.as_str().to_string());
            }
        }
        if end >= data.len() {
            break;
        }
        // Carry OVERLAP bytes forward (GoldbergScanner.cs Array.Copy logic).
        offset = end - OVERLAP.min(end);
    }

    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_basic_interfaces() {
        let blob = b"\x00\x01SteamUser021\x00\x02SteamMatchMaking009\x00";
        let got = extract_interfaces_from_bytes(blob);
        assert_eq!(got, vec!["SteamMatchMaking009", "SteamUser021"]);
    }

    #[test]
    fn dedupes_repeated_names_and_sorts_output() {
        let blob = b"SteamUtils010SteamUtils010SteamClient017";
        let got = extract_interfaces_from_bytes(blob);
        assert_eq!(got, vec!["SteamClient017", "SteamUtils010"]);
    }

    #[test]
    fn distinguishes_networking_family() {
        let blob = b"SteamNetworking005SteamNetworkingSockets012SteamNetworkingMessages004";
        let got = extract_interfaces_from_bytes(blob);
        assert_eq!(
            got,
            vec![
                "SteamNetworking005",
                "SteamNetworkingMessages004",
                "SteamNetworkingSockets012"
            ]
        );
    }

    #[test]
    fn finds_interface_split_across_chunk_boundary() {
        // Place an interface so it straddles the first window boundary at
        // BUFFER_SIZE - small offset; the 1 KiB overlap must recover it.
        let mut blob: Vec<u8> = vec![b'A'; BUFFER_SIZE - 8];
        blob.extend_from_slice(b"SteamUser021");
        blob.extend_from_slice(&[b'B'; 128]);
        let got = extract_interfaces_from_bytes(&blob);
        assert!(
            got.contains(&"SteamUser021".to_string()),
            "overlap must recover boundary-split match, got {got:?}"
        );
    }

    #[test]
    fn empty_input_yields_empty_list() {
        assert!(extract_interfaces_from_bytes(b"").is_empty());
    }

    #[test]
    fn ignores_partial_names_without_version_digits() {
        let blob = b"SteamUserNoVersion SteamUser";
        assert!(extract_interfaces_from_bytes(blob).is_empty());
    }

    #[test]
    fn missing_file_is_scan_error_not_panic() {
        let missing = std::env::temp_dir().join("gse-iface-missing.dll");
        let err = extract_interfaces(&missing).unwrap_err();
        assert!(matches!(err, EngineError::ScanFailed(_)));
    }
}
