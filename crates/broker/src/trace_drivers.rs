//! SPEC §10 M10 — broker-side trace drivers.
//!
//! Hosts the 500 ms-cadence DOM-snapshot ticker that runs once per traced
//! page. Spawned at session-register time when the session has trace mode
//! on, and re-spawned for new tabs as they open. Each driver task is
//! tracked on [`crate::SessionEntry::trace_drivers`] so shutdown can abort
//! them cleanly per the §10 quality gate ("no spawn without JoinHandle
//! storage").

use std::sync::Arc;
use std::time::Duration;

use observability::trace::{TraceEvent, TraceSink};
use serde_json::Value;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::registry::SessionEntry;

/// Cadence required by SPEC §10 M10.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn one 500 ms DOM-snapshot driver task for `page` and return its
/// [`JoinHandle`]. The caller must store the handle on the SessionEntry
/// (via [`SessionEntry::push_trace_driver`]) so the driver is aborted on
/// session shutdown / crash recovery.
///
/// CANCELLATION: safe — `tokio::time::interval` is cancel-safe; on drop the
/// task simply stops ticking.
pub fn spawn_dom_snapshot_driver(
    sink: Arc<dyn TraceSink>,
    page: Arc<browser_engine::Page>,
    session_id: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately; skip it so the 500 ms cadence is
        // honored from t=500ms onward (matches SPEC wording).
        ticker.tick().await;
        let tab_id = page.tab_id().0.clone();

        loop {
            ticker.tick().await;
            // Cheap DOM snapshot via DOMSnapshot.captureSnapshot. We
            // deliberately don't run the full ax-engine snapshot here —
            // the 500 ms cadence makes that too expensive; replay tools
            // get enough fidelity from the raw CDP node tree.
            let res = match page
                .cdp_call(
                    "DOMSnapshot.captureSnapshot",
                    Some(serde_json::json!({"computedStyles": []})),
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    debug!(
                        session_id = %session_id,
                        tab_id = %tab_id,
                        error = %e,
                        "trace dom_snapshot tick failed (page likely closed)"
                    );
                    // If CDP errors persist (page died), let the task end —
                    // a fresh tab hooks in via `attach_trace_driver_for_page`.
                    break;
                }
            };
            let snapshot_seq = next_snapshot_seq();
            let payload = match res {
                Value::Null => Value::Object(Default::default()),
                v => v,
            };
            match sink.save_snapshot_json(snapshot_seq, &payload) {
                Ok((snapshot_path, hash)) => {
                    sink.record(TraceEvent::DomSnapshot {
                        ts_ms: sink.now_ms(),
                        session_id: session_id.clone(),
                        tab_id: tab_id.clone(),
                        snapshot_seq,
                        hash,
                        snapshot_path,
                    });
                }
                Err(e) => {
                    warn!(error = %e, "trace dom_snapshot persist failed");
                }
            }
        }
    })
}

/// Spawn a driver for one page and register the handle on `entry`.
pub fn attach_trace_driver_for_page(
    entry: &Arc<SessionEntry>,
    sink: Arc<dyn TraceSink>,
    page: Arc<browser_engine::Page>,
) {
    let h = spawn_dom_snapshot_driver(sink, page, entry.session_id.clone());
    entry.push_trace_driver(h);
}

/// Spawn drivers for every existing tab on this session. Called from
/// `session.register` after the trace sink is attached.
pub fn attach_trace_drivers(entry: &Arc<SessionEntry>, sink: Arc<dyn TraceSink>) {
    let pages = entry.browser.load().default_context().list_tabs();
    for p in pages {
        attach_trace_driver_for_page(entry, Arc::clone(&sink), p);
    }
}

fn next_snapshot_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
