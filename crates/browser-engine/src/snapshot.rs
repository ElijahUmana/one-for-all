//! AX-tree snapshot — the heart of the LLM-facing surface.
//!
//! Implements SPEC §7 `page.snapshot`. Per SPEC D14 we merge
//! `Accessibility.getFullAXTree` with `DOMSnapshot.captureSnapshot`, then
//! produce indexed elements with stable refs.
//!
//! `ref` format is `e<index>`, scoped to `(tab_id, snapshot_seq)`. The
//! broker validates that a `ref` came from the latest snapshot and emits
//! `-32004 ElementStale` otherwise. (Validation is in the broker's router,
//! not here — this module produces refs and remembers the seq.)

use anyhow::{Context as _, Result};
use ax_engine::mutation::{MutationError, MutationRecord};
use cdp_client::{
    generated::domains::{
        accessibility as cdp_a11y, dom_snapshot as cdp_dom_snapshot, runtime as cdp_runtime,
        target as cdp_target,
    },
    SessionId,
};
use observability::metrics::mutation_metrics;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::page::{DeltaAnchor, Page};

/// Bounding box in CSS pixels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Element state, mirroring SPEC §7 element shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ElementState {
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub pressed: Option<bool>,
    pub selected: Option<bool>,
    pub disabled: bool,
}

/// Single element in a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub index: usize,
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub description: Option<String>,
    pub state: ElementState,
    pub bbox: BBox,
    pub interactable: bool,
    pub frame_id: String,
    /// 32-byte hex hash from D14 (stable id across reflows).
    pub stable_id: String,
}

/// SPEC §10 M1 — viewport summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Viewport {
    pub w: f64,
    pub h: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub device_scale_factor: f64,
}

/// SPEC §10 M1 — network rollup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkSummary {
    pub in_flight: u64,
    pub completed_since_last: u64,
    pub failed_since_last: u64,
}

/// One full snapshot. `tree` is a condensed form for human inspection;
/// `elements` is the canonical indexed array used for actions.
///
/// SPEC §10 M1 added the `console`, `exceptions`, `network`, `focused_ref`,
/// and `viewport` fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_seq: u64,
    /// SPEC §10 M2 — `0` for full snapshots that were not produced via a
    /// `since_seq` request; echoes the caller's `since_seq` when the
    /// engine fell back to a full snapshot from the delta path.
    #[serde(default)]
    pub base_seq: u64,
    /// SPEC §10 M2 — always `false` for full snapshots. Distinguishes the
    /// two `SnapshotResponse` shapes when the wire form is untagged.
    #[serde(default)]
    pub partial: bool,
    /// SPEC §10 M2 — set to `true` when the engine fell back to a full
    /// snapshot because the caller's `since_seq` could not be honored:
    /// the in-page mutation log overflowed past `MAX_LOG = 4096`, the
    /// per-page anchor was cleared (top-frame nav, frame attach), the
    /// drain transport failed, or the caller's seq did not match the
    /// current anchor. Combined with `base_seq` echoing the caller's
    /// original `since_seq`, this lets agents discard their cached
    /// element table without guessing why their delta chain broke.
    /// Always `false` on the no-`since_seq` `page.snapshot` entry point.
    #[serde(default)]
    pub anchor_stale: bool,
    pub url: String,
    pub title: String,
    pub elements: Vec<Element>,
    pub tree: Value,
    pub console: Vec<crate::page::ConsoleMessage>,
    pub exceptions: Vec<crate::page::PageException>,
    pub network: NetworkSummary,
    pub focused_ref: Option<String>,
    pub viewport: Viewport,
}

#[derive(Debug, Clone)]
struct SnapshotParts {
    nodes_val: Value,
    elements: Vec<Element>,
}

#[derive(Debug, Clone)]
struct ChildFrameRef {
    target_id: String,
}

/// SPEC §10 M2 — partial snapshot. Returned by `page.snapshot {since_seq:
/// N}` when `N` matches the page's most recent `snapshot_seq` and the
/// in-page MutationObserver log has not overflowed since then.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDelta {
    pub snapshot_seq: u64,
    pub base_seq: u64,
    /// Always `true` for delta responses — opposite of [`Snapshot::partial`].
    pub partial: bool,
    pub mutations: Vec<MutationRecord>,
    pub url: String,
    pub title: String,
}

/// SPEC §10 M2 — wire shape of `page.snapshot`. Untagged so the legacy
/// full-snapshot consumers continue to deserialize against `Snapshot` and
/// new consumers can branch on `partial`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapshotResponse {
    /// Full M1 snapshot. `partial` is `false`.
    Full(Snapshot),
    /// Delta since `base_seq`. `partial` is `true`.
    Delta(SnapshotDelta),
}

impl Page {
    /// Implements `page.snapshot` (SPEC §7). Returns the full M1-augmented
    /// snapshot and refreshes this page's delta anchor so the next
    /// `snapshot_delta_since` call can compute mutations relative to it.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        // SPEC §10 M2 — Drain the in-page mutation log so the next
        // delta call only sees mutations that happened *after* this
        // snapshot, and stamp the high-water mark on the new anchor.
        // Drain BEFORE walking the AX tree so we don't lose any records
        // that arrive while DOMSnapshot/Accessibility build the full
        // tree. Best-effort: a transport hiccup here just means the
        // first delta after this snapshot will fall back to a full one.
        let high_water = drain_mutation_high_water(self).await;
        self.do_snapshot(
            high_water, /*base_seq=*/ 0, /*partial=*/ false, /*anchor_stale=*/ false,
        )
        .await
    }

    /// SPEC §10 M2 — `page.snapshot {since_seq: N}`.
    ///
    /// Returns a [`SnapshotDelta`] when `since_seq` matches this page's
    /// current delta anchor and the in-page mutation log is contiguous
    /// since the anchor's high-water mark. Otherwise falls back to a
    /// full [`Snapshot`] (with `partial: false`, `base_seq` echoing the
    /// caller's `since_seq` so the consumer can correlate, and
    /// `anchor_stale: true` so the agent knows to discard cached refs).
    ///
    /// Fallback triggers (full snapshot returned with `anchor_stale:
    /// true`):
    /// - `since_seq` does not match the current `snapshot_seq` — caller
    ///   too far behind, or no anchor (top-frame nav cleared it).
    /// - The mutation log overflowed past `MAX_LOG = 4096` since the
    ///   anchor (gap detected: smallest drained seq > anchor +1).
    /// - The drain transport call failed.
    /// - The drain JSON failed to parse (e.g. an SPA clobbered
    ///   `window.__claudeBridgeMutationDrain`); recorded in
    ///   `mutation_metrics().drain_failures`.
    ///
    /// Special case: `since_seq == 0` is "initial call, no anchor yet"
    /// — equivalent to a plain `page.snapshot`, so `anchor_stale` is
    /// `false`.
    pub async fn snapshot_delta_since(&self, since_seq: u64) -> Result<SnapshotResponse> {
        // since_seq=0 means the caller has no anchor yet — treat as a
        // full snapshot request (still drain so the new anchor's
        // high-water reflects current page state). This is the initial
        // handshake, not a stale anchor, so `anchor_stale: false`.
        if since_seq == 0 {
            let snap = self.snapshot().await?;
            return Ok(SnapshotResponse::Full(snap));
        }

        if page_has_child_frames(self).await? {
            mutation_metrics().record_anchor_invalidation();
            let snap = self
                .do_snapshot(
                    drain_mutation_high_water(self).await,
                    since_seq,
                    /*partial=*/ false,
                    /*anchor_stale=*/ true,
                )
                .await?;
            return Ok(SnapshotResponse::Full(snap));
        }

        // Without a valid anchor matching `since_seq`, we can't safely
        // compute a delta — the in-page seq counter is meaningless to us
        // until we re-establish the high-water mark. Fall back to full
        // and signal `anchor_stale: true` so the caller discards refs.
        let anchor = self.delta_anchor();
        let Some(anchor) = anchor else {
            // No anchor (likely top-frame nav cleared it). Fall back.
            mutation_metrics().record_anchor_invalidation();
            let snap = self
                .do_snapshot(
                    drain_mutation_high_water(self).await,
                    since_seq,
                    /*partial=*/ false,
                    /*anchor_stale=*/ true,
                )
                .await?;
            return Ok(SnapshotResponse::Full(snap));
        };
        if anchor.snapshot_seq != since_seq {
            // Caller is behind; we'd be lying if we claimed the delta
            // covers their `since_seq`. Full snapshot it is.
            mutation_metrics().record_anchor_invalidation();
            let snap = self
                .do_snapshot(
                    drain_mutation_high_water(self).await,
                    since_seq,
                    /*partial=*/ false,
                    /*anchor_stale=*/ true,
                )
                .await?;
            return Ok(SnapshotResponse::Full(snap));
        }

        // Drain the JS log. Filter to seq > anchor.high_water; detect
        // overflow by checking for a gap.
        let drained = match drain_mutation_log(self).await {
            Ok(v) => v,
            Err(err) => {
                // Drain failed (transport or parse). Degrade with a
                // logged warning + metric rather than mask as an empty
                // delta — silently masking would strand the caller on
                // a stale anchor. SPEC §10 M2 / N33.
                mutation_metrics().record_drain_failure();
                warn!(
                    target: "ax_engine::mutation",
                    "drain failure: {err}; falling back to full snapshot"
                );
                let snap = self
                    .do_snapshot(
                        drain_mutation_high_water(self).await,
                        since_seq,
                        /*partial=*/ false,
                        /*anchor_stale=*/ true,
                    )
                    .await?;
                return Ok(SnapshotResponse::Full(snap));
            }
        };

        if mutation_log_overflowed(anchor.mutation_high_water, &drained) {
            // The MutationObserver bootstrap drops oldest records past
            // MAX_LOG=4096 — silently truncating would be lossy, so we
            // promote to a full snapshot. The new anchor's high-water
            // is the largest seq we DID see (so subsequent calls don't
            // chase records we already discarded).
            mutation_metrics().record_drain_overflow();
            let new_high_water = drained
                .iter()
                .map(|m| m.seq)
                .max()
                .unwrap_or(anchor.mutation_high_water);
            let snap = self
                .do_snapshot(
                    new_high_water,
                    since_seq,
                    /*partial=*/ false,
                    /*anchor_stale=*/ true,
                )
                .await?;
            return Ok(SnapshotResponse::Full(snap));
        }

        // Happy path: filter to mutations strictly newer than the
        // anchor, bump the snapshot seq, refresh the anchor.
        let mutations: Vec<MutationRecord> = drained
            .into_iter()
            .filter(|m| m.seq > anchor.mutation_high_water)
            .collect();
        let new_high_water = mutations
            .iter()
            .map(|m| m.seq)
            .max()
            .unwrap_or(anchor.mutation_high_water);
        let new_seq = self.next_snapshot_seq();
        self.set_delta_anchor(DeltaAnchor {
            snapshot_seq: new_seq,
            mutation_high_water: new_high_water,
        });

        Ok(SnapshotResponse::Delta(SnapshotDelta {
            snapshot_seq: new_seq,
            base_seq: since_seq,
            partial: true,
            mutations,
            url: self.url(),
            title: self.title(),
        }))
    }

    /// Internal: build a full snapshot, stamp `base_seq`/`partial`/
    /// `anchor_stale`, and (re)anchor `delta_anchor` to the given
    /// `mutation_high_water`. Used by both [`snapshot`] (with
    /// `base_seq=0, anchor_stale=false`) and the delta fallback paths
    /// (with `base_seq` echoing the caller's `since_seq` and
    /// `anchor_stale=true`).
    async fn do_snapshot(
        &self,
        mutation_high_water: u64,
        base_seq: u64,
        partial: bool,
        anchor_stale: bool,
    ) -> Result<Snapshot> {
        let seq = self.next_snapshot_seq();

        let mut root = snapshot_parts_for_session(self.cdp_session()).await?;
        let child_frames = child_frame_targets(self).await?;
        for child in child_frames {
            match snapshot_child_frame(self, &child).await {
                Ok(child_snapshot) => splice_snapshot_elements(&mut root.elements, child_snapshot),
                Err(err) => warn!(
                    target_id = %child.target_id,
                    error = %err,
                    "child-frame snapshot failed; continuing with root snapshot"
                ),
            }
        }

        let url = self.url();
        let title = self.title();

        // SPEC §10 M1 — augmented snapshot extras.
        let (console, exceptions, completed, failed) = self.drain_snapshot_extras();
        let network = NetworkSummary {
            in_flight: self.in_flight_count() as u64,
            completed_since_last: completed,
            failed_since_last: failed,
        };

        let viewport = collect_viewport(self).await.unwrap_or_default();
        let focused_ref = collect_focused_ref(self, &root.elements).await;

        // Stamp the new anchor *after* the full snapshot pipeline
        // succeeded. This means future `snapshot_delta_since(seq)` calls
        // will only see mutations strictly newer than `mutation_high_water`.
        self.set_delta_anchor(DeltaAnchor {
            snapshot_seq: seq,
            mutation_high_water,
        });

        Ok(Snapshot {
            snapshot_seq: seq,
            base_seq,
            partial,
            anchor_stale,
            url,
            title,
            elements: root.elements,
            // Wrap the AX tree in a thin object so consumers don't need to
            // know it came from a typed `Returns` struct. The shape matches
            // what the previous Value-poking impl produced.
            tree: json!({ "nodes": root.nodes_val }),
            console,
            exceptions,
            network,
            focused_ref,
            viewport,
        })
    }
}

async fn snapshot_parts_for_session(session: &cdp_client::CdpSession) -> Result<SnapshotParts> {
    let ax_full = session
        .send(cdp_a11y::GetFullAxTreeParams::default())
        .await
        .context("Accessibility.getFullAXTree")?;

    let dom = session
        .send(cdp_dom_snapshot::CaptureSnapshotParams {
            computed_styles: Value::Array(vec![
                Value::String("display".to_owned()),
                Value::String("visibility".to_owned()),
                Value::String("pointer-events".to_owned()),
            ]),
            include_blended_background_colors: Some(false),
            include_text_color_opacities: Some(false),
            include_dom_rects: Some(true),
            include_paint_order: Some(false),
        })
        .await
        .ok();

    let nodes_val = ax_full.nodes;
    let nodes = nodes_val.as_array().cloned().unwrap_or_default();

    let mut elements = Vec::with_capacity(nodes.len());
    let mut sibling_counter: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        let role = ax_string(node, "role").unwrap_or_default();
        let name = ax_string(node, "name").unwrap_or_default();
        let value = ax_string(node, "value");
        let description = ax_string(node, "description");
        let parent_role = node
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|pid| {
                nodes
                    .iter()
                    .find(|n| n.get("nodeId").and_then(Value::as_str) == Some(pid))
            })
            .and_then(|p| ax_string(p, "role"))
            .unwrap_or_default();

        let key = (role.clone(), parent_role.clone());
        let sibling_idx = {
            let e = sibling_counter.entry(key).or_insert(0);
            let v = *e;
            *e += 1;
            v
        };

        let stable_id = stable_id_hash(&role, &name, &parent_role, sibling_idx);

        let bbox = bbox_from_dom(&dom, node).unwrap_or(BBox {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        });

        let state = ElementState {
            checked: ax_bool(node, "checked"),
            expanded: ax_bool(node, "expanded"),
            pressed: ax_bool(node, "pressed"),
            selected: ax_bool(node, "selected"),
            disabled: ax_bool(node, "disabled").unwrap_or(false),
        };

        let interactable = matches!(
            role.as_str(),
            "button"
                | "link"
                | "textbox"
                | "checkbox"
                | "combobox"
                | "menuitem"
                | "tab"
                | "switch"
                | "slider"
                | "searchbox"
                | "spinbutton"
        ) && !state.disabled
            && bbox.w > 0.0
            && bbox.h > 0.0;

        elements.push(Element {
            index: i,
            element_ref: format!("e{i}"),
            role,
            name,
            value,
            description,
            state,
            bbox,
            interactable,
            frame_id: ax_string(node, "frameId").unwrap_or_default(),
            stable_id,
        });
    }

    Ok(SnapshotParts {
        nodes_val,
        elements,
    })
}

async fn page_has_child_frames(page: &Page) -> Result<bool> {
    Ok(!child_frame_targets(page).await?.is_empty())
}

async fn child_frame_targets(page: &Page) -> Result<Vec<ChildFrameRef>> {
    let res = page
        .cdp_call("Target.getTargets", Some(json!({})))
        .await
        .context("Target.getTargets")?;
    let infos = res
        .get("targetInfos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut children = Vec::new();
    for info in infos {
        if info.get("type").and_then(Value::as_str) != Some("iframe") {
            continue;
        }
        if info.get("parentId").and_then(Value::as_str) != Some(page.target_id()) {
            continue;
        }
        let Some(_frame_id) = info.get("parentFrameId").and_then(Value::as_str) else {
            continue;
        };
        let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
            continue;
        };
        children.push(ChildFrameRef {
            target_id: target_id.to_owned(),
        });
    }
    Ok(children)
}

async fn snapshot_child_frame(page: &Page, child: &ChildFrameRef) -> Result<Vec<Element>> {
    let attach = page
        .browser()
        .cdp()
        .root_session()
        .send(cdp_target::AttachToTargetParams {
            target_id: Value::String(child.target_id.clone()),
            flatten: Some(true),
        })
        .await
        .context("Target.attachToTarget")?;
    let session_id = attach
        .session_id
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Target.attachToTarget returned non-string sessionId"))?
        .to_owned();
    let session_id = SessionId(session_id);
    let session = page.browser().cdp().session_for(&session_id);

    let snapshot_result = snapshot_parts_for_session(&session).await;
    page.browser().cdp().drop_session(&session_id);
    let child_snapshot = snapshot_result?;
    let mut elements = child_snapshot.elements;
    for element in &mut elements {
        if element.frame_id.is_empty() {
            element.frame_id = child.target_id.clone();
        }
        element.stable_id = remix_stable_id_with_frame(&element.stable_id, &element.frame_id);
    }
    Ok(elements)
}

fn splice_snapshot_elements(parent: &mut Vec<Element>, mut child: Vec<Element>) {
    let base = parent.len();
    for (i, element) in child.iter_mut().enumerate() {
        element.index = base + i;
        element.element_ref = format!("e{}", element.index);
    }
    parent.extend(child);
}

fn remix_stable_id_with_frame(id: &str, frame_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    h.update([0x1F]);
    h.update(frame_id.as_bytes());
    hex::encode(h.finalize())
}

/// SPEC §10 M2 — drain `window.__claudeBridgeMutationLog` via the
/// `ax_engine` helper and return the highest seq seen (or `0` if the
/// log was empty / drain failed). Internal helper used by `snapshot()`
/// to anchor the high-water mark. Failures are intentionally swallowed
/// here — the worst case is the *next* delta call falls back to a full
/// snapshot. The delta path itself uses the typed [`drain_mutation_log`]
/// so it can distinguish transport / parse errors and bump the
/// `mutation_metrics()` counters.
async fn drain_mutation_high_water(page: &Page) -> u64 {
    drain_mutation_log(page)
        .await
        .ok()
        .and_then(|v| v.into_iter().map(|m| m.seq).max())
        .unwrap_or(0)
}

/// SPEC §10 M2 — drain the in-page MutationObserver log. Thin wrapper
/// around [`ax_engine::mutation::drain_log`] that exposes the typed
/// `CdpSession` of this page. Propagates the typed [`MutationError`] so
/// the snapshot delta path can fall back to a full snapshot, log the
/// failure with `tracing::warn!`, and bump
/// `mutation_metrics().drain_failures`.
async fn drain_mutation_log(
    page: &Page,
) -> std::result::Result<Vec<MutationRecord>, MutationError> {
    ax_engine::mutation::drain_log(page.cdp_session()).await
}

/// SPEC §10 M2 — overflow detector. `MAX_LOG = 4096` records in the JS
/// ring; once full, oldest records are dropped while the seq counter
/// keeps climbing. So a gap in the drained seqs (smallest > anchor + 1)
/// means we lost records and can't safely compute the delta.
///
/// Pure helper — exposed to `#[cfg(test)]` for unit coverage.
fn mutation_log_overflowed(anchor_high_water: u64, drained: &[MutationRecord]) -> bool {
    let Some(min_seq) = drained.iter().map(|m| m.seq).min() else {
        return false; // empty drain — nothing dropped
    };
    min_seq > anchor_high_water + 1
}

async fn collect_viewport(page: &Page) -> Option<Viewport> {
    let res = page
        .cdp_send(cdp_runtime::EvaluateParams {
            expression: "JSON.stringify({w:window.innerWidth,h:window.innerHeight,sx:window.scrollX,sy:window.scrollY,dpr:window.devicePixelRatio})".to_owned(),
            return_by_value: Some(true),
            ..Default::default()
        })
        .await
        .ok()?;
    let raw = res.result.get("value").and_then(Value::as_str)?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    Some(Viewport {
        w: v.get("w").and_then(Value::as_f64).unwrap_or(0.0),
        h: v.get("h").and_then(Value::as_f64).unwrap_or(0.0),
        scroll_x: v.get("sx").and_then(Value::as_f64).unwrap_or(0.0),
        scroll_y: v.get("sy").and_then(Value::as_f64).unwrap_or(0.0),
        device_scale_factor: v.get("dpr").and_then(Value::as_f64).unwrap_or(1.0),
    })
}

async fn collect_focused_ref(page: &Page, elements: &[Element]) -> Option<String> {
    let res = page
        .cdp_send(cdp_runtime::EvaluateParams {
            expression: "(() => { const a = document.activeElement; if (!a) return null; const r = a.getBoundingClientRect(); return JSON.stringify({x:r.x,y:r.y,w:r.width,h:r.height}); })()".to_owned(),
            return_by_value: Some(true),
            ..Default::default()
        })
        .await
        .ok()?;
    let raw = res.result.get("value").and_then(Value::as_str)?;
    let r: serde_json::Value = serde_json::from_str(raw).ok()?;
    let cx = r.get("x").and_then(Value::as_f64)? + r.get("w").and_then(Value::as_f64)? / 2.0;
    let cy = r.get("y").and_then(Value::as_f64)? + r.get("h").and_then(Value::as_f64)? / 2.0;
    elements
        .iter()
        .find(|e| {
            let bx = e.bbox.x;
            let by = e.bbox.y;
            cx >= bx && cx <= bx + e.bbox.w && cy >= by && cy <= by + e.bbox.h
        })
        .map(|e| e.element_ref.clone())
}

fn ax_string(node: &Value, key: &str) -> Option<String> {
    // AX nodes are like {"name": {"type": "string", "value": "Sign in"}}
    node.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| node.get(key).and_then(Value::as_str).map(str::to_owned))
}

fn ax_bool(node: &Value, key: &str) -> Option<bool> {
    node.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_bool)
        .or_else(|| node.get(key).and_then(Value::as_bool))
}

fn stable_id_hash(role: &str, name: &str, parent_role: &str, sibling_idx: usize) -> String {
    // SPEC D14: sha256(role|0x1F|name|0x1F|parent_role|0x1F|sibling_idx)
    let mut h = Sha256::new();
    h.update(role.as_bytes());
    h.update([0x1F]);
    h.update(name.as_bytes());
    h.update([0x1F]);
    h.update(parent_role.as_bytes());
    h.update([0x1F]);
    h.update(sibling_idx.to_string().as_bytes());
    let out = h.finalize();
    hex::encode(out)
}

fn bbox_from_dom(
    _dom: &Option<cdp_dom_snapshot::CaptureSnapshotReturns>,
    node: &Value,
) -> Option<BBox> {
    // CDP AX nodes can carry `boundingBox` in some Chrome versions; honor that
    // first.
    if let Some(bb) = node.get("boundingBox") {
        let x = bb.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = bb.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let w = bb.get("width").and_then(Value::as_f64).unwrap_or(0.0);
        let h = bb.get("height").and_then(Value::as_f64).unwrap_or(0.0);
        if w > 0.0 || h > 0.0 {
            return Some(BBox { x, y, w, h });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ax_engine::mutation::MutationRecord;

    #[test]
    fn stable_id_is_deterministic() {
        let a = stable_id_hash("button", "Sign in", "form", 0);
        let b = stable_id_hash("button", "Sign in", "form", 0);
        assert_eq!(a, b);
    }

    #[test]
    fn stable_id_differs_on_sibling_idx() {
        let a = stable_id_hash("button", "OK", "form", 0);
        let b = stable_id_hash("button", "OK", "form", 1);
        assert_ne!(a, b);
    }

    // ---------------------------------------------------------------
    // SPEC §10 M2 — delta snapshot tests.
    //
    // Unit tests that exercise the in-process delta machinery without
    // requiring a live Chromium. The CDP-bound paths
    // (`Page::snapshot()`, `Page::snapshot_delta_since`) are exercised
    // end-to-end by `installer/e2e-smoke.sh`.
    // ---------------------------------------------------------------

    fn rec(seq: u64) -> MutationRecord {
        MutationRecord {
            seq,
            kind: "added".into(),
            tag: "DIV".into(),
            cb: String::new(),
        }
    }

    #[test]
    fn overflow_helper_empty_drain_no_overflow() {
        // Empty drain is the normal "no mutations since last snapshot"
        // case — never an overflow.
        assert!(!mutation_log_overflowed(0, &[]));
        assert!(!mutation_log_overflowed(42, &[]));
    }

    #[test]
    fn overflow_helper_contiguous_no_overflow() {
        // Drain starts at anchor + 1: contiguous, no records dropped.
        let drained = vec![rec(6), rec(7), rec(8)];
        assert!(!mutation_log_overflowed(5, &drained));
    }

    #[test]
    fn overflow_helper_anchor_zero_starting_at_one() {
        // Brand-new page: anchor=0 (no prior snapshot), drain starts
        // at seq=1. The first observed mutation must be seq=1; if the
        // smallest is >1, records dropped before we ever drained.
        assert!(!mutation_log_overflowed(0, &[rec(1), rec(2)]));
        assert!(mutation_log_overflowed(0, &[rec(2), rec(3)]));
    }

    #[test]
    fn overflow_helper_gap_detected() {
        // Anchor=5, smallest drained=10 → records 6..=9 were dropped.
        let drained = vec![rec(10), rec(11)];
        assert!(mutation_log_overflowed(5, &drained));
    }

    #[test]
    fn overflow_helper_unordered_drain_uses_min() {
        // The JS bootstrap pushes in order, but the helper must not
        // assume that — it uses min(seq), not first(seq).
        let drained = vec![rec(20), rec(8), rec(15)];
        // anchor=7, min=8 → contiguous.
        assert!(!mutation_log_overflowed(7, &drained));
        // anchor=5, min=8 → gap of 6 and 7.
        assert!(mutation_log_overflowed(5, &drained));
    }

    #[test]
    fn snapshot_response_full_serializes_with_partial_false() {
        let snap = Snapshot {
            snapshot_seq: 1,
            base_seq: 0,
            partial: false,
            anchor_stale: false,
            url: "https://example.com/".into(),
            title: "Example".into(),
            elements: vec![],
            tree: serde_json::json!({"nodes": []}),
            console: vec![],
            exceptions: vec![],
            network: NetworkSummary::default(),
            focused_ref: None,
            viewport: Viewport::default(),
        };
        let s = serde_json::to_value(SnapshotResponse::Full(snap)).unwrap();
        assert_eq!(s.get("partial").and_then(Value::as_bool), Some(false));
        assert_eq!(s.get("base_seq").and_then(Value::as_u64), Some(0));
        assert_eq!(s.get("snapshot_seq").and_then(Value::as_u64), Some(1));
        // SPEC §10 M2 — `anchor_stale` is `false` for happy-path full
        // snapshots (the no-`since_seq` entry point); only the delta
        // fallback paths ever set it `true`.
        assert_eq!(s.get("anchor_stale").and_then(Value::as_bool), Some(false));
        // Full snapshots carry `elements` and `tree`; deltas don't.
        assert!(s.get("elements").is_some());
        assert!(s.get("tree").is_some());
    }

    #[test]
    fn snapshot_anchor_stale_round_trips() {
        // SPEC §10 M2 — when the engine falls back to a full snapshot
        // because the caller's `since_seq` could not be honored
        // (overflow / drain failure / cleared anchor / wrong seq), it
        // sets `anchor_stale: true`. Agents pivot on this to discard
        // their cached element table.
        let snap = Snapshot {
            snapshot_seq: 9,
            base_seq: 5,
            partial: false,
            anchor_stale: true,
            url: "https://example.com/".into(),
            title: "Example".into(),
            elements: vec![],
            tree: serde_json::json!({"nodes": []}),
            console: vec![],
            exceptions: vec![],
            network: NetworkSummary::default(),
            focused_ref: None,
            viewport: Viewport::default(),
        };
        let wire = serde_json::to_string(&SnapshotResponse::Full(snap)).unwrap();
        let back: SnapshotResponse = serde_json::from_str(&wire).unwrap();
        match back {
            SnapshotResponse::Full(s) => {
                assert!(s.anchor_stale);
                assert_eq!(s.base_seq, 5);
                assert_eq!(s.snapshot_seq, 9);
                assert!(!s.partial);
            }
            SnapshotResponse::Delta(_) => panic!("expected Full, got Delta"),
        }
    }

    #[test]
    fn snapshot_anchor_stale_defaults_false_for_legacy_payloads() {
        // SPEC §10 M2 — older clients won't send the `anchor_stale`
        // field; serde must default it to `false` so a legacy full
        // snapshot stored before this change deserializes cleanly.
        let legacy = serde_json::json!({
            "snapshot_seq": 3,
            "base_seq": 0,
            "partial": false,
            "url": "u",
            "title": "t",
            "elements": [],
            "tree": {"nodes": []},
            "console": [],
            "exceptions": [],
            "network": {"in_flight": 0, "completed_since_last": 0, "failed_since_last": 0},
            "focused_ref": null,
            "viewport": {"w": 0.0, "h": 0.0, "scroll_x": 0.0, "scroll_y": 0.0, "device_scale_factor": 0.0}
        });
        let snap: Snapshot = serde_json::from_value(legacy).unwrap();
        assert!(!snap.anchor_stale);
    }

    #[test]
    fn snapshot_response_delta_serializes_with_partial_true() {
        let delta = SnapshotDelta {
            snapshot_seq: 7,
            base_seq: 6,
            partial: true,
            mutations: vec![rec(1), rec(2)],
            url: "https://example.com/".into(),
            title: "Example".into(),
        };
        let s = serde_json::to_value(SnapshotResponse::Delta(delta)).unwrap();
        assert_eq!(s.get("partial").and_then(Value::as_bool), Some(true));
        assert_eq!(s.get("base_seq").and_then(Value::as_u64), Some(6));
        assert_eq!(s.get("snapshot_seq").and_then(Value::as_u64), Some(7));
        // Delta omits the heavy fields.
        assert!(s.get("elements").is_none());
        assert!(s.get("tree").is_none());
        // Mutations array preserved with seq + kind.
        let muts = s.get("mutations").and_then(Value::as_array).unwrap();
        assert_eq!(muts.len(), 2);
        assert_eq!(muts[0].get("seq").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn snapshot_response_round_trips_through_untagged_enum() {
        // Any consumer reading the wire must be able to deserialize
        // both branches via the untagged enum. The discriminant is
        // structural: deltas have `mutations`, full snapshots have
        // `elements` + `tree`.
        let delta = SnapshotDelta {
            snapshot_seq: 2,
            base_seq: 1,
            partial: true,
            mutations: vec![rec(42)],
            url: "u".into(),
            title: "t".into(),
        };
        let wire = serde_json::to_string(&SnapshotResponse::Delta(delta)).unwrap();
        let back: SnapshotResponse = serde_json::from_str(&wire).unwrap();
        match back {
            SnapshotResponse::Delta(d) => {
                assert_eq!(d.base_seq, 1);
                assert_eq!(d.snapshot_seq, 2);
                assert_eq!(d.mutations.len(), 1);
                assert_eq!(d.mutations[0].seq, 42);
            }
            SnapshotResponse::Full(_) => panic!("expected Delta, got Full"),
        }
    }
}
