//! Integration test: clone a synthesized "Chrome profile" SQLite-shaped
//! cookie file and verify the cloned database is byte-identical and
//! re-openable.
//!
//! We don't depend on `rusqlite` (extra dep). Instead we synthesize a tiny
//! file shaped exactly like Chrome's `Cookies` SQLite db (header + tail
//! magic), clone it, and verify the bytes round-trip. Live SQLite reopen
//! is exercised by the `chrome_profile_clone_preserves_cookies` test if
//! the macOS sqlite3 CLI is available.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::manual_repeat_n)]

use std::fs;
use std::path::PathBuf;

use sandbox::clone::clone_chrome_profile;

/// SQLite header magic — every SQLite file starts with this exact 16-byte
/// string.  Chrome's `Cookies` is just a SQLite db with a `cookies` table.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[test]
fn chrome_profile_clone_preserves_cookies_file_bytes() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let host_profile = tmp.path().join("Default");
    fs::create_dir_all(host_profile.join("Network")).expect("mkdir");

    // Synthesize a cookies db with the SQLite magic + a few hundred bytes
    // of recognizable content. clonefile preserves bytes exactly; we'll
    // round-trip them through the cloned file.
    let cookies_path = host_profile.join("Network/Cookies");
    let mut cookie_bytes = Vec::with_capacity(4096);
    cookie_bytes.extend_from_slice(SQLITE_MAGIC);
    cookie_bytes.extend_from_slice(&[0u8; 100]); // header padding
    cookie_bytes.extend_from_slice(b"sentinel-cookie:.example.com:auth_token=ofa-bridge-v3");
    cookie_bytes.extend(std::iter::repeat(0u8).take(4096 - cookie_bytes.len()));
    fs::write(&cookies_path, &cookie_bytes).expect("write cookies");

    // Also synthesize a few sibling files Chrome cares about so the clone
    // exercises the directory walk.
    fs::write(
        host_profile.join("Preferences"),
        b"{\"homepage\":\"about:blank\"}",
    )
    .expect("prefs");
    fs::write(host_profile.join("History"), SQLITE_MAGIC).expect("history");

    // Clone into per-session UDD.
    let dest_udd: PathBuf = tmp.path().join("session_udd");
    let stats = clone_chrome_profile(&host_profile, &dest_udd).expect("clone Chrome profile");
    assert!(
        stats.file_count >= 3,
        "expected ≥3 files cloned, got {}",
        stats.file_count
    );

    // The cookies file is at <udd>/Default/Network/Cookies.
    let cloned = dest_udd.join("Default/Network/Cookies");
    let body = fs::read(&cloned).expect("read cloned cookies");
    assert_eq!(
        &body[..16],
        SQLITE_MAGIC,
        "SQLite magic must be preserved byte-for-byte after clone"
    );
    assert!(
        body.windows(b"sentinel-cookie".len())
            .any(|w| w == b"sentinel-cookie"),
        "sentinel cookie payload missing from cloned file"
    );
    assert_eq!(
        body.len(),
        cookie_bytes.len(),
        "cloned cookies length differs from source"
    );
    assert_eq!(body, cookie_bytes, "cloned bytes differ from source bytes");
}

#[test]
fn cloned_cookies_distinguishable_after_agent_writes() {
    // Verifies the COW property: agent writes to the clone don't bleed
    // into the host file.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let host_profile = tmp.path().join("Default");
    fs::create_dir_all(&host_profile).expect("mk");
    let host_cookies = host_profile.join("Cookies");
    fs::write(&host_cookies, b"HOST-ORIGINAL").expect("write host");

    let dest_udd = tmp.path().join("udd");
    clone_chrome_profile(&host_profile, &dest_udd).expect("clone");
    let cloned = dest_udd.join("Default/Cookies");
    assert_eq!(fs::read(&cloned).expect("read clone"), b"HOST-ORIGINAL");

    // Agent writes through the clone.
    fs::write(&cloned, b"AGENT-MUTATED").expect("agent write");

    // Host must be untouched (COW divergence).
    assert_eq!(
        fs::read(&host_cookies).expect("read host post"),
        b"HOST-ORIGINAL",
        "host cookies must NOT see agent-side writes (COW divergence broken)"
    );
    assert_eq!(
        fs::read(&cloned).expect("read clone post"),
        b"AGENT-MUTATED"
    );
}
