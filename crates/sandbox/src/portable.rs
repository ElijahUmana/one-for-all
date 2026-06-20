//! Cross-platform isolation primitive trait.
//!
//! Phase 2.5 portability: the macOS implementation uses APFS
//! `clonefile(2)` + `sandbox-exec`. Linux will use `systemd-nspawn` (or
//! `bwrap` / LXC) with `overlayfs` for the COW story.
//!
//! This module defines the trait so the broker depends on the abstract
//! shape, not on the macOS impl directly. Linux gets a real impl when
//! Phase 2.5 lands; today it's a `Disabled` fallback that surfaces a
//! clear "not yet implemented" without silently degrading.

use std::path::{Path, PathBuf};

use crate::inherit::InheritSpec;
use crate::Result;

/// Per-session isolation outputs. `argv_prefix` is what the broker
/// prepends to the Chromium argv when spawning. Empty `argv_prefix`
/// means "no wrapping" (e.g. on a Linux host before Phase 2.5 ships).
#[derive(Debug, Clone)]
pub struct PreparedSandbox {
    /// Path to the per-session rootfs. `--user-data-dir` is set to this.
    pub rootfs: PathBuf,
    /// Argv prefix to splice in before the Chromium binary.
    pub argv_prefix: Vec<std::ffi::OsString>,
    /// Whether real confinement is active. `false` = fall-through.
    pub enforced: bool,
}

/// Cross-platform sandboxing primitive.
pub trait Isolator: Send + Sync {
    /// Prepare a fresh per-session sandbox. Clones host state per
    /// `inherit`, generates whatever profile/policy file the OS needs,
    /// returns the argv wrapper.
    fn prepare(
        &self,
        session_id: &str,
        rootfs: &Path,
        host_chrome_profile: Option<&Path>,
        inherit: &[InheritSpec],
        network_outbound: bool,
    ) -> Result<PreparedSandbox>;

    /// Human-readable platform name, surfaced via `_internal.status`.
    fn name(&self) -> &'static str;
}

/// Macros host: platform-default. Returns the right impl for the build
/// target without callers having to `#[cfg]` themselves.
#[cfg(target_os = "macos")]
pub fn default_isolator() -> Box<dyn Isolator> {
    Box::new(MacOsIsolator)
}

#[cfg(not(target_os = "macos"))]
pub fn default_isolator() -> Box<dyn Isolator> {
    Box::new(LinuxStubIsolator)
}

// ---------- macOS impl (delegates to the rest of this crate) ----------

#[cfg(target_os = "macos")]
pub struct MacOsIsolator;

#[cfg(target_os = "macos")]
impl Isolator for MacOsIsolator {
    fn prepare(
        &self,
        session_id: &str,
        rootfs: &Path,
        host_chrome_profile: Option<&Path>,
        inherit: &[InheritSpec],
        network_outbound: bool,
    ) -> Result<PreparedSandbox> {
        use crate::clone::{clone_chrome_profile, clone_user_dirs};
        use crate::errors::Error;
        use crate::sbpl::{generate_sbpl, write_sbpl, SbplParams};
        use crate::spawn::SANDBOX_EXEC_PATH;

        std::fs::create_dir_all(rootfs).map_err(|e| Error::io(rootfs, e))?;

        if let Some(host) = host_chrome_profile {
            match clone_chrome_profile(host, rootfs) {
                Ok(_) | Err(Error::DestinationExists(_)) => {}
                Err(Error::CloneUnsupported { .. }) => {
                    // Fall through; broker handles V-R1 seeding.
                }
                Err(e) => return Err(e),
            }
        }

        if !inherit.is_empty() {
            match clone_user_dirs(inherit, rootfs) {
                Ok(_) => {}
                Err(Error::CloneUnsupported { .. }) => {}
                Err(e) => return Err(e),
            }
        }

        let canonical = std::fs::canonicalize(rootfs).map_err(|e| Error::io(rootfs, e))?;
        let mut params = SbplParams::from_inherit(session_id, &canonical, inherit);
        params.network_outbound = network_outbound;
        let text = generate_sbpl(&params);
        let profile = canonical.join("sandbox.sb");
        write_sbpl(&profile, &text)?;

        let argv_prefix: Vec<std::ffi::OsString> = vec![
            SANDBOX_EXEC_PATH.into(),
            "-f".into(),
            profile.as_os_str().to_owned(),
            "--".into(),
        ];

        Ok(PreparedSandbox {
            rootfs: canonical,
            argv_prefix,
            enforced: true,
        })
    }

    fn name(&self) -> &'static str {
        "macos:apfs+sandbox-exec"
    }
}

// ---------- Linux stub (Phase 2.5 placeholder, NOT theatre) ----------

pub struct LinuxStubIsolator;

impl Isolator for LinuxStubIsolator {
    fn prepare(
        &self,
        _session_id: &str,
        rootfs: &Path,
        _host_chrome_profile: Option<&Path>,
        _inherit: &[InheritSpec],
        _network_outbound: bool,
    ) -> Result<PreparedSandbox> {
        std::fs::create_dir_all(rootfs).map_err(|e| crate::errors::Error::io(rootfs, e))?;
        // Surface explicit "not enforced" rather than silently producing
        // a sandbox that isn't one. The broker logs this and the operator
        // sees a "running unconfined" signal.
        tracing::warn!(
            rootfs = %rootfs.display(),
            "Linux isolator stub: Phase 2.5 nspawn/overlayfs not yet shipped — agent runs unconfined"
        );
        Ok(PreparedSandbox {
            rootfs: rootfs.to_path_buf(),
            argv_prefix: Vec::new(),
            enforced: false,
        })
    }

    fn name(&self) -> &'static str {
        "linux:stub-unconfined"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_stub_returns_unenforced_prepared_sandbox() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let i = LinuxStubIsolator;
        let p = i
            .prepare("s_lin", &tmp.path().join("rootfs"), None, &[], true)
            .expect("ok");
        assert!(!p.enforced);
        assert!(p.argv_prefix.is_empty());
        assert_eq!(i.name(), "linux:stub-unconfined");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_isolator_produces_enforced_sandbox_with_prefix() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let i = MacOsIsolator;
        let rootfs = tmp.path().join("rootfs");
        let p = i
            .prepare("s_mac", &rootfs, None, &[], true)
            .expect("prepare");
        assert!(p.enforced);
        assert_eq!(p.argv_prefix.len(), 4);
        assert_eq!(
            p.argv_prefix[0],
            std::ffi::OsString::from("/usr/bin/sandbox-exec")
        );
        assert_eq!(p.argv_prefix[1], std::ffi::OsString::from("-f"));
        assert_eq!(p.argv_prefix[3], std::ffi::OsString::from("--"));
    }
}
