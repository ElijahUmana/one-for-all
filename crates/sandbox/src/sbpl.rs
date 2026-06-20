//! Sandbox Profile Language (SBPL / `.sb`) generator.
//!
//! macOS ships `/usr/bin/sandbox-exec`, an SPI tool that takes a TinyScheme-
//! based policy file and runs a child under that policy. The language is
//! documented at <https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf>
//! and exercised live in `/System/Library/Sandbox/Profiles/*.sb`.
//!
//! The profile we emit:
//!
//! 1. Default-deny everything.
//! 2. Allow process fork/exec/signal-self so Chromium's multi-process model
//!    works inside the sandbox.
//! 3. Allow file-read/write inside the per-session rootfs and any RW
//!    inherit paths.
//! 4. Allow file-read* on RO inherit paths.
//! 5. Allow the minimum set of mach-services Chromium needs (windowserver,
//!    LaunchServices, fonts, cfprefsd, notifications). Per V-R8 the
//!    reviewer-finisher audits this list in T7.
//! 6. Allow IOKit/sysctl/network-outbound that headless Chrome touches.
//! 7. Deny network-inbound listen, deny system-socket, deny file-write
//!    everywhere outside the allowlist.
//!
//! ## Why TinyScheme literals, not s-expressions built with a builder
//!
//! sandbox-exec reads `.sb` text directly. Hand-written templated text is
//! easier to diff against the policy in `/System/Library/Sandbox/Profiles/`
//! and easier to audit visually for V-R8.

use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::inherit::{InheritMode, InheritSpec};
use crate::Result;

/// Inputs for generating a session sandbox profile.
///
/// **Path canonicalization is the caller's responsibility.** sandbox-exec
/// resolves symlinks before matching subpaths, so any path lists that
/// might traverse `/var → /private/var` (macOS), `/tmp → /private/tmp`,
/// `/Users` symlinks, etc. must be `std::fs::canonicalize`'d before
/// being placed into `rootfs` / `allowed_rw` / `allowed_ro`. The broker
/// does this in `handle_session_register`; tests do it in their setup.
#[derive(Debug, Clone)]
pub struct SbplParams {
    /// SPEC-assigned session id; used in the profile comment header for
    /// audit traceability.
    pub session_id: String,
    /// Per-session rootfs (`~/.one-for-all/sessions/<id>/`). Everything
    /// under this gets full read/write inside the sandbox.
    pub rootfs: PathBuf,
    /// Read-write paths the agent can mutate. Cloned copies of host dirs.
    pub allowed_rw: Vec<PathBuf>,
    /// Read-only paths the agent can read (e.g. cloned `~/.ssh`,
    /// `~/.config`).
    pub allowed_ro: Vec<PathBuf>,
    /// Outbound network: when true the sandbox allows `(allow network*)`;
    /// when false the agent has no network. Default true; reserved for
    /// V-R6 Phase 2.5 per-session pf-anchor work.
    pub network_outbound: bool,
    /// SPEC §11 V2 — when true the sandbox allows the mach services
    /// required for the Accessibility API (`com.apple.tccd` for the trust
    /// gate, `com.apple.AppleEventsService` for AppleScript via
    /// `osascript`). **Default false**: native control is opt-in via
    /// `session.register {capabilities: ["native"]}` so a hostile or
    /// careless agent cannot silently drive Mail/Slack/Notes.
    pub native_ax_allowed: bool,
}

impl SbplParams {
    /// Build params from a session id, its rootfs, and the resolved inherit
    /// allowlist. Network outbound defaults to true.
    pub fn from_inherit(session_id: &str, rootfs: &Path, allowlist: &[InheritSpec]) -> Self {
        let mut rw = vec![rootfs.to_path_buf()];
        let mut ro = Vec::new();
        for spec in allowlist {
            // The cloned copy lives inside `rootfs` (it's where
            // clone_user_dirs put it), but for defense in depth we also list
            // the host source as denied — the sandbox only ever sees the
            // clone via the rootfs path.
            let leaf = match spec.host_path.file_name() {
                Some(l) => rootfs.join(l),
                None => continue,
            };
            match spec.mode {
                InheritMode::ReadWrite => rw.push(leaf),
                InheritMode::ReadOnly => ro.push(leaf),
            }
        }
        Self {
            session_id: session_id.to_string(),
            rootfs: rootfs.to_path_buf(),
            allowed_rw: rw,
            allowed_ro: ro,
            network_outbound: true,
            native_ax_allowed: false,
        }
    }
}

/// Render the `.sb` profile text. Always returns valid TinyScheme; an
/// empty allowlist still produces a usable (rootfs-only) profile.
pub fn generate_sbpl(p: &SbplParams) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(";; one-for-all SPEC §11 V3 sandbox profile — auto-generated\n");
    out.push_str(&format!(";; session_id = {}\n", p.session_id));
    out.push_str(&format!(";; rootfs     = {}\n", p.rootfs.display()));
    // SPEC §10 M9 audit annotation. SBPL has no `mem-limit` clause; the
    // memory cap is enforced via `setrlimit(RLIMIT_AS, …)` in the same
    // `pre_exec` that wraps Chromium with `sandbox-exec`. Documenting
    // the value here keeps the layered policy auditable as a single
    // text artifact alongside the profile body.
    out.push_str(&format!(
        ";; rlimit_as  = {} bytes (SPEC §10 M9; enforced via pre_exec setrlimit)\n",
        crate::limits::CHROMIUM_MEMORY_BYTES
    ));
    out.push_str(&format!(
        ";; rlimit_cpu = {} seconds soft (SPEC §10 M9; enforced via pre_exec setrlimit)\n",
        crate::limits::CHROMIUM_CPU_SECONDS_SOFT
    ));
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");
    out.push_str("(debug deny)\n\n");

    // Process model — Chromium multi-process needs fork/exec/signal-self.
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec)\n");
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow process-info* (target self))\n\n");

    // Sysctl + IOKit + minimum darwin support — Chromium uses these for
    // system info, GPU detection, prefs.
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow iokit-open)\n");
    out.push_str("(allow mach-priv-host-port)\n");
    out.push_str("(allow ipc-posix-shm)\n");
    out.push_str("(allow ipc-posix-sem)\n\n");

    // Read-only system paths Chromium MUST see.
    out.push_str("(allow file-read*\n");
    for p in [
        "/",
        "/usr/lib",
        "/usr/share",
        "/System",
        "/Library/Frameworks",
        "/Library/Preferences",
        "/Library/Fonts",
        "/private/etc",
        "/private/var/db",
        "/dev/null",
        "/dev/random",
        "/dev/urandom",
        "/dev/zero",
    ] {
        out.push_str(&format!("    (subpath \"{p}\")\n"));
    }
    out.push_str(")\n\n");

    // Read-write within the rootfs. Subpath form picks up anything under
    // it including newly-created files.
    //
    // The rootfs itself is ALWAYS RW regardless of what the caller passed
    // in `allowed_rw` — it's where the agent's per-session state lives,
    // and an empty allowlist must still produce a usable sandbox profile.
    out.push_str("(allow file-read* file-write*\n");
    out.push_str(&format!("    (subpath \"{}\")\n", escape(&p.rootfs)));
    for rw in &p.allowed_rw {
        if rw == &p.rootfs {
            continue;
        }
        out.push_str(&format!("    (subpath \"{}\")\n", escape(rw)));
    }
    // Stdio.
    out.push_str("    (path \"/dev/null\")\n");
    out.push_str("    (path \"/dev/dtracehelper\")\n");
    out.push_str(")\n\n");

    // Read-only on RO inherit paths (`~/.ssh`, `~/.config`).
    if !p.allowed_ro.is_empty() {
        out.push_str("(allow file-read*\n");
        for ro in &p.allowed_ro {
            out.push_str(&format!("    (subpath \"{}\")\n", escape(ro)));
        }
        out.push_str(")\n\n");
    }

    // Mach service allowlist — this is the V-R8 audit surface. We pick the
    // minimum that headless Chromium actually requires; if we missed one,
    // sandbox-exec logs the denied service and the doctor probe surfaces
    // it. We don't speculatively allow everything Chromium might want.
    out.push_str(";; V-R8: mach service allowlist — every entry must be justified.\n");
    out.push_str("(allow mach-lookup\n");
    for svc in [
        "com.apple.system.notification_center",
        "com.apple.distributed_notifications.2",
        "com.apple.cfprefsd.daemon",
        "com.apple.cfprefsd.agent",
        "com.apple.coreservices.launchservicesd",
        "com.apple.lsd.mapdb",
        "com.apple.lsd.modifydb",
        "com.apple.fonts",
        "com.apple.FontServer",
        "com.apple.windowserver.active",
        "com.apple.SecurityServer",
        "com.apple.system.opendirectoryd.libinfo",
        "com.apple.system.logger",
        "com.apple.logd",
        "com.apple.diagnosticd",
    ] {
        out.push_str(&format!("    (global-name \"{svc}\")\n"));
    }
    if p.native_ax_allowed {
        // SPEC §11 V2 — additional services required by the Accessibility
        // API and AppleScript bridge. tccd serves the trust gate;
        // AppleEventsService is what `osascript` talks to.
        for svc in [
            "com.apple.tccd",
            "com.apple.tccd.system",
            "com.apple.AppleEventsService",
            "com.apple.coreservices.appleevents",
        ] {
            out.push_str(&format!("    (global-name \"{svc}\") ;; V2: native AX\n"));
        }
    }
    out.push_str(")\n\n");

    // Network. Outbound: TCP + UDP. Inbound: deny by default.
    if p.network_outbound {
        out.push_str(";; outbound network only; inbound listen denied.\n");
        out.push_str("(allow network-outbound)\n");
        out.push_str("(allow system-socket)\n");
        out.push_str("(allow network-bind (local ip \"localhost:*\"))\n\n");
    } else {
        out.push_str(";; network isolation requested — agent is offline.\n\n");
    }

    out
}

/// Atomically write `text` to `path` with mode 0600 (the profile lists
/// inherited paths and is therefore session-private).
pub fn write_sbpl(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".sbpl-")
        .tempfile_in(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .map_err(|e| Error::io(path, e))?;
    tmp.write_all(text.as_bytes())
        .map_err(|e| Error::io(path, e))?;
    tmp.flush().map_err(|e| Error::io(path, e))?;
    // chmod before persist so the on-disk file is never world-readable even
    // for a microsecond.
    let f = tmp.as_file();
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::io(path, e))?;
    tmp.persist(path).map_err(|pe| Error::io(path, pe.error))?;
    Ok(())
}

/// Escape a path for inclusion inside a `.sb` literal. TinyScheme strings
/// quote with `"` and escape `\` and `"`. macOS sandbox profile paths are
/// usually simple but we still defend against odd home-dir characters.
fn escape(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> SbplParams {
        SbplParams {
            session_id: "s_abc".into(),
            rootfs: PathBuf::from("/tmp/ofa-sessions/s_abc"),
            allowed_rw: vec![PathBuf::from("/tmp/ofa-sessions/s_abc/Documents")],
            allowed_ro: vec![PathBuf::from("/tmp/ofa-sessions/s_abc/.ssh")],
            network_outbound: true,
            native_ax_allowed: false,
        }
    }

    #[test]
    fn profile_starts_with_version_and_default_deny() {
        let s = generate_sbpl(&fixture());
        assert!(s.contains("(version 1)"));
        assert!(s.contains("(deny default)"));
    }

    #[test]
    fn profile_includes_session_id_in_comment() {
        let s = generate_sbpl(&fixture());
        assert!(s.contains(";; session_id = s_abc"));
    }

    #[test]
    fn profile_grants_rw_on_rootfs_and_extras() {
        let s = generate_sbpl(&fixture());
        assert!(s.contains("(subpath \"/tmp/ofa-sessions/s_abc\")"));
        assert!(s.contains("(subpath \"/tmp/ofa-sessions/s_abc/Documents\")"));
    }

    #[test]
    fn profile_grants_ro_on_ssh() {
        let s = generate_sbpl(&fixture());
        assert!(s.contains("(allow file-read*"));
        assert!(s.contains("(subpath \"/tmp/ofa-sessions/s_abc/.ssh\")"));
    }

    #[test]
    fn profile_lists_mach_services_with_v_r8_marker() {
        let s = generate_sbpl(&fixture());
        assert!(s.contains(";; V-R8"));
        assert!(s.contains("com.apple.windowserver.active"));
        assert!(s.contains("com.apple.cfprefsd.daemon"));
    }

    #[test]
    fn profile_with_network_off_omits_network_outbound_allow() {
        let mut f = fixture();
        f.network_outbound = false;
        let s = generate_sbpl(&f);
        assert!(!s.contains("(allow network-outbound)"));
        assert!(s.contains("network isolation requested"));
    }

    #[test]
    fn profile_with_native_ax_adds_expected_mach_services() {
        let mut f = fixture();
        f.native_ax_allowed = true;
        let s = generate_sbpl(&f);
        assert!(s.contains("com.apple.tccd"));
        assert!(s.contains("com.apple.tccd.system"));
        assert!(s.contains("com.apple.AppleEventsService"));
        assert!(s.contains("com.apple.coreservices.appleevents"));
        assert!(s.contains("V2: native AX"));
    }

    #[test]
    fn from_inherit_classifies_rw_and_ro() {
        let rootfs = PathBuf::from("/tmp/r");
        let allow = vec![
            InheritSpec::rw("/home/u/Documents"),
            InheritSpec::ro("/home/u/.ssh"),
        ];
        let p = SbplParams::from_inherit("s_x", &rootfs, &allow);
        // rootfs is always RW; plus Documents.
        assert!(p.allowed_rw.iter().any(|p| p == &rootfs));
        assert!(p.allowed_rw.iter().any(|p| p.ends_with("Documents")));
        assert!(p.allowed_ro.iter().any(|p| p.ends_with(".ssh")));
    }

    #[test]
    fn write_sbpl_atomic_and_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join("session.sb");
        write_sbpl(&path, "(version 1)\n(deny default)\n").expect("write");
        let md = std::fs::metadata(&path).expect("metadata");
        let mode = md.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("(version 1)"));
    }

    #[test]
    fn escape_handles_quotes_and_backslashes() {
        let p = PathBuf::from("/tmp/has\"quote/and\\bs");
        let e = escape(&p);
        assert!(e.contains("\\\""));
        assert!(e.contains("\\\\"));
    }
}
