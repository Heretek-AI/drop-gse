//! Pinned-release fetcher for Goldberg-family emulator distributions.
//!
//! Downloads a release asset from a **pinned tag** (never `latest` — the
//! architecture spec §5 requires checksummed, reproducible sources), verifies
//! its SHA-256 digest against an expected value supplied by the server's
//! `EmulatorBinding`, and caches it under the platform data dir keyed by
//! digest so repeat sessions don't re-download.
//!
//! Extraction pulls only the binaries we need out of the archive:
//! gbe_fork packages them at `release/regular/<arch>/<lib>`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::fs;

use crate::error::EngineError;

/// gbe_fork release asset names (goldberg-patcher `cross.rs` constants).
pub const GBE_WIN_ASSET: &str = "emu-win-release.7z";
pub const GBE_LINUX_ASSET: &str = "emu-linux-release.tar.bz2";

/// Direct-download base; `<base>/releases/download/<tag>/<asset>`.
pub const GBE_RELEASES_BASE: &str = "https://github.com/Detanup01/gbe_fork/releases/download";

/// Which emulator distribution to fetch.
#[derive(Debug, Clone)]
pub struct ReleaseSpec {
    /// Release tag, e.g. `v0.9.9` (pinned — never "latest").
    pub tag: String,
    /// Asset filename inside that release, e.g. [`GBE_WIN_ASSET`].
    pub asset_name: String,
    /// Expected SHA-256 of the downloaded asset (hex). Mismatch aborts.
    pub sha256: String,
}

impl ReleaseSpec {
    /// Build a validated spec: `sha256` must be exactly 64 lowercase hex
    /// chars; `tag`/`asset_name` must not contain path separators or `..`
    /// (they are interpolated into a download URL and cache filenames, so a
    /// crafted value must not be able to escape either).
    pub fn new(tag: String, asset_name: String, sha256: String) -> Result<Self, EngineError> {
        let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if sha256.len() != 64 || !sha256.bytes().all(is_hex) {
            return Err(EngineError::Fetch(format!(
                "invalid sha256 digest {sha256:?}: expected 64 lowercase hex chars"
            )));
        }
        for (label, value) in [("tag", &tag), ("asset", &asset_name)] {
            if value.is_empty()
                || value.contains(['/', '\\'])
                || value.split(['/', '\\']).any(|seg| seg == "..")
                || value.contains("..")
            {
                return Err(EngineError::Fetch(format!(
                    "invalid release {label} {value:?}: path separators and '..' are not allowed"
                )));
            }
        }
        Ok(Self {
            tag,
            asset_name,
            sha256,
        })
    }

    pub fn url(&self) -> String {
        format!("{GBE_RELEASES_BASE}/{}/{}", self.tag, self.asset_name)
    }
}

/// Where fetched+verified archives are cached. Keyed by digest so distinct
/// releases never collide and partial downloads can't masquerade as cache hits.
fn cache_path(cache_dir: &Path, spec: &ReleaseSpec) -> PathBuf {
    cache_dir.join(format!("gse-emu-{}.bin", spec.sha256))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn sha256_of(path: &Path) -> Result<String, EngineError> {
    let data = fs::read(path).await?;
    Ok(to_hex(&Sha256::digest(&data)))
}

/// Ensure the release asset named by `spec` exists in `cache_dir` with a
/// verified digest, downloading it via `download_to` when needed.
///
/// `download_to(url, dest)` is injected so tests can serve fixtures from a
/// local server instead of GitHub.
pub async fn ensure_cached<F, Fut>(
    cache_dir: &Path,
    spec: &ReleaseSpec,
    download_to: F,
) -> Result<PathBuf, EngineError>
where
    F: FnOnce(String, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<(), EngineError>>,
{
    let cached = cache_path(cache_dir, spec);
    // Verified cache hit — otherwise fall through and re-download below.
    if cached.is_file() && sha256_of(&cached).await? == spec.sha256 {
        return Ok(cached);
    }

    fs::create_dir_all(cache_dir).await?;
    // Download to a STAGING path first; the caller's downloader is responsible
    // for atomic placement at that staging path. The final digest-keyed name
    // only ever appears once the artifact has passed verification below, so
    // concurrent invocations can't observe each other's partial writes.
    let staging = cached.with_extension("staging");
    download_to(spec.url(), staging.clone()).await?;

    let actual = sha256_of(&staging).await?;
    if actual != spec.sha256 {
        // Remove bad artifact immediately so nothing downstream trusts it.
        let _ = fs::remove_file(&staging).await;
        return Err(EngineError::DigestMismatch {
            url: spec.url(),
            expected: spec.sha256.clone(),
            found: actual,
        });
    }
    // Digest verified at the staging path — only NOW does it earn the final
    // digest-keyed name (atomic within the same filesystem).
    fs::rename(&staging, &cached).await?;
    Ok(cached)
}

/// Locate `wanted` files inside the archive root once extracted, returning
/// `(lib, path)` pairs under `extract_dir`. gbe_fork archives nest payload
/// DLLs at `release/regular/<arch>/<name>` (`libs_patch.rs` in
/// goldberg-patcher).
///
/// `arch_for` maps each lib to its ABI folder — resolution is by the
/// TARGET's ABI, not the host (32-bit `steam_api.dll` → `x32` even on an
/// x86_64 host). Libs whose ABI is unknown map to `None` and are omitted
/// from the result.
pub fn emulator_payload_paths(
    extract_dir: &Path,
    arch_for: impl Fn(&str) -> Option<&'static str>,
    libs: &[&str],
) -> Vec<(String, PathBuf)> {
    libs.iter()
        .filter_map(|lib| {
            arch_for(lib).map(|arch| {
                (
                    lib.to_string(),
                    extract_dir
                        .join("release")
                        .join("regular")
                        .join(arch)
                        .join(lib),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PAYLOAD: &[u8] = b"fake-emulator-archive-bytes";
    const GOOD_DIGEST: &str = "a3f2b0e1c5d4a6f8b9c0d1e2f3a4b5c6d7e8f90123456789abcdef0123456789";

    fn digest_of(data: &[u8]) -> String {
        to_hex(&Sha256::digest(data))
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let tmp = TempDir::new().unwrap();
        let spec =
            ReleaseSpec::new("v0.9.9".into(), GBE_WIN_ASSET.into(), digest_of(PAYLOAD)).unwrap();
        let served = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let s2 = served.clone();
        let path = ensure_cached(tmp.path(), &spec, move |_url, dest| async move {
            *s2.lock().unwrap() += 1;
            fs::write(dest, PAYLOAD).await.map_err(EngineError::from)
        })
        .await
        .unwrap();
        assert_eq!(*served.lock().unwrap(), 1);
        assert_eq!(fs::read(&path).await.unwrap(), PAYLOAD);
    }

    #[tokio::test]
    async fn digest_mismatch_aborts_and_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let spec =
            ReleaseSpec::new("v0.9.9".into(), GBE_WIN_ASSET.into(), GOOD_DIGEST.into()).unwrap(); // well-formed but WRONG digest on purpose
        let err = ensure_cached(tmp.path(), &spec, |_url, dest| async move {
            fs::write(dest, PAYLOAD).await.map_err(EngineError::from)
        })
        .await
        .unwrap_err();
        assert!(matches!(err, EngineError::DigestMismatch { .. }));
        // Bad artifact must not remain in the cache dir.
        assert!(cache_path(tmp.path(), &spec).metadata().is_err());
    }

    #[tokio::test]
    async fn second_call_is_cache_hit_without_download() {
        let tmp = TempDir::new().unwrap();
        let spec =
            ReleaseSpec::new("v0.9.9".into(), GBE_LINUX_ASSET.into(), digest_of(PAYLOAD)).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let c2 = calls.clone();
        // Fn closure (not FnOnce) so the same downloader can serve both calls.
        let downloader = |_url: String, dest: PathBuf| {
            let c = c2.clone();
            async move {
                *c.lock().unwrap() += 1;
                fs::write(dest, PAYLOAD).await.map_err(EngineError::from)
            }
        };
        use std::future::Future;
        async fn invoke<
            F: Fn(String, PathBuf) -> Fut,
            Fut: Future<Output = Result<(), EngineError>>,
        >(
            f: &F,
            u: String,
            d: PathBuf,
        ) -> Result<(), EngineError> {
            f(u, d).await
        }
        ensure_cached(tmp.path(), &spec, |u, d| invoke(&downloader, u, d))
            .await
            .unwrap();
        ensure_cached(tmp.path(), &spec, |u, d| invoke(&downloader, u, d))
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), 1, "second call must be a cache hit");
    }

    #[tokio::test]
    async fn corrupt_cache_is_replaced_not_trusted() {
        let tmp = TempDir::new().unwrap();
        let spec =
            ReleaseSpec::new("v0.9.9".into(), GBE_WIN_ASSET.into(), digest_of(PAYLOAD)).unwrap();
        // Plant garbage where a valid cache entry would live.
        fs::write(cache_path(tmp.path(), &spec), b"garbage")
            .await
            .unwrap();

        let path = ensure_cached(tmp.path(), &spec, |_url, dest| async move {
            fs::write(dest, PAYLOAD).await.map_err(EngineError::from)
        })
        .await
        .unwrap();
        assert_eq!(fs::read(&path).await.unwrap(), PAYLOAD);
    }

    #[test]
    fn payload_paths_follow_archive_layout() {
        let root = Path::new("/tmp/x");
        // Per-TARGET ABI: 32-bit names → x32 even on an x86_64 host.
        let arch_for = |lib: &str| match lib {
            "steam_api.dll" | "steamclient.dll" => Some("x32"),
            _ => Some("x64"),
        };
        let paths = emulator_payload_paths(
            root,
            arch_for,
            &["steam_api.dll", "steam_api64.dll", "steamclient64.dll"],
        );
        assert_eq!(
            paths[0].1,
            root.join("release/regular/x32/steam_api.dll"),
            "32-bit targets resolve to x32 regardless of host"
        );
        assert_eq!(
            paths[1].1,
            root.join("release/regular/x64/steam_api64.dll"),
            "gbe_fork archives use release/regular/<arch>/<lib>"
        );
        assert_eq!(paths[2].0, "steamclient64.dll");
    }

    #[test]
    fn unknown_abi_is_omitted_not_mispathed() {
        let root = Path::new("/tmp/x");
        let paths = emulator_payload_paths(root, |_| None, &["steam_api64.dll"]);
        assert!(paths.is_empty());
    }

    #[test]
    fn release_spec_validation() {
        assert!(ReleaseSpec::new("v1".into(), "a.7z".into(), digest_of(b"x")).is_ok());
        // Wrong digest length / charset rejected.
        assert!(ReleaseSpec::new("v1".into(), "a.7z".into(), "abc".into()).is_err());
        assert!(ReleaseSpec::new("v1".into(), "a.7z".into(), "Z".repeat(64)).is_err());
        // Path traversal in tag or asset rejected.
        assert!(ReleaseSpec::new("../evil".into(), "a.7z".into(), digest_of(b"x")).is_err());
        assert!(ReleaseSpec::new("v1".into(), "sub/dir.7z".into(), digest_of(b"x")).is_err());
        // Empty values rejected.
        assert!(ReleaseSpec::new(String::new(), "a.7z".into(), digest_of(b"x")).is_err());
    }
}
