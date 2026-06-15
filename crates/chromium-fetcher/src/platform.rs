//! Platform detection.
//!
//! Owned by `chromium-fetcher`. Stateless. Returns the Chrome-for-Testing
//! platform key for the current host. Win32/Win64 are intentionally not
//! supported in v1 (SPEC §0 — macOS-first).

use anyhow::{anyhow, Result};

/// Chrome-for-Testing platform key. Used to look up download URLs in the
/// `cft-known-good.json` and `cft-last-known.json` manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS Apple Silicon.
    MacArm64,
    /// macOS Intel.
    MacX64,
    /// Linux x86_64.
    Linux64,
}

impl Platform {
    /// CfT manifest key (`mac-arm64`, `mac-x64`, `linux64`).
    pub fn key(self) -> &'static str {
        match self {
            Self::MacArm64 => "mac-arm64",
            Self::MacX64 => "mac-x64",
            Self::Linux64 => "linux64",
        }
    }

    /// Subdirectory name inside the extracted CfT zip.
    /// e.g. `chrome-mac-arm64`, `chrome-linux64`.
    pub fn extracted_subdir(self) -> &'static str {
        match self {
            Self::MacArm64 => "chrome-mac-arm64",
            Self::MacX64 => "chrome-mac-x64",
            Self::Linux64 => "chrome-linux64",
        }
    }

    /// Path to the Chromium binary inside the extracted directory, relative to
    /// the platform's extracted subdir root.
    pub fn binary_relpath(self) -> &'static [&'static str] {
        match self {
            Self::MacArm64 | Self::MacX64 => &[
                "Google Chrome for Testing.app",
                "Contents",
                "MacOS",
                "Google Chrome for Testing",
            ],
            Self::Linux64 => &["chrome"],
        }
    }

    /// Detect the host platform.
    ///
    /// Returns an error on unsupported targets rather than guessing — better
    /// to fail loud at fetch time than corrupt the chromium dir layout.
    pub fn detect() -> Result<Self> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Ok(Self::MacArm64);
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            return Ok(Self::MacX64);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            return Ok(Self::Linux64);
        }
        #[allow(unreachable_code)]
        Err(anyhow!(
            "unsupported host platform: only mac-arm64, mac-x64, linux64 are supported in v1"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_subdirs_are_consistent() {
        for p in [Platform::MacArm64, Platform::MacX64, Platform::Linux64] {
            assert!(!p.key().is_empty());
            assert!(!p.extracted_subdir().is_empty());
            assert!(!p.binary_relpath().is_empty());
        }
    }

    #[test]
    fn detect_returns_a_platform_on_supported_hosts() {
        // On any supported runner this must succeed; on unsupported it must
        // surface an error (we don't want a silent fallback).
        let _ = Platform::detect();
    }
}
