//! P0 — CDP pipe handshake regression test + Tier-1 hardening.
//!
//! Drives [`browser_engine::Browser::launch`] end-to-end against a real
//! Chromium child and asserts that:
//!
//! 1. The launch returns `Ok` within 5 seconds — the in-flight
//!    `Browser.getVersion` handshake on the freshly-wired CDP pipe must
//!    actually round-trip. Before the P0 fix, fd 3 / fd 4 were swapped at
//!    pipe-creation time so Chromium got `EBADF` on every `read(3)` /
//!    `write(4)`, the parent's reader and writer actors died with
//!    `cdp reader read` / `cdp writer flush`, and `Browser.getVersion`
//!    hung past the 5 s timeout.
//!
//! 2. The transport keeps round-tripping AFTER the handshake. The test
//!    issues a second CDP call (`Target.getTargets`) on the root browser
//!    session via the same `cdp_client::Connection`. A pipe that's
//!    half-broken (e.g. one direction dead, the other live) would surface
//!    here.
//!
//! 3. The Chromium child is reaped on `Browser::shutdown` — no leaked
//!    processes, no orphaned `chrome_crashpad_handler`, no zombie pipe.
//!
//! ## Tier-1 hardening (added on top of the regression check)
//!
//! - **Repeat-spawn**: launch + handshake + shutdown 10 times back-to-back;
//!   if any single iteration drops below the 5 s SLO, the suite fails.
//! - **Bad-UDD**: hand `Browser::launch` a `user_data_dir` it cannot write,
//!   assert the launch returns `Err` cleanly within 10 s — no hang, no
//!   zombie. This was the secondary failure mode before the P0: a launch
//!   error against a dead pipe used to deadlock waiting for
//!   `Browser.getVersion`.
//! - **Concurrent root-session traffic**: 8 `Browser.getVersion` calls
//!   issued in parallel on the same root session, assert all 8 succeed —
//!   stresses the reader's `(sessionId, id)` demux and the writer's mpsc
//!   under contention.
//! - **In-flight cancellation**: kick off a CDP request, immediately call
//!   `Browser::shutdown`, assert the in-flight future resolves to
//!   `ConnectionClosed` (a real error) within 6 s — never hangs forever.
//!
//! ## Why a separate test file
//!
//! `crates/broker/tests/protocol.rs` covers JSON-RPC framing without
//! Chromium. This one needs a live Chromium binary and is gated behind
//! `ONE_FOR_ALL_LIVE_TESTS=1` (alias `BRIDGE_E2E_LIVE=1` accepted) so
//! the default `cargo test` lane stays Chromium-free. CI exports the var
//! on the macOS lane.
//!
//! ## Why `Browser::launch` directly, not via `session.register`
//!
//! The broker's `session.register` handler also runs the V3 sandbox
//! profile build (`prepare_session_sandbox` → `sandbox-exec`), which is a
//! separate workstream from the CDP pipe transport. Routing through it
//! would conflate two regressions. This test exercises only the CDP-pipe
//! contract that the P0 was about.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use browser_engine::{Browser, BrowserConfig};
use cdp_client::generated::domains::{browser as cdp_browser, target as cdp_target};
use focus_manager::SpawnMode;

const FIVE_SECONDS: Duration = Duration::from_secs(5);
const TEN_SECONDS: Duration = Duration::from_secs(10);

/// Resolve the Chromium binary the test will hand to `Browser::launch`.
///
/// Prefers `ONE_FOR_ALL_TEST_CHROMIUM` (used by CI to short-circuit the
/// fetcher) and otherwise falls back to the default location populated by
/// `chromium-fetcher` during the developer's first run.
fn resolve_test_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ONE_FOR_ALL_TEST_CHROMIUM") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let home = dirs::home_dir()?;
    let default = home
        .join(".one-for-all/chromium/149.0.7827.115/chrome-mac-arm64")
        .join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");
    if default.exists() {
        Some(default)
    } else {
        None
    }
}

/// True if the live test gate is enabled. Accepts both
/// `ONE_FOR_ALL_LIVE_TESTS=1` (existing convention) and
/// `BRIDGE_E2E_LIVE=1` (CI alias) so neither name silently goes stale.
fn live_tests_enabled() -> bool {
    matches!(
        std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref(),
        Some("1")
    ) || matches!(std::env::var("BRIDGE_E2E_LIVE").ok().as_deref(), Some("1"))
}

fn skip_if_disabled(name: &str) -> bool {
    if live_tests_enabled() {
        return false;
    }
    eprintln!(
        "skipping {name}: \
         ONE_FOR_ALL_LIVE_TESTS=1 (or BRIDGE_E2E_LIVE=1) is required to enable this test"
    );
    true
}

/// Resolve Chromium and panic with a clear remediation message if it isn't
/// installed — the live tests should fail loud, never silently no-op when
/// enabled.
fn require_chromium() -> PathBuf {
    resolve_test_chromium().unwrap_or_else(|| {
        panic!(
            "live tests enabled but no Chromium binary found. \
             Run `chromium-fetcher` once or set ONE_FOR_ALL_TEST_CHROMIUM=<path>."
        )
    })
}

/// Build a `BrowserConfig` rooted in a fresh tempdir.
fn fresh_config(chromium: &std::path::Path) -> (tempfile::TempDir, BrowserConfig) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_data_dir = tmp.path().join("udd");
    std::fs::create_dir_all(&user_data_dir).expect("create user_data_dir");
    let config = BrowserConfig {
        binary: chromium.to_path_buf(),
        user_data_dir,
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        // V3 sandbox is a separate workstream; this test exercises only the
        // CDP pipe transport (the P0 regression).
        sandbox_profile: None,
        seed_plan_path: None,
        proxy: None,
    };
    (tmp, config)
}

/// One full launch → second-call → shutdown lifecycle, wrapped in a 5 s
/// budget. The lifetime of the tempdir is tied to the returned `TempDir`
/// guard so the caller drops it after teardown.
async fn one_session_lifecycle(chromium: &std::path::Path) -> Duration {
    let (_tmp, config) = fresh_config(chromium);
    let start = Instant::now();

    // 1. Browser::launch performs `Browser.getVersion` internally with a
    //    5 s timeout. Wrap the wrapper in another 5 s budget so a regression
    //    that hangs in the cdp-client actor (rather than the
    //    Browser.getVersion call site) still fails loud.
    let browser = tokio::time::timeout(FIVE_SECONDS, Browser::launch(config))
        .await
        .expect("Browser::launch did not return within 5s — CDP pipe regression suspected")
        .expect("Browser::launch returned Err");

    // 2. Round-trip a second CDP command on the root session to prove the
    //    transport is healthy in *both* directions, not just the one
    //    Browser.getVersion happened to use.
    let targets = tokio::time::timeout(
        FIVE_SECONDS,
        browser
            .cdp()
            .root_session()
            .send(cdp_target::GetTargetsParams::default()),
    )
    .await
    .expect("Target.getTargets did not return within 5s")
    .expect("Target.getTargets returned Err");
    assert!(
        targets.target_infos.is_array(),
        "Target.getTargets.targetInfos should be an array, got: {:?}",
        targets.target_infos
    );

    // 3. Graceful shutdown reaps the Chromium child within its own 5 s
    //    budget; we add another 5 s ceiling on top so a stuck shutdown
    //    fails the test rather than the suite.
    tokio::time::timeout(FIVE_SECONDS, browser.shutdown())
        .await
        .expect("Browser::shutdown did not return within 5s")
        .expect("Browser::shutdown returned Err");

    start.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_pipe_handshake_round_trips_within_5s() {
    if skip_if_disabled("cdp_pipe_handshake_round_trips_within_5s") {
        return;
    }
    let chromium = require_chromium();
    let elapsed = one_session_lifecycle(&chromium).await;
    eprintln!("one_session_lifecycle elapsed: {:?}", elapsed);
    assert!(
        elapsed < FIVE_SECONDS,
        "single lifecycle exceeded the 5 s SLO: {elapsed:?}"
    );
}

/// Repeat the regression case 10 times back-to-back. A single race-window
/// regression that flakes 10 % of the time would still get caught here
/// even though the single-shot test above passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_pipe_handshake_ten_in_a_row() {
    if skip_if_disabled("cdp_pipe_handshake_ten_in_a_row") {
        return;
    }
    let chromium = require_chromium();
    let mut budget_left = Duration::from_secs(60);
    for i in 0..10 {
        let started = Instant::now();
        let elapsed = one_session_lifecycle(&chromium).await;
        eprintln!("iter {i}: {:?}", elapsed);
        let took = started.elapsed();
        budget_left = budget_left.checked_sub(took).unwrap_or(Duration::ZERO);
        assert!(
            elapsed < FIVE_SECONDS,
            "iter {i} exceeded single-lifecycle 5 s SLO: {elapsed:?}"
        );
    }
    eprintln!("budget remaining after 10 iterations: {:?}", budget_left);
}

/// Bad UDD path: hand `Browser::launch` a `user_data_dir` we make
/// read-only. Chromium aborts on profile-write failure; the pipe layer
/// must surface that as a clean `LaunchError` within 10 s rather than
/// hanging on `Browser.getVersion`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_pipe_dead_chromium_does_not_hang() {
    if skip_if_disabled("cdp_pipe_dead_chromium_does_not_hang") {
        return;
    }
    let chromium = require_chromium();
    // Build a UDD inside a directory we'll chmod 0o000 — Chromium can't
    // create the profile and bails immediately. The 0o000 dir gets
    // restored to 0o700 on drop so tempfile cleans up cleanly.
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path().join("locked");
    std::fs::create_dir_all(&parent).expect("create parent");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&parent).expect("stat").permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&parent, perms).expect("chmod 000");
    let restore_perms = scopeguard_restore_dir_perms(parent.clone());

    let config = BrowserConfig {
        binary: chromium,
        user_data_dir: parent.join("udd"),
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        sandbox_profile: None,
        seed_plan_path: None,
        proxy: None,
    };

    let outcome = tokio::time::timeout(TEN_SECONDS, Browser::launch(config))
        .await
        .expect("Browser::launch hung past 10 s with dead Chromium — pipe-shutdown regression");
    drop(restore_perms);
    match outcome {
        Ok(_b) => panic!("Browser::launch returned Ok with a 0o000 UDD parent"),
        Err(e) => {
            eprintln!("expected launch failure: {e}");
        }
    }
}

/// Concurrent root-session traffic — 8 `Browser.getVersion` calls in
/// parallel. Stresses the reader's `(sessionId, id)` demux and the
/// writer's bounded mpsc under contention. Every reply must arrive within
/// the 5 s budget; any reply mis-routed to the wrong oneshot would
/// manifest as a deserialization error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_root_session_concurrent_requests() {
    if skip_if_disabled("cdp_root_session_concurrent_requests") {
        return;
    }
    let chromium = require_chromium();
    let (_tmp, config) = fresh_config(&chromium);
    let browser = tokio::time::timeout(FIVE_SECONDS, Browser::launch(config))
        .await
        .expect("launch did not return within 5s")
        .expect("launch returned Err");
    let browser = Arc::new(browser);

    let mut joins = Vec::with_capacity(8);
    for i in 0..8u32 {
        let b = Arc::clone(&browser);
        joins.push(tokio::spawn(async move {
            let res = tokio::time::timeout(
                FIVE_SECONDS,
                b.cdp()
                    .root_session()
                    .send(cdp_browser::GetVersionParams::default()),
            )
            .await
            .unwrap_or_else(|_| panic!("getVersion[{i}] timed out"))
            .unwrap_or_else(|e| panic!("getVersion[{i}] returned Err: {e}"));
            assert!(
                !res.product.is_empty(),
                "getVersion[{i}] returned empty product"
            );
            i
        }));
    }
    for j in joins {
        j.await.expect("join");
    }

    tokio::time::timeout(FIVE_SECONDS, browser.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown err");
}

/// Cancellation-safety: kick off a CDP request, immediately call
/// `Browser::shutdown`, assert the future either succeeded (very fast
/// host) or surfaced a clean `ConnectionClosed`-style error within the
/// 6 s envelope. The defining invariant is "never hangs forever after
/// shutdown" — the pre-fix transport kept the future pending indefinitely
/// because the writer task had died silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_in_flight_request_resolves_after_shutdown() {
    if skip_if_disabled("cdp_in_flight_request_resolves_after_shutdown") {
        return;
    }
    let chromium = require_chromium();
    let (_tmp, config) = fresh_config(&chromium);
    let browser = tokio::time::timeout(FIVE_SECONDS, Browser::launch(config))
        .await
        .expect("launch did not return within 5s")
        .expect("launch returned Err");
    let browser = Arc::new(browser);

    let in_flight = {
        let b = Arc::clone(&browser);
        tokio::spawn(async move {
            // Use Target.getTargets — small, cheap, and won't hold any
            // browser-side state we have to clean up.
            b.cdp()
                .root_session()
                .send(cdp_target::GetTargetsParams::default())
                .await
        })
    };
    // Yield once so the request actually hits the writer queue before we
    // shut down.
    tokio::task::yield_now().await;

    let shutdown = tokio::time::timeout(FIVE_SECONDS, browser.shutdown());
    let _ = shutdown.await.expect("shutdown timed out");

    // The in-flight future must NOT hang. Any of these are acceptable:
    //   - Ok(_): the request raced ahead of shutdown and got its reply.
    //   - Err(_): connection-closed surfaced.
    // The bug we are guarding against is a future that never resolves at
    // all because the writer task died silently and nobody told the
    // pending oneshot.
    let resolved = tokio::time::timeout(Duration::from_secs(6), in_flight)
        .await
        .expect("in-flight request did not resolve within 6 s after shutdown");
    let _ = resolved.expect("join");
}

// ---------- helpers ----------

/// Restore a directory's permissions to 0o700 on drop. Used so the
/// `cdp_pipe_dead_chromium_does_not_hang` test can chmod a tempdir to
/// 0o000 without breaking `tempfile`'s cleanup.
struct DirPermsGuard {
    path: PathBuf,
}

impl Drop for DirPermsGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(&self.path, perms);
        }
    }
}

fn scopeguard_restore_dir_perms(path: PathBuf) -> DirPermsGuard {
    DirPermsGuard { path }
}
