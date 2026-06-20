//! SPEC §12 U7 — clipboard roundtrip integration test.
//!
//! Writes a string, reads it back, asserts equality + history advancement.
//! Gated behind macOS AND `OFA_E2E_CLIPBOARD=1` because the test mutates the
//! user's REAL system pasteboard — running unattended would clobber their
//! clipboard. Set the env var when running on a developer machine to
//! validate U7 end-to-end.

#![cfg(target_os = "macos")]

use native_control::{
    clipboard::{history, read_string, write_string, ClipboardCache},
    PrivacyPolicy, RedactionEngine,
};

fn enabled() -> bool {
    std::env::var("OFA_E2E_CLIPBOARD").is_ok()
}

#[tokio::test]
async fn write_then_read_returns_same_string() {
    if !enabled() {
        eprintln!("OFA_E2E_CLIPBOARD not set; skipping pasteboard mutation test");
        return;
    }
    let cache = ClipboardCache::new();
    let engine = RedactionEngine::new();
    write_string(&cache, "one-for-all clipboard roundtrip")
        .await
        .expect("write_string");
    let got = read_string(&cache, &engine).await.expect("read_string");
    assert_eq!(got.as_deref(), Some("one-for-all clipboard roundtrip"));
}

#[tokio::test]
async fn redaction_pattern_hides_text_at_read() {
    if !enabled() {
        eprintln!("OFA_E2E_CLIPBOARD not set; skipping pasteboard mutation test");
        return;
    }
    let cache = ClipboardCache::new();
    let engine = RedactionEngine::new();
    engine.install(&PrivacyPolicy {
        redact_patterns: vec![r"sk-[A-Za-z0-9]{10,}".into()],
        app_blocklist: vec![],
    });
    write_string(&cache, "API key is sk-aaaaaaaaaaaaaa")
        .await
        .expect("write_string");
    let got = read_string(&cache, &engine).await.expect("read_string");
    assert_eq!(got, None);
}

#[tokio::test]
async fn history_grows_across_writes() {
    if !enabled() {
        eprintln!("OFA_E2E_CLIPBOARD not set; skipping pasteboard mutation test");
        return;
    }
    let cache = ClipboardCache::new();
    let engine = RedactionEngine::new();
    let initial = history(&cache, &engine).await.expect("history0").len();
    write_string(&cache, "first").await.expect("w1");
    write_string(&cache, "second").await.expect("w2");
    let after = history(&cache, &engine).await.expect("history1");
    assert!(
        after.len() >= initial + 2,
        "history should grow by at least 2 (initial={initial}, after={})",
        after.len()
    );
    assert_eq!(after.last().unwrap().text.as_deref(), Some("second"));
}
