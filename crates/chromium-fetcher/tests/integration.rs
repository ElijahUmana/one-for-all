//! Integration tests for `chromium-fetcher`.
//!
//! Most coverage is in unit tests inside each module. This file holds the
//! tests that need a temp install root or that are gated behind live network.

use chromium_fetcher::{fetch, Channel, FetchOptions, Platform};

#[test]
fn manifest_cache_present_or_skipped_quietly() {
    // Simply parses the cache dir helper; the real schema check happens in
    // the manifest module's unit tests.
    let _ = chromium_fetcher::manifest::cache_dir();
}

#[tokio::test]
async fn fetch_errors_clearly_when_cache_missing() {
    // Force a bogus install root and rely on the *real* cache being absent in
    // CI: this test is mostly a smoke check that fetch surfaces a useful
    // error rather than panicking.
    let tmp = tempfile::tempdir().unwrap();
    let opts = FetchOptions {
        channel: Channel::Stable,
        install_root: Some(tmp.path().join("install")),
        platform: Some(Platform::Linux64), // deterministic
        ..FetchOptions::default()
    };
    // Whether or not the user's home cache is present, calling fetch with a
    // bogus version forces the known-good lookup path; we just want to see
    // an Err result, not a panic.
    let result = fetch(Some("0.0.0.0-does-not-exist"), &opts).await;
    assert!(result.is_err(), "expected error for unknown version");
}

#[tokio::test]
#[ignore = "live network — set OFA_LIVE_FETCH=1 to run"]
async fn live_fetch_stable_macarm64() {
    if std::env::var("OFA_LIVE_FETCH").ok().as_deref() != Some("1") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let opts = FetchOptions {
        install_root: Some(tmp.path().to_path_buf()),
        ..FetchOptions::default()
    };
    let bin = fetch(None, &opts).await.expect("live fetch");
    assert!(bin.exists(), "binary should exist at {}", bin.display());
}
