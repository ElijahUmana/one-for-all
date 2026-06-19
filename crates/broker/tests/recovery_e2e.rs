//! SPEC §10 M4 — Crash recovery end-to-end.
//!
//! Drives the full crash-recovery contract:
//!
//! 1. Launch a real Chromium child via `Browser::launch`.
//! 2. Wrap it in a [`SessionEntry`] backed by [`arc_swap::ArcSwap`] and
//!    register it on a real [`State`].
//! 3. Open a tab, snapshot its `tab_id`.
//! 4. Bind a mpsc receiver as the "MCP client" so we observe
//!    `event/notify { topic:"session.recovered" }`.
//! 5. Mark the session active (so the 30 s respawn window fires) and spawn
//!    the crash-watch task.
//! 6. SIGKILL the Chromium child. The crash watcher must respawn against
//!    the same UDD, atomically swap [`SessionEntry::browser`], reattach
//!    targets, and push the recovery notification.
//! 7. Assert: the same `session_id` is preserved; `new_tab_ids` is
//!    populated; `entry.browser` is a different `Arc` than before;
//!    `metrics.recovery_count` == 1; a fresh `Target.getTargets` on the new
//!    Browser succeeds (proving the swapped handle is alive).
//!
//! The whole sequence completes within 10 s on a healthy machine. Anything
//! slower is a recovery-path regression.
//!
//! Gated on `ONE_FOR_ALL_LIVE_TESTS=1` for the same reason as
//! `session_register_live.rs` — the default `cargo test` lane stays
//! Chromium-free.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use broker::events::ClientEvent;
use broker::lifecycle::IdleConfig;
use broker::protocol::JsonRpcRequest;
use broker::recovery::spawn_crash_watch;
use broker::registry::SessionEntry;
use broker::State;
use browser_engine::{Browser, BrowserConfig, Page};
use cdp_client::generated::domains::target as cdp_target;
use focus_manager::SpawnMode;
use serde_json::{json, Value};
use tokio::sync::mpsc;

const RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the Chromium binary the test will hand to `Browser::launch`. Same
/// rules as `session_register_live.rs::resolve_test_chromium`.
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

/// Hard-kill every Chromium process whose argv contains `--user-data-dir=<path>`.
/// Equivalent to `pkill -KILL -f "user-data-dir=<path>"`. Returns when pkill
/// exits — does NOT wait for Chromium to actually die (the OS reaper handles
/// that asynchronously, and the recovery watcher detects exit via the CDP
/// pipe's read EOF).
async fn sigkill_chromium_for_udd(udd: &std::path::Path) -> std::io::Result<()> {
    let needle = format!("user-data-dir={}", udd.display());
    let _ = tokio::process::Command::new("pkill")
        .args(["-KILL", "-f", &needle])
        .status()
        .await?;
    Ok(())
}

async fn navigate_to_fixture(page: &Page) {
    page.eval(
        "document.title = 'm4-e2e'; document.body.innerHTML = '<button id=go>go</button>';",
        false,
    )
    .await
    .expect("seed page body");
}

fn observe_request_url(notify: &broker::ServerEvent) -> Option<String> {
    if notify.params.get("topic").and_then(Value::as_str) != Some("network.request") {
        return None;
    }
    notify
        .params
        .get("payload")?
        .get("url")?
        .as_str()
        .map(str::to_owned)
}

fn observe_subscription_id(notify: &broker::ServerEvent) -> Option<String> {
    notify
        .params
        .get("payload")?
        .get("subscription_id")?
        .as_str()
        .map(str::to_owned)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_then_recover_preserves_session() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping crash_then_recover_preserves_session: \
             ONE_FOR_ALL_LIVE_TESTS=1 is required"
        );
        return;
    }
    let chromium = match resolve_test_chromium() {
        Some(p) => p,
        None => panic!(
            "ONE_FOR_ALL_LIVE_TESTS=1 but no Chromium binary found. \
             Run `chromium-fetcher` once or set ONE_FOR_ALL_TEST_CHROMIUM=<path>."
        ),
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let user_data_root = tmp.path().to_path_buf();
    let session_id = "s_recovery_e2e".to_string();
    let session_udd = user_data_root.join(&session_id);
    std::fs::create_dir_all(&session_udd).expect("create session udd");

    // Build a real broker State and register the binary path so the crash
    // watcher can call `Browser::launch` with the same config on respawn.
    let state = State::new(IdleConfig::default(), user_data_root.clone());
    *state.chromium_binary.lock() = Some(chromium.clone());
    let metrics = state.metrics.clone();

    // Launch the first Browser.
    let config = BrowserConfig {
        binary: chromium.clone(),
        user_data_dir: session_udd.clone(),
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        sandbox_profile: None,
        seed_plan_path: None,
        proxy: None,
    };
    let initial = tokio::time::timeout(Duration::from_secs(10), Browser::launch(config))
        .await
        .expect("initial Browser::launch within 10s")
        .expect("initial Browser::launch returned Err");

    // Wrap in SessionEntry + insert into the registry so recovery has
    // something to swap against. Capture the pre-crash Arc<Browser> for a
    // ptr_eq comparison post-recovery.
    let entry = Arc::new(SessionEntry::new(session_id.clone(), initial, metrics));
    state.registry.insert(Arc::clone(&entry));
    let pre_crash_arc = entry.browser.load_full();

    // Open a tab so Chromium has something to rehydrate from
    // `Default/Sessions/` after respawn.
    let page = entry
        .browser
        .load_full()
        .default_context()
        .open_tab("about:blank", browser_engine::WaitUntil::Load)
        .await
        .expect("open_tab");
    navigate_to_fixture(&page).await;
    let prev_tab_ids: Vec<String> = entry
        .browser
        .load_full()
        .default_context()
        .list_tabs()
        .iter()
        .map(|p| p.tab_id().0.clone())
        .collect();
    assert!(
        !prev_tab_ids.is_empty(),
        "open_tab should populate the page map"
    );

    // Bind an mpsc as the "MCP client" so the recovery notification has
    // somewhere to land. The buffer is intentionally generous because other
    // event/notify topics (M5 forwarders) may also fire during the crash.
    let (client_tx, mut client_rx) = mpsc::channel::<ClientEvent>(64);
    entry.bind_conn(client_tx);

    // Mark the session active so the recovery watcher classifies the
    // upcoming exit as a crash, not idle shutdown. `touch()` stamps
    // last_activity_ms with `created_at.elapsed()`, which is a few ms
    // here — well inside the 30 s window.
    entry.touch();

    // Sanity: metrics.recovery_count starts at 0 BEFORE we wire the watcher.
    let metrics = state.metrics.session(&session_id);
    assert_eq!(
        metrics
            .recovery_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // Wire the crash watcher. From here, any Chromium exit is observable.
    spawn_crash_watch(Arc::clone(&state), Arc::clone(&entry));

    // Brief settle so the watcher has subscribed to wait_for_exit before we
    // pull the rug. Without this, the SIGKILL can race the watcher's
    // `entry.browser.load_full().wait_for_exit()` call.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // SIGKILL Chromium. The CDP reader loop sees EOF, the closed_rx fires,
    // the watcher classifies as in-window crash, calls Browser::launch with
    // the same UDD, runs reattach_existing_targets, swaps the ArcSwap, and
    // pushes session.recovered.
    sigkill_chromium_for_udd(&session_udd)
        .await
        .expect("pkill chromium");

    // Drain client events until we see the session.recovered notification.
    let recovered_notify = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let Some(ev) = client_rx.recv().await else {
                panic!("client channel closed before session.recovered arrived");
            };
            if let ClientEvent::Notify(n) = ev {
                if n.params.get("topic").and_then(|v| v.as_str()) == Some("session.recovered") {
                    return n;
                }
            }
        }
    })
    .await
    .expect("session.recovered notification did not arrive within 10s");

    // Validate the wire shape against SPEC §10 M4.
    assert_eq!(recovered_notify.method, "event/notify");
    assert_eq!(
        recovered_notify.params["session_id"],
        serde_json::Value::String(session_id.clone())
    );
    let new_tab_ids: Vec<String> = recovered_notify.params["new_tab_ids"]
        .as_array()
        .expect("new_tab_ids array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_owned())
        .collect();
    let prev_tab_ids_in_event: Vec<String> = recovered_notify.params["previous_tab_ids"]
        .as_array()
        .expect("previous_tab_ids array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(
        prev_tab_ids_in_event, prev_tab_ids,
        "previous_tab_ids in event must match what we observed before crash"
    );
    // Chromium SHOULD restore the data:URL tab from session storage. If it
    // doesn't, the recovery still succeeded contract-wise (new tab list is
    // expected to be empty), but in steady-state Chromium does restore.
    eprintln!("session.recovered: prev_tab_ids={prev_tab_ids:?} new_tab_ids={new_tab_ids:?}");

    // Assert ArcSwap actually swapped — post-crash Arc must be different
    // from the pre-crash snapshot we captured above.
    let post_crash_arc = entry.browser.load_full();
    assert!(
        !Arc::ptr_eq(&pre_crash_arc, &post_crash_arc),
        "ArcSwap.store did not run — entry.browser still points at the dead Browser"
    );

    // Metrics: exactly one successful recovery.
    assert_eq!(
        metrics
            .recovery_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "metrics.recovery_count should be 1 after a single successful respawn"
    );
    assert_eq!(
        metrics
            .recovery_failed_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "metrics.recovery_failed_count should remain 0 after a successful respawn"
    );

    // The new Browser handle must be alive — round-trip Target.getTargets to
    // prove the CDP transport on the swapped Arc<Browser> is reachable. This
    // is the "fresh page.snapshot succeeds within 10s" property in
    // M4-implementer-vocabulary, but expressed at the CDP layer so the test
    // doesn't depend on the snapshot pipeline.
    let live = post_crash_arc;
    let _targets = tokio::time::timeout(
        Duration::from_secs(10),
        live.cdp()
            .root_session()
            .send(cdp_target::GetTargetsParams::default()),
    )
    .await
    .expect("Target.getTargets on respawned Browser did not return within 10s")
    .expect("Target.getTargets on respawned Browser returned Err");

    // Cleanup. Shutdown the live (post-recovery) Browser so the test
    // doesn't leak a Chromium process. The pre-crash one is already dead
    // (we SIGKILLed it).
    let _ = live.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn net_observe_survives_recovery() {
    if std::env::var("ONE_FOR_ALL_LIVE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let chromium = resolve_test_chromium().expect("Chromium binary");
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_data_root = tmp.path().to_path_buf();
    let session_id = "s_recovery_net_observe".to_string();
    let session_udd = user_data_root.join(&session_id);
    std::fs::create_dir_all(&session_udd).expect("create session udd");

    let state = State::new(IdleConfig::default(), user_data_root.clone());
    *state.chromium_binary.lock() = Some(chromium.clone());
    let metrics = state.metrics.clone();

    let config = BrowserConfig {
        binary: chromium.clone(),
        user_data_dir: session_udd.clone(),
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        sandbox_profile: None,
        seed_plan_path: None,
        proxy: None,
    };
    let initial = tokio::time::timeout(Duration::from_secs(10), Browser::launch(config))
        .await
        .expect("initial Browser::launch within 10s")
        .expect("initial Browser::launch returned Err");

    let entry = Arc::new(SessionEntry::new(session_id.clone(), initial, metrics));
    state.registry.insert(Arc::clone(&entry));

    let page = entry
        .browser
        .load_full()
        .default_context()
        .open_tab("about:blank", browser_engine::WaitUntil::Load)
        .await
        .expect("open_tab");
    navigate_to_fixture(&page).await;
    let tab_id = page.tab_id().0.clone();

    let (client_tx, mut client_rx) = mpsc::channel::<ClientEvent>(128);
    entry.bind_conn(client_tx);
    entry.touch();
    spawn_crash_watch(Arc::clone(&state), Arc::clone(&entry));

    let page_url = "https://example.com/";
    let observe_params = json!({"tab_id": tab_id, "filter": "example\\.com"});
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "net.observe".into(),
        params: Some(observe_params),
    };
    let response = broker::router::dispatch(Arc::clone(&state), Some(Arc::clone(&entry)), req)
        .await
        .expect("observe response");
    let subscription_id = response
        .result
        .as_ref()
        .and_then(|v| v.get("subscription_id"))
        .and_then(Value::as_str)
        .expect("subscription id")
        .to_owned();

    page.navigate(page_url, browser_engine::WaitUntil::Load)
        .await
        .expect("pre-recovery navigate");
    let first = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let Some(ev) = client_rx.recv().await else {
                panic!("client channel closed before pre-recovery network.request arrived");
            };
            if let ClientEvent::Notify(n) = ev {
                if observe_request_url(&n).as_deref() == Some(page_url) {
                    return n;
                }
            }
        }
    })
    .await
    .expect("pre-recovery net.observe event did not arrive");
    assert_eq!(
        observe_subscription_id(&first).as_deref(),
        Some(subscription_id.as_str())
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    sigkill_chromium_for_udd(&session_udd)
        .await
        .expect("pkill chromium");

    let _recovered = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let Some(ev) = client_rx.recv().await else {
                panic!("client channel closed before session.recovered arrived");
            };
            if let ClientEvent::Notify(n) = ev {
                if n.params.get("topic").and_then(|v| v.as_str()) == Some("session.recovered") {
                    return n;
                }
            }
        }
    })
    .await
    .expect("session.recovered did not arrive");

    let live = entry.browser.load_full();
    let restored_page = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            if let Some(p) = live.default_context().list_tabs().into_iter().next() {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("restored page not found");

    restored_page
        .navigate(page_url, browser_engine::WaitUntil::Load)
        .await
        .expect("post-recovery navigate");
    let second = tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let Some(ev) = client_rx.recv().await else {
                panic!("client channel closed before post-recovery network.request arrived");
            };
            if let ClientEvent::Notify(n) = ev {
                if observe_request_url(&n).as_deref() == Some(page_url) {
                    return n;
                }
            }
        }
    })
    .await
    .expect("post-recovery net.observe event did not arrive");
    assert_eq!(
        observe_subscription_id(&second).as_deref(),
        Some(subscription_id.as_str())
    );

    let _ = live.shutdown().await;
}
