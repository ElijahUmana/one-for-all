//! Sandbox denial probe.
//!
//! sandbox-exec denials are silent — the syscall returns `EPERM` and
//! the caller usually has no error pretty-printer wired. The broker
//! runs a probe at session start that intentionally tries to WRITE to a
//! known-denied path; if the probe SUCCEEDS the sandbox is theatre and
//! we surface that to the agent (logs + a metric flip).
//!
//! Why this matters: a profile error (typo in the .sb, missing path,
//! wrong mach-service name) might produce a sandbox that allows
//! everything. The default-deny stance means a *non-functional* profile
//! is even more dangerous than no profile at all because the operator
//! thinks they're confined.
//!
//! ## Why writes, not reads
//!
//! The base profile has to grant `file-read*` on `/` for Chromium to find
//! its system libraries (libSystem, /usr/lib, /System frameworks). That
//! means a read-based probe ALWAYS succeeds even when the sandbox is
//! correctly enforcing. Writes are the right test surface: the profile
//! must NOT allow `file-write*` outside `allowed_rw`.

use std::path::Path;
use std::process::Command;

use crate::errors::Error;
use crate::spawn::SANDBOX_EXEC_PATH;
use crate::Result;

/// Outcome of `probe_sandbox_enforces`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ProbeOutcome {
    /// The profile rejected our forbidden write — sandbox is functional.
    Enforcing,
    /// The profile allowed our forbidden write — sandbox is theatre.
    NotEnforcing,
    /// The probe could not run (sandbox-exec missing, path issue, etc.).
    Inconclusive,
}

/// Run a denial probe against `profile_path` using `forbidden_target` as
/// a known-outside-rootfs path. The function will create+remove the
/// target file as part of the probe; supply a path the caller doesn't
/// care about.
///
/// `forbidden_target` must be:
/// - OUTSIDE any `allowed_rw` subpath listed in the profile,
/// - on a writeable filesystem,
/// - non-existent (we don't want to clobber anything).
pub fn probe_sandbox_enforces(
    profile_path: &Path,
    forbidden_target: &Path,
) -> Result<ProbeOutcome> {
    if !Path::new(SANDBOX_EXEC_PATH).exists() {
        return Err(Error::SandboxExecMissing);
    }
    if !profile_path.exists() {
        return Err(Error::SourceMissing(profile_path.to_path_buf()));
    }
    if forbidden_target.exists() {
        return Err(Error::DestinationExists(forbidden_target.to_path_buf()));
    }
    if let Some(parent) = forbidden_target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    // sh -c 'echo PROBE > <target>' — if the sandbox lets the redirect
    // succeed, the file gets created and we surface NotEnforcing.
    let out = Command::new(SANDBOX_EXEC_PATH)
        .arg("-f")
        .arg(profile_path)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!("echo PROBE > {}", forbidden_target.display()))
        .output()
        .map_err(|e| Error::io(profile_path, e))?;

    let outcome = if forbidden_target.exists() {
        // The shell created the file inside the sandbox — that means
        // the profile did NOT block the write.
        let _ = std::fs::remove_file(forbidden_target);
        ProbeOutcome::NotEnforcing
    } else if out.status.success() {
        // sh exited 0 but the file isn't there — bizarre, treat as
        // inconclusive rather than lying.
        ProbeOutcome::Inconclusive
    } else {
        ProbeOutcome::Enforcing
    };
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn probe_reports_enforcing_for_correct_profile() {
        use crate::sbpl::{generate_sbpl, write_sbpl, SbplParams};

        let tmp = tempfile::tempdir().expect("tmpdir");
        std::fs::create_dir(tmp.path().join("rootfs")).expect("mkdir");
        let rootfs = std::fs::canonicalize(tmp.path().join("rootfs")).expect("canonicalize");

        let params = SbplParams {
            session_id: "s_probe".into(),
            rootfs: rootfs.clone(),
            allowed_rw: vec![rootfs.clone()],
            allowed_ro: Vec::new(),
            network_outbound: false,
            native_ax_allowed: false,
        };
        let prof = tmp.path().join("p.sb");
        write_sbpl(&prof, &generate_sbpl(&params)).expect("write profile");

        // forbidden_target is OUTSIDE rootfs and must NOT yet exist.
        let target = std::fs::canonicalize(tmp.path())
            .expect("canonicalize tmp")
            .join("forbidden_probe.txt");
        let outcome = probe_sandbox_enforces(&prof, &target).expect("probe");
        assert_eq!(outcome, ProbeOutcome::Enforcing);
        assert!(!target.exists(), "probe should clean up the (failed) write");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn probe_reports_not_enforcing_when_profile_allows_forbidden_path() {
        use crate::sbpl::{generate_sbpl, write_sbpl, SbplParams};

        let tmp = tempfile::tempdir().expect("tmpdir");
        // Allow the entire tmp dir — the "forbidden_target" inside it is
        // actually allowed → probe should report NotEnforcing.
        let allow_root = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let params = SbplParams {
            session_id: "s_theatre".into(),
            rootfs: allow_root.clone(),
            allowed_rw: vec![allow_root.clone()],
            allowed_ro: Vec::new(),
            network_outbound: false,
            native_ax_allowed: false,
        };
        let prof = allow_root.join("p.sb");
        write_sbpl(&prof, &generate_sbpl(&params)).expect("write profile");
        let target = allow_root.join("writable.txt");
        let outcome = probe_sandbox_enforces(&prof, &target).expect("probe");
        assert_eq!(outcome, ProbeOutcome::NotEnforcing);
        assert!(!target.exists(), "probe should clean up its own probe file");
    }
}
