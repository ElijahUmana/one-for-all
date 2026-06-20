//! Integration test: verify the generated sandbox profile actually blocks a
//! forbidden write under `/usr/bin/sandbox-exec`.
//!
//! This test runs ONLY on macOS and is gated on `sandbox-exec` being
//! present (every macOS host has it). On non-macOS the test compiles to a
//! no-op so `cargo check --workspace` works on CI runners.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use sandbox::sbpl::{generate_sbpl, write_sbpl, SbplParams};

#[test]
fn sandbox_blocks_write_outside_rootfs() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    // sandbox-exec resolves symlinks before matching subpaths, and macOS
    // tempdirs live under `/var/folders/…` which is a symlink to
    // `/private/var/folders/…`. Canonicalize so the profile lists real
    // paths — this matches what the broker does in production.
    let rootfs_raw = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs_raw).expect("mkdir rootfs");
    let rootfs = std::fs::canonicalize(&rootfs_raw).expect("canonicalize rootfs");
    let tmp_real = std::fs::canonicalize(tmp.path()).expect("canonicalize tmp");

    let profile_path = tmp.path().join("test.sb");
    let params = SbplParams {
        session_id: "s_test".into(),
        rootfs: rootfs.clone(),
        allowed_rw: vec![rootfs.clone()],
        allowed_ro: Vec::new(),
        // Network not relevant to this test.
        network_outbound: false,
        native_ax_allowed: false,
    };
    let text = generate_sbpl(&params);
    write_sbpl(&profile_path, &text).expect("write profile");

    // Sanity: forbidden_target lives outside the rootfs but inside the
    // tempdir, so this test never touches anything the user cares about.
    let forbidden_target = tmp_real.join("forbidden_outside_rootfs.txt");

    // Run `/bin/sh -c 'echo nope > <forbidden_target>'` under the sandbox.
    // Expectation: the redirection fails because file-write* is denied
    // outside the rootfs.
    let out = Command::new("/usr/bin/sandbox-exec")
        .arg("-f")
        .arg(&profile_path)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!("echo nope > {}", forbidden_target.display()))
        .output()
        .expect("spawn sandbox-exec");

    assert!(
        !forbidden_target.exists(),
        "sandbox profile failed: forbidden target was created at {}",
        forbidden_target.display()
    );
    assert!(
        !out.status.success(),
        "sandbox-exec exit was {} but should have blocked the write\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn sandbox_allows_write_inside_rootfs() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let rootfs_raw = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs_raw).expect("mkdir rootfs");
    // Same canonicalization rationale as above.
    let rootfs = std::fs::canonicalize(&rootfs_raw).expect("canonicalize rootfs");

    let profile_path = tmp.path().join("ok.sb");
    let params = SbplParams {
        session_id: "s_ok".into(),
        rootfs: rootfs.clone(),
        allowed_rw: vec![rootfs.clone()],
        allowed_ro: Vec::new(),
        network_outbound: false,
        native_ax_allowed: false,
    };
    write_sbpl(&profile_path, &generate_sbpl(&params)).expect("write");

    let allowed_target = rootfs.join("inside.txt");
    let out = Command::new("/usr/bin/sandbox-exec")
        .arg("-f")
        .arg(&profile_path)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!("echo ok > {}", allowed_target.display()))
        .output()
        .expect("spawn");

    assert!(
        out.status.success(),
        "expected sandbox to allow write inside rootfs; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(&allowed_target).expect("read written file");
    assert_eq!(body.trim(), "ok");
}
