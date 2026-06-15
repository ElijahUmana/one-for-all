//! Chrome-for-Testing manifest parsing.
//!
//! Owned by `chromium-fetcher`. Stateless. Two manifest formats are handled:
//!
//! * `last-known-good-versions-with-downloads.json` — channel → version map.
//!   Cached at `~/.one-for-all/cache/docs/cft-last-known.json`.
//! * `known-good-versions-with-downloads.json` — full historical version list.
//!   Cached at `~/.one-for-all/cache/docs/cft-known-good.json`.
//!
//! Both feed the same lookup: given a desired `(version, platform)`, return
//! the download URL.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Channel name. Per SPEC D12 the default resolved channel is `Stable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
    Dev,
    Canary,
}

impl Channel {
    fn as_key(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Beta => "Beta",
            Self::Dev => "Dev",
            Self::Canary => "Canary",
        }
    }
}

/// One platform/url tuple inside `downloads.chrome[]`.
#[derive(Debug, Clone, Deserialize)]
struct PlatformUrl {
    platform: String,
    url: String,
}

/// `downloads` payload — only the `chrome` asset is needed for our purposes.
/// Other assets (chromedriver, chrome-headless-shell) are explicitly ignored;
/// extras in the JSON are accepted via serde's default behavior so that
/// future asset additions don't break parsing.
#[derive(Debug, Clone, Deserialize)]
struct Downloads {
    #[serde(default)]
    chrome: Vec<PlatformUrl>,
}

/// One row of `known-good-versions-with-downloads.json`'s `versions[]`.
#[derive(Debug, Clone, Deserialize)]
struct VersionEntry {
    version: String,
    #[allow(dead_code)]
    revision: String,
    downloads: Downloads,
}

#[derive(Debug, Clone, Deserialize)]
struct KnownGood {
    versions: Vec<VersionEntry>,
}

/// One row of `last-known-good-versions-with-downloads.json`'s `channels.*`.
#[derive(Debug, Clone, Deserialize)]
struct ChannelEntry {
    #[allow(dead_code)]
    channel: String,
    version: String,
    #[allow(dead_code)]
    revision: String,
    downloads: Downloads,
}

#[derive(Debug, Clone, Deserialize)]
struct LastKnown {
    channels: HashMap<String, ChannelEntry>,
}

/// Resolved download target: a chromium version + a direct URL for the host
/// platform's zip.
#[derive(Debug, Clone)]
pub struct ResolvedDownload {
    pub version: String,
    pub url: String,
}

/// Parse `last-known-good-versions-with-downloads.json` and return the
/// download URL for the given channel + platform.
pub fn resolve_from_last_known(
    last_known_json_bytes: &[u8],
    channel: Channel,
    platform_key: &str,
) -> Result<ResolvedDownload> {
    let parsed: LastKnown =
        serde_json::from_slice(last_known_json_bytes).context("parse cft-last-known.json")?;
    let entry = parsed
        .channels
        .get(channel.as_key())
        .ok_or_else(|| anyhow!("channel {:?} not present in last-known manifest", channel))?;
    let url = entry
        .downloads
        .chrome
        .iter()
        .find(|p| p.platform == platform_key)
        .ok_or_else(|| {
            anyhow!(
                "platform {} not present in chrome downloads for channel {:?}",
                platform_key,
                channel
            )
        })?
        .url
        .clone();
    Ok(ResolvedDownload {
        version: entry.version.clone(),
        url,
    })
}

/// Parse `known-good-versions-with-downloads.json` and return the download URL
/// for an exact version + platform.
pub fn resolve_from_known_good(
    known_good_json_bytes: &[u8],
    version: &str,
    platform_key: &str,
) -> Result<ResolvedDownload> {
    let parsed: KnownGood =
        serde_json::from_slice(known_good_json_bytes).context("parse cft-known-good.json")?;
    let entry = parsed
        .versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| anyhow!("version {} not in known-good manifest", version))?;
    let url = entry
        .downloads
        .chrome
        .iter()
        .find(|p| p.platform == platform_key)
        .ok_or_else(|| {
            anyhow!(
                "platform {} not present in chrome downloads for version {}",
                platform_key,
                version
            )
        })?
        .url
        .clone();
    Ok(ResolvedDownload {
        version: entry.version.clone(),
        url,
    })
}

/// Locate the cached manifest files. The fetcher prefers cache-only operation
/// for reproducibility (SPEC D12); a future revision can fall through to the
/// live HTTPS endpoints when the cache is older than a TTL.
pub fn cache_dir() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home dir"))?;
    Ok(home.join(".one-for-all").join("cache").join("docs"))
}

/// Read the bytes of a cache file, with a clear error including the path.
pub fn read_cache(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read manifest cache file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAST_KNOWN_FIXTURE: &[u8] = br#"{
        "timestamp": "2026-06-13T09:14:42.266Z",
        "channels": {
            "Stable": {
                "channel": "Stable",
                "version": "149.0.7827.115",
                "revision": "1625079",
                "downloads": {
                    "chrome": [
                        {"platform": "linux64", "url": "https://example.test/linux64.zip"},
                        {"platform": "mac-arm64", "url": "https://example.test/mac-arm64.zip"},
                        {"platform": "mac-x64", "url": "https://example.test/mac-x64.zip"}
                    ]
                }
            }
        }
    }"#;

    const KNOWN_GOOD_FIXTURE: &[u8] = br#"{
        "timestamp": "2026-06-13T09:14:42.266Z",
        "versions": [
            {
                "version": "113.0.5672.0",
                "revision": "1121400",
                "downloads": {
                    "chrome": [
                        {"platform": "mac-arm64", "url": "https://example.test/old-mac-arm64.zip"}
                    ]
                }
            },
            {
                "version": "149.0.7827.115",
                "revision": "1625079",
                "downloads": {
                    "chrome": [
                        {"platform": "mac-arm64", "url": "https://example.test/new-mac-arm64.zip"},
                        {"platform": "linux64", "url": "https://example.test/new-linux64.zip"}
                    ]
                }
            }
        ]
    }"#;

    #[test]
    fn last_known_resolves_stable_mac_arm64() {
        let r = resolve_from_last_known(LAST_KNOWN_FIXTURE, Channel::Stable, "mac-arm64").unwrap();
        assert_eq!(r.version, "149.0.7827.115");
        assert_eq!(r.url, "https://example.test/mac-arm64.zip");
    }

    #[test]
    fn last_known_unknown_platform_errors() {
        let err =
            resolve_from_last_known(LAST_KNOWN_FIXTURE, Channel::Stable, "win64").unwrap_err();
        assert!(err.to_string().contains("win64"));
    }

    #[test]
    fn known_good_resolves_specific_version() {
        let r = resolve_from_known_good(KNOWN_GOOD_FIXTURE, "149.0.7827.115", "linux64").unwrap();
        assert_eq!(r.version, "149.0.7827.115");
        assert_eq!(r.url, "https://example.test/new-linux64.zip");
    }

    #[test]
    fn known_good_unknown_version_errors() {
        let err =
            resolve_from_known_good(KNOWN_GOOD_FIXTURE, "999.0.0.0", "mac-arm64").unwrap_err();
        assert!(err.to_string().contains("999"));
    }

    /// Smoke parse the real cached manifests if present (skipped otherwise).
    /// This catches schema drift between the fixtures above and the wild data
    /// without hard-coding live URLs.
    #[test]
    fn parses_real_cached_manifests_if_present() {
        let dir = match cache_dir() {
            Ok(d) => d,
            Err(_) => return,
        };
        let last = dir.join("cft-last-known.json");
        if last.exists() {
            let bytes = std::fs::read(&last).unwrap();
            // Try to resolve mac-arm64 stable; success or a clean platform error are both OK
            // (different runners).
            let _ = resolve_from_last_known(&bytes, Channel::Stable, "mac-arm64");
        }
        let kg = dir.join("cft-known-good.json");
        if kg.exists() {
            let bytes = std::fs::read(&kg).unwrap();
            let parsed: KnownGood = serde_json::from_slice(&bytes).unwrap();
            assert!(parsed.versions.len() > 100);
        }
    }
}
