//! `chromium-fetcher` — resolve, download, verify, and extract a
//! Chrome-for-Testing build under `~/.one-for-all/chromium/<rev>/`.
//!
//! # Public API
//!
//! Single entry point: [`fetch`]. Pass `None` to resolve the current Stable
//! channel from the cached `cft-last-known.json`, or `Some("149.0.7827.115")`
//! to pin to an exact revision.
//!
//! Returns the absolute path to the launchable Chromium binary.
//!
//! # Threading & ownership
//!
//! Stateless module; the only "owner" is the on-disk install root at
//! `~/.one-for-all/chromium/`. Concurrent calls for the same revision are
//! safe in the sense that the second caller will observe the `.complete`
//! marker and short-circuit. Concurrent first-time fetches of the same rev
//! would each try to download — fine but wasteful; callers should serialize
//! upstream if they care.
//!
//! # Reproducibility (SPEC D12)
//!
//! - Manifests come from cached JSON at `~/.one-for-all/cache/docs/`. The
//!   cache files are populated out-of-band (research dump). When the cache
//!   is missing for a requested revision, [`fetch`] returns an error rather
//!   than silently hitting the live endpoint.
//! - Per-revision SHA-256 is TOFU: written on first fetch, required on
//!   subsequent fetches.

// SPEC §10: zero `.unwrap()` / `.expect()` in production code.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

pub mod download;
pub mod extract;
pub mod manifest;
pub mod platform;
pub mod verify;

pub use manifest::Channel;
pub use platform::Platform;

/// Default install root: `~/.one-for-all/chromium`.
pub fn install_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home dir"))?;
    Ok(home.join(".one-for-all").join("chromium"))
}

/// Build the path to the launchable Chromium binary inside an extracted rev.
fn binary_path(rev_dir: &Path, plat: Platform) -> PathBuf {
    let mut p = rev_dir.join(plat.extracted_subdir());
    for seg in plat.binary_relpath() {
        p.push(seg);
    }
    p
}

/// Options for [`fetch`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub channel: Channel,
    pub install_root: Option<PathBuf>,
    pub download: download::DownloadOptions,
    /// If set, override platform detection (mostly useful for tests).
    pub platform: Option<Platform>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            channel: Channel::Stable,
            install_root: None,
            download: download::DownloadOptions::default(),
            platform: None,
        }
    }
}

/// Resolve, download, verify, and extract a Chrome-for-Testing build.
///
/// * `rev = None` → use [`FetchOptions::channel`] (default Stable) and
///   resolve the version from `cft-last-known.json`.
/// * `rev = Some("149.0.7827.115")` → pin to that exact revision via
///   `cft-known-good.json`.
///
/// Returns the absolute path to the launchable Chromium binary.
pub async fn fetch(rev: Option<&str>, opts: &FetchOptions) -> Result<PathBuf> {
    let plat = match opts.platform {
        Some(p) => p,
        None => Platform::detect()?,
    };
    let cache = manifest::cache_dir().context("locate manifest cache dir")?;

    let resolved = if let Some(version) = rev {
        let path = cache.join("cft-known-good.json");
        let bytes = manifest::read_cache(&path)?;
        manifest::resolve_from_known_good(&bytes, version, plat.key())?
    } else {
        let path = cache.join("cft-last-known.json");
        let bytes = manifest::read_cache(&path)?;
        manifest::resolve_from_last_known(&bytes, opts.channel, plat.key())?
    };

    let root = match &opts.install_root {
        Some(p) => p.clone(),
        None => install_root()?,
    };
    let rev_dir = root.join(&resolved.version);

    if extract::is_complete(&rev_dir) {
        let bin = binary_path(&rev_dir, plat);
        if bin.exists() {
            tracing::info!(version = %resolved.version, bin = %bin.display(), "fetch hit cache");
            return Ok(bin);
        }
        tracing::warn!(
            dir = %rev_dir.display(),
            "complete marker present but binary missing — re-fetching"
        );
    }

    std::fs::create_dir_all(&rev_dir)
        .with_context(|| format!("create rev dir {}", rev_dir.display()))?;

    let zip_path = rev_dir.join(format!("{}.zip", plat.extracted_subdir()));

    // SHA-256 TOFU with one retry: if a verify fails, the on-disk zip is
    // corrupt — wipe it and the matching `.tmp` companion, then re-fetch
    // from scratch. Two consecutive SHA failures escalate to the caller.
    let mut sha_attempts = 0u32;
    loop {
        sha_attempts += 1;
        download::download_zip(&resolved.url, &zip_path, &opts.download).await?;

        let sha_result: Result<()> = if let Some(expected) = verify::read_pin(&rev_dir)? {
            verify::verify_against(&zip_path, &expected).await
        } else {
            match verify::sha256_hex(&zip_path).await {
                Ok(actual) => {
                    verify::write_pin(&rev_dir, &actual)?;
                    tracing::info!(
                        version = %resolved.version,
                        sha256 = %actual,
                        "pinned new revision"
                    );
                    Ok(())
                }
                Err(e) => Err(e),
            }
        };

        match sha_result {
            Ok(()) => break,
            Err(e) if sha_attempts < 2 => {
                tracing::warn!(
                    zip = %zip_path.display(),
                    error = %e,
                    "sha verify failed; wiping zip+tmp and retrying once"
                );
                let _ = std::fs::remove_file(&zip_path);
                let mut tmp_name = zip_path
                    .file_name()
                    .map(|s| s.to_os_string())
                    .unwrap_or_default();
                tmp_name.push(".tmp");
                let _ = std::fs::remove_file(zip_path.with_file_name(tmp_name));
            }
            Err(e) => {
                return Err(e.context("sha verification failed twice in a row"));
            }
        }
    }

    extract::extract(&zip_path, &rev_dir)?;
    // Best-effort: remove the zip after a successful extract to save disk.
    let _ = std::fs::remove_file(&zip_path);

    let bin = binary_path(&rev_dir, plat);
    if !bin.exists() {
        return Err(anyhow!(
            "after extract, binary not at expected path {}",
            bin.display()
        ));
    }
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_path_mac_arm64_layout() {
        let bin = binary_path(Path::new("/tmp/rev"), Platform::MacArm64);
        let s = bin.to_string_lossy();
        assert!(s.ends_with(
            "/tmp/rev/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        ));
    }

    #[test]
    fn binary_path_linux64_layout() {
        let bin = binary_path(Path::new("/tmp/rev"), Platform::Linux64);
        assert_eq!(bin, Path::new("/tmp/rev/chrome-linux64/chrome"));
    }
}
