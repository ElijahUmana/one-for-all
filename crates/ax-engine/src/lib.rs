//! `ax-engine` — accessibility-tree-driven page snapshots.
//!
//! Drives `Accessibility.getFullAXTree` + `DOMSnapshot.captureSnapshot` via
//! [`cdp_client::CdpSession`], then merges the two into a flat
//! [`merge::Snapshot`] of [`merge::Element`] rows. Each element carries a
//! deterministic `stable_id` (SHA-256 per SPEC §1 D14) plus a dense
//! `index` and a `ref="e<N>"` per SPEC §7.
//!
//! # Public surface
//!
//! - [`snapshot`] — high-level entry point: enable AX → fetch tree → fetch
//!   DOM snapshot → merge → augment with M1 fields → return.
//! - [`install_mutation_observer`] — SPEC §10 M2: install the per-document
//!   bootstrap so [`mutation::drain_log`] can return the records that
//!   landed since the last drain. The wire-level
//!   `page.snapshot {since_seq: N}` branch is implemented in
//!   `browser_engine::Page::snapshot_delta_since`, which combines this
//!   crate's [`mutation`] primitives with its own per-tab `snapshot_seq`
//!   anchor; see SPEC §7 "Snapshot delta shape".
//! - [`merge`] — pure merge algorithm, exposed for tests and callers that
//!   already have the raw CDP responses.
//! - [`mutation`] — observer bootstrap + drain helpers (M2).
//! - [`index::StableId`] — the stable identity hash.
//! - [`iframe::splice`] — compose child-frame snapshots into a parent.
//! - [`shadow`] — internal shadow-DOM helpers (`pub(crate)`); see SPEC §10
//!   for the closed-shadow-root limitation.
//!
//! # Threading
//!
//! Stateless. `snapshot()` only awaits CDP round-trips; nothing global.

#![deny(unsafe_op_in_unsafe_fn)]
// SPEC §10: zero `.unwrap()` / `.expect()` in production code.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod iframe;
pub mod index;
pub mod merge;
pub mod mutation;
pub(crate) mod shadow;

pub use index::StableId;
pub use merge::{
    ConsoleMessage, Element, ElementState, ExceptionRecord, NetworkSummary, Rect, Snapshot,
    Viewport,
};

use anyhow::{Context, Result};
use cdp_client::CdpSession;
use serde_json::{json, Value};

/// Drive a CDP session to produce one [`Snapshot`].
///
/// CANCELLATION: safe — every CDP round-trip is independent and the
/// underlying mpsc/broadcast plumbing handles drop cleanly. The returned
/// future awaits but does not retain non-tokio state.
pub async fn snapshot(session: &CdpSession) -> Result<Snapshot> {
    enable_domains(session).await?;

    let ax = session
        .send_raw(
            "Accessibility.getFullAXTree",
            json!({ "fetchRelatives": true }),
        )
        .await
        .context("Accessibility.getFullAXTree")?;

    let dom = session
        .send_raw(
            "DOMSnapshot.captureSnapshot",
            json!({
                "computedStyles": ["display", "visibility", "pointer-events"],
                "includeDOMRects": true,
                "includePaintOrder": false,
            }),
        )
        .await
        .context("DOMSnapshot.captureSnapshot")?;

    let (url, title) = url_and_title(session).await;
    let mut snap = merge::merge(&ax, &dom, url, title)?;

    // M1: augment with viewport + focused_ref. Console / exceptions /
    // network are filled in by the engine layer (which holds the
    // since-last-snapshot ring buffers); ax-engine just carries the schema.
    snap.viewport = fetch_viewport(session).await.unwrap_or_default();
    snap.focused_ref = fetch_focused_ref(session, &snap.elements).await;

    Ok(snap)
}

/// Install the per-document MutationObserver bootstrap (SPEC §10 M2).
/// Idempotent; safe to call once per session at attach time.
pub async fn install_mutation_observer(session: &CdpSession) -> Result<String> {
    mutation::install_observer(session).await
}

async fn enable_domains(session: &CdpSession) -> Result<()> {
    session
        .send_raw("Accessibility.enable", Value::Null)
        .await
        .context("Accessibility.enable")?;
    session
        .send_raw("DOMSnapshot.enable", Value::Null)
        .await
        .context("DOMSnapshot.enable")?;
    // M1 needs Runtime + Page enabled so the engine sees console + exceptions
    // + lifecycle events. enable() is idempotent.
    let _ = session.send_raw("Runtime.enable", Value::Null).await;
    let _ = session.send_raw("Page.enable", Value::Null).await;
    Ok(())
}

async fn url_and_title(session: &CdpSession) -> (String, String) {
    let info = match session.send_raw("Target.getTargetInfo", Value::Null).await {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new()),
    };
    let url = info
        .get("targetInfo")
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = info
        .get("targetInfo")
        .and_then(|t| t.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (url, title)
}

async fn fetch_viewport(session: &CdpSession) -> Option<Viewport> {
    let m = session
        .send_raw("Page.getLayoutMetrics", Value::Null)
        .await
        .ok()?;
    let visual = m.get("visualViewport")?;
    Some(Viewport {
        w: visual
            .get("clientWidth")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        h: visual
            .get("clientHeight")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        scroll_x: visual.get("pageX").and_then(Value::as_f64).unwrap_or(0.0),
        scroll_y: visual.get("pageY").and_then(Value::as_f64).unwrap_or(0.0),
        device_scale_factor: m
            .get("cssVisualViewport")
            .and_then(|v| v.get("scale"))
            .and_then(Value::as_f64)
            .unwrap_or(1.0),
    })
}

/// Resolve `document.activeElement` to one of our snapshot refs by matching
/// the focused node's `outerHTML` signature against the snapshot. Falls
/// back to `None` when nothing is focused.
async fn fetch_focused_ref(session: &CdpSession, elements: &[Element]) -> Option<String> {
    let res = session
        .send_raw(
            "Runtime.evaluate",
            json!({
                "expression": "(function(){\
                    var a = document.activeElement; \
                    if (!a || a === document.body) return null; \
                    var r = a.getBoundingClientRect(); \
                    return {tag: a.tagName, x: r.x, y: r.y, w: r.width, h: r.height}; \
                })()",
                "returnByValue": true,
            }),
        )
        .await
        .ok()?;
    let val = res.get("result").and_then(|r| r.get("value"))?;
    if val.is_null() {
        return None;
    }
    let x = val.get("x").and_then(Value::as_f64)?;
    let y = val.get("y").and_then(Value::as_f64)?;
    let w = val.get("w").and_then(Value::as_f64)?;
    let h = val.get("h").and_then(Value::as_f64)?;
    // Match by bbox center hit-test against snapshot elements.
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    for e in elements {
        if let Some(b) = e.bbox {
            if cx >= b.x && cx <= b.x + b.w && cy >= b.y && cy <= b.y + b.h {
                return Some(e.r#ref.clone());
            }
        }
    }
    None
}
