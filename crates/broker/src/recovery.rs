//! SPEC §10 M4 — Crash recovery.
//!
//! Per-session Chromium SIGSEGV / OOM detection: subscribes to the child
//! exit channel exposed by [`browser_engine::Browser::wait_for_exit`]; if
//! the child terminates non-cleanly within 30 s of last activity we respawn
//! against the same `--user-data-dir`, swap the live `Browser` handle on
//! [`SessionEntry`] via [`arc_swap::ArcSwap`], reattach to all targets that
//! Chromium itself rehydrated from `Default/Sessions/`, re-arm the M5
//! console + exception forwarders and the M2 mutation observer, and surface
//! `event/notify { topic: "session.recovered", session_id, previous_tab_ids,
//! new_tab_ids }` to the bound MCP client.
//!
//! ## Threading
//!
//! Spawned as one detached task per registered session. The task loops:
//! `wait_for_exit → respawn+swap → re-subscribe to NEW Browser's exit
//! channel`, so a second crash is also caught. Bounded by
//! [`MAX_RESPAWN_ATTEMPTS`] consecutive failures so a systemically broken
//! UDD doesn't infinite-loop the runtime.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use ax_engine::mutation::OBSERVER_BOOTSTRAP_JS;
use browser_engine::{Browser, BrowserConfig, Page};

use crate::registry::SessionEntry;
use crate::ServerEvent;
use crate::State;

/// Minimum activity recency (ms) to trigger a respawn. SPEC §10 M4.
pub(crate) const RESPAWN_ACTIVITY_WINDOW_MS: u64 = 30_000;

/// Hard ceiling on consecutive failed respawn attempts before the watcher
/// gives up. A successful respawn resets the counter.
const MAX_RESPAWN_ATTEMPTS: u32 = 3;

/// Spawn the crash-watch task for `entry`. Runs until the Browser exits
/// cleanly (clean shutdown via lifecycle drain), the entry is dropped, or
/// recovery has hit [`MAX_RESPAWN_ATTEMPTS`] consecutive failures. The
/// returned [`tokio::task::JoinHandle`] is also pushed onto
/// [`State::recovery_handles`] so the broker shutdown drain can await it
/// (CR-1).
pub fn spawn_crash_watch(state: Arc<State>, entry: Arc<SessionEntry>) {
    let state_for_task = Arc::clone(&state);
    let handle = tokio::spawn(async move { run(state_for_task, entry).await });
    state.recovery_handles.lock().push(handle);
}

/// Pure helper, exposed for unit tests. Returns `true` when `last_activity_ms`
/// indicates traffic within the last [`RESPAWN_ACTIVITY_WINDOW_MS`]. A value
/// of `0` means "session never touched" — that's NOT recent activity, so the
/// session is treated as idle (no respawn).
pub(crate) fn is_within_respawn_window(last_activity_ms: u64) -> bool {
    last_activity_ms != 0 && last_activity_ms < RESPAWN_ACTIVITY_WINDOW_MS
}

/// Pure helper, exposed for unit tests. Builds the SPEC §10 M4 `event/notify`
/// envelope. Pulled out so a unit test can assert on the JSON shape without
/// running a real Browser.
pub(crate) fn build_recovered_event(
    session_id: &str,
    previous_tab_ids: &[String],
    new_tab_ids: &[String],
) -> ServerEvent {
    ServerEvent {
        jsonrpc: "2.0".into(),
        method: "event/notify".into(),
        params: json!({
            "topic": "session.recovered",
            "session_id": session_id,
            "previous_tab_ids": previous_tab_ids,
            "new_tab_ids": new_tab_ids,
            "payload": {},
        }),
    }
}

/// CR-2 — `event/notify { topic: "session.recovery_failed" }` envelope. Emitted
/// once when the watcher exhausts [`MAX_RESPAWN_ATTEMPTS`] consecutive
/// respawn failures, immediately before the watcher gives up. Carries the
/// number of attempts and a single-line error string so MCP clients can
/// surface a useful failure message instead of going silent.
pub(crate) fn build_recovery_failed_event(
    session_id: &str,
    attempts: u32,
    last_error: &str,
) -> ServerEvent {
    ServerEvent {
        jsonrpc: "2.0".into(),
        method: "event/notify".into(),
        params: json!({
            "topic": "session.recovery_failed",
            "session_id": session_id,
            "attempts": attempts,
            "last_error": last_error,
            "payload": {},
        }),
    }
}

async fn run(state: Arc<State>, entry: Arc<SessionEntry>) {
    let mut consecutive_failures: u32 = 0;
    // CR-2 — most recent respawn error stringified, surfaced in the
    // `session.recovery_failed` event when MAX_RESPAWN_ATTEMPTS is hit. The
    // assignment in the failure arm is read on the cap-exhaust branch.
    #[allow(unused_assignments)]
    let mut last_error: String = String::new();
    loop {
        // Subscribe to the *current* live Browser's exit channel. After a
        // successful respawn the loop re-enters and grabs the new channel.
        let exit_rx = {
            let browser = entry.browser.load_full();
            browser.wait_for_exit()
        };

        let Some(mut rx) = exit_rx else {
            // Either another watcher already took the receiver (shouldn't
            // happen — only this task subscribes), or it was consumed by
            // `Browser::shutdown` (clean exit path). Either way, bail.
            info!(
                session_id = %entry.session_id,
                "crash watcher: exit channel unavailable; stopping"
            );
            return;
        };

        // Block until the CDP reader loop signals exit (Chromium died or
        // the pipe closed). This is the same signal that `Browser::shutdown`
        // uses internally for the graceful path.
        let _ = rx.recv().await;

        let last_activity = entry.last_activity_ms();
        if !is_within_respawn_window(last_activity) {
            info!(
                session_id = %entry.session_id,
                last_activity_ms = last_activity,
                "chromium exited; activity outside respawn window — leaving session drained"
            );
            return;
        }

        warn!(
            session_id = %entry.session_id,
            last_activity_ms = last_activity,
            "chromium crashed within activity window; attempting respawn against same UDD"
        );

        match respawn_and_swap(&state, &entry).await {
            Ok(outcome) => {
                consecutive_failures = 0;
                state
                    .metrics
                    .session(&entry.session_id)
                    .recovery_count
                    .fetch_add(1, Ordering::Relaxed);
                info!(
                    session_id = %entry.session_id,
                    new_tab_count = outcome.new_tab_ids.len(),
                    "session recovered"
                );
                let ev = build_recovered_event(
                    &entry.session_id,
                    &outcome.previous_tab_ids,
                    &outcome.new_tab_ids,
                );
                let _ = entry.try_push(ev);
                // Re-enter loop: the ArcSwap now holds the new Browser, and
                // the next iteration grabs ITS exit channel.
                continue;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                last_error = format!("{e:#}");
                state
                    .metrics
                    .session(&entry.session_id)
                    .recovery_failed_count
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    session_id = %entry.session_id,
                    attempt = consecutive_failures,
                    max = MAX_RESPAWN_ATTEMPTS,
                    error = %e,
                    "respawn failed"
                );
                if consecutive_failures >= MAX_RESPAWN_ATTEMPTS {
                    warn!(
                        session_id = %entry.session_id,
                        "respawn failed {MAX_RESPAWN_ATTEMPTS} times; giving up"
                    );
                    // CR-2 — surface to the bound MCP client BEFORE the
                    // watcher returns. Best-effort: if no client is bound the
                    // event is dropped, matching `try_push` semantics.
                    let ev = build_recovery_failed_event(
                        &entry.session_id,
                        consecutive_failures,
                        &last_error,
                    );
                    let _ = entry.try_push(ev);
                    return;
                }
                // Brief backoff so we don't busy-spin on a UDD that's
                // poisoned (e.g. ProcessSingleton lock).
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

struct RespawnOutcome {
    previous_tab_ids: Vec<String>,
    new_tab_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredPageIdentity {
    pub tab_id: String,
    pub target_id: String,
    pub url: String,
    pub title: String,
}

async fn respawn_and_swap(state: &Arc<State>, entry: &Arc<SessionEntry>) -> Result<RespawnOutcome> {
    // Snapshot the dying Browser's config + tab ids BEFORE the swap so the
    // event payload can include them and so we can relaunch with the same
    // mode / extra_args / UDD.
    let (mode, previous_tab_ids, previous_pages) = {
        let old = entry.browser.load_full();
        let mode = old.mode();
        let pages = old.default_context().list_tabs();
        let prev_ids: Vec<String> = pages.iter().map(|p| p.tab_id().0.clone()).collect();
        let prev_pages: Vec<RecoveredPageIdentity> = pages
            .iter()
            .map(|p| RecoveredPageIdentity {
                tab_id: p.tab_id().0.clone(),
                target_id: p.target_id().to_owned(),
                url: p.url(),
                title: p.title(),
            })
            .collect();
        (mode, prev_ids, prev_pages)
    };

    let binary = state
        .chromium_binary
        .lock()
        .clone()
        .ok_or_else(|| anyhow!("chromium binary not yet resolved"))?;
    let user_data_dir = state.user_data_root.join(&entry.session_id);

    let staged_proxy = entry.staged_proxy.read().clone();
    let sandbox_profile = entry
        .session_sandbox
        .read()
        .as_ref()
        .map(|sandbox| sandbox.profile_path.clone());
    let new_browser = Browser::launch(BrowserConfig {
        binary,
        user_data_dir: user_data_dir.clone(),
        mode,
        extra_args: Vec::new(),
        sandbox_profile,
        seed_plan_path: Some(sandbox::seed_plan_path(&user_data_dir)),
        proxy: staged_proxy,
    })
    .await
    .map_err(|e| anyhow!("respawn Browser::launch: {e}"))?;
    let new_browser = Arc::new(new_browser);

    // Reattach to all targets Chromium rehydrated from `Default/Sessions/`.
    // Stealth re-injection happens inside the helper.
    let restored_pages = new_browser
        .default_context()
        .reattach_existing_targets()
        .await
        .context("reattach_existing_targets")?;

    // Re-arm M2 + M5 on each restored page BEFORE the swap so a hot reader
    // racing in on `entry.browser.load()` won't see a Browser whose pages
    // are missing forwarders.
    for page in &restored_pages {
        if let Err(e) = install_mutation_observer_bootstrap(page).await {
            warn!(
                session_id = %entry.session_id,
                tab_id = %page.tab_id().0,
                error = %e,
                "M2 mutation-observer install failed on restored page"
            );
        }
        // M5 forwarders.
        crate::router::attach_page_event_forwarders(entry, Arc::clone(page));
    }
    crate::router::replay_network_observe_subscriptions(entry, &restored_pages, &previous_pages);

    let new_tab_ids: Vec<String> = restored_pages
        .iter()
        .map(|p| p.tab_id().0.clone())
        .collect();

    // Atomic swap. Any reader holding a `load_full()` Arc snapshot from
    // before the swap continues to use the dead Browser for the lifetime
    // of their request — that request will surface a CDP error rather
    // than panicking, and the next call will see the new Browser.
    entry.browser.store(new_browser);

    Ok(RespawnOutcome {
        previous_tab_ids,
        new_tab_ids,
    })
}

/// Run the M2 [`OBSERVER_BOOTSTRAP_JS`] on a freshly-restored Page.
///
/// `ax_engine::install_mutation_observer` takes a `cdp_client::CdpSession`,
/// but our restored Pages own a [`browser_engine::page::Page`] backed by the
/// shared `cdp_client` transport (the prior in-tree `browser-engine/src/cdp.rs`
/// shim was deleted when `cdp_client` absorbed it). The wire effect of the
/// two CDP calls is identical, so we issue them directly via [`Page::cdp_call`].
async fn install_mutation_observer_bootstrap(page: &Page) -> Result<()> {
    page.cdp_call(
        "Page.addScriptToEvaluateOnNewDocument",
        Some(json!({"source": OBSERVER_BOOTSTRAP_JS})),
    )
    .await
    .context("Page.addScriptToEvaluateOnNewDocument (M2 observer)")?;
    // Also evaluate on the *current* document — addScriptToEvaluateOnNewDocument
    // only fires on new doc creation, but Chromium has already rehydrated
    // these tabs, so without this the observer wouldn't be armed until the
    // next navigation.
    let _ = page
        .cdp_call(
            "Runtime.evaluate",
            Some(json!({
                "expression": OBSERVER_BOOTSTRAP_JS,
                "awaitPromise": false,
                "returnByValue": true,
            })),
        )
        .await;
    Ok(())
}

// `mpsc` is imported above so the test module below can construct an mpsc
// for free without re-importing through tokio.
#[allow(dead_code)]
fn _suppress_unused_mpsc_import_check(_t: mpsc::Sender<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respawn_window_classifies_recent_activity() {
        // Inside the window.
        assert!(is_within_respawn_window(1));
        assert!(is_within_respawn_window(15_000));
        assert!(is_within_respawn_window(RESPAWN_ACTIVITY_WINDOW_MS - 1));
        // Outside the window.
        assert!(!is_within_respawn_window(RESPAWN_ACTIVITY_WINDOW_MS));
        assert!(!is_within_respawn_window(RESPAWN_ACTIVITY_WINDOW_MS + 1));
        assert!(!is_within_respawn_window(60_000));
        // The "never touched" sentinel must not trigger a respawn.
        assert!(!is_within_respawn_window(0));
    }

    #[test]
    fn recovered_event_has_required_keys() {
        let prev = vec!["t_aaaa".to_string(), "t_bbbb".to_string()];
        let new = vec!["t_cccc".to_string()];
        let ev = build_recovered_event("s_42", &prev, &new);

        assert_eq!(ev.jsonrpc, "2.0");
        assert_eq!(ev.method, "event/notify");
        assert_eq!(ev.params["topic"], "session.recovered");
        assert_eq!(ev.params["session_id"], "s_42");
        assert_eq!(
            ev.params["previous_tab_ids"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            2
        );
        assert_eq!(ev.params["previous_tab_ids"][0], "t_aaaa");
        assert_eq!(
            ev.params["new_tab_ids"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            1
        );
        assert_eq!(ev.params["new_tab_ids"][0], "t_cccc");
        // Empty payload object per SPEC §2.6 envelope.
        assert!(ev.params["payload"].is_object());
    }

    #[test]
    fn recovered_event_serializes_for_wire() {
        let ev = build_recovered_event("s_a", &[], &["t_x".into()]);
        let s = serde_json::to_string(&ev).unwrap();
        // Wire shape sanity — the broker pipes this verbatim to the MCP
        // client, so we check for the literal keys clients pattern-match on.
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"method\":\"event/notify\""));
        assert!(s.contains("\"topic\":\"session.recovered\""));
        assert!(s.contains("\"session_id\":\"s_a\""));
        assert!(s.contains("\"new_tab_ids\""));
    }

    #[test]
    fn recovery_failed_event_has_required_keys() {
        let ev = build_recovery_failed_event("s_99", 3, "Browser::launch: ENOSPC");
        assert_eq!(ev.method, "event/notify");
        assert_eq!(ev.params["topic"], "session.recovery_failed");
        assert_eq!(ev.params["session_id"], "s_99");
        assert_eq!(ev.params["attempts"], 3);
        assert_eq!(ev.params["last_error"], "Browser::launch: ENOSPC");
        assert!(ev.params["payload"].is_object());
    }
}
