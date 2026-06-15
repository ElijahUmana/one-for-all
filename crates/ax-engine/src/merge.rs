//! AX tree + DOMSnapshot merge.
//!
//! Owned by `ax-engine`. Pure functions over `serde_json::Value` shapes so
//! the algorithm can be unit-tested with fixture data.
//!
//! # Inputs
//!
//! - `ax`: the body of `Accessibility.getFullAXTree { fetchRelatives: true }`.
//!   Shape: `{"nodes": [AXNode, …]}` where each node has `nodeId`, `role`,
//!   `name`, `properties`, `childIds`, `backendDOMNodeId`.
//! - `dom`: the body of `DOMSnapshot.captureSnapshot`. Shape:
//!   `{"documents": [{"nodes": {…columnar layout…}, "layout": {…}, "textBoxes": …}]}`.
//!
//! # Output
//!
//! [`Snapshot`] containing a dense `Vec<Element>` where `index` matches
//! position. Each element carries the stable-id hash from
//! [`crate::index::StableId::compute`] plus the SPEC §7 element fields.
//!
//! # Filtering
//!
//! "Interactable" follows SPEC §7 + the team-lead brief: the role is one of
//! the actionable AX roles (button, link, textbox, …) or the element is
//! contenteditable. Hidden elements (zero-area bbox, `display:none`,
//! `visibility:hidden`) are dropped.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::index::StableId;

/// Bounding rectangle in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Viewport state as seen by the page (SPEC §10 M1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub w: f64,
    pub h: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub device_scale_factor: f64,
}

/// Single console message captured by the engine since the previous
/// snapshot (SPEC §10 M1). Browser-engine fills these in; ax-engine just
/// carries the typed shape so the wire schema is locked in one place.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsoleMessage {
    pub level: String,
    pub text: String,
    pub source: String,
    pub ts_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// Unhandled exception record (SPEC §10 M1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExceptionRecord {
    pub text: String,
    pub stack: String,
    pub ts_ms: f64,
}

/// Network rollup since the previous snapshot (SPEC §10 M1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkSummary {
    pub in_flight: u32,
    pub completed_since_last: u32,
    pub failed_since_last: u32,
}

/// Per-element accessibility state — SPEC §7.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ElementState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressed: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    pub disabled: bool,
}

/// One row of the snapshot's `elements[]` — wire-shape per SPEC §7.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub index: u32,
    /// Per SPEC: `"e<N>"`. Scoped to `(tab_id, snapshot_seq)` upstream;
    /// at this layer we just emit the index-derived form.
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub state: ElementState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
    pub interactable: bool,
    /// The frame this element belongs to. Empty string for the root
    /// document; populated by [`crate::iframe`] when merging child frames.
    pub frame_id: String,
    /// Stable identity hash (sha256, see [`crate::index`]).
    pub stable_id: StableId,
    /// Backend DOM node id — useful for click resolution by the engine.
    pub backend_node_id: i64,
}

/// One AX snapshot for a single tab. Multiple frames are spliced in by
/// [`crate::iframe`].
///
/// SPEC §10 M1: snapshots include console, exceptions, network rollup,
/// focused_ref, and viewport. The base merge() leaves M1 fields at
/// defaults; the engine layer fills them in from observed events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Per SPEC §7: monotonic per-tab snapshot sequence number. Filled in
    /// by the engine layer; defaults to 0 here.
    #[serde(default)]
    pub snapshot_seq: u64,
    pub elements: Vec<Element>,
    pub tree: Option<Value>,
    pub url: String,
    pub title: String,
    /// Console messages observed since the previous snapshot.
    #[serde(default)]
    pub console: Vec<ConsoleMessage>,
    /// Unhandled exceptions observed since the previous snapshot.
    #[serde(default)]
    pub exceptions: Vec<ExceptionRecord>,
    /// Network activity rollup since the previous snapshot.
    #[serde(default)]
    pub network: NetworkSummary,
    /// `ref` of the currently focused element, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_ref: Option<String>,
    /// Viewport metrics. Default = zeros (filled by engine).
    #[serde(default)]
    pub viewport: Viewport,
}

/// AX roles considered interactable by default. Lowercased.
const INTERACTABLE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "switch",
    "slider",
    "spinbutton",
    "treeitem",
    "option",
];

fn ax_value_str(node: &Value, key: &str) -> Option<String> {
    node.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // Some AX nodes use shape { "type": "computedString", "value": "..." }
            node.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        })
}

fn ax_property_bool(props: &[Value], name: &str) -> Option<bool> {
    for p in props {
        if p.get("name").and_then(Value::as_str) == Some(name) {
            if let Some(v) = p.get("value").and_then(|v| v.get("value")) {
                if let Some(b) = v.as_bool() {
                    return Some(b);
                }
                if let Some(s) = v.as_str() {
                    return Some(s.eq_ignore_ascii_case("true"));
                }
            }
        }
    }
    None
}

fn ax_property_value(props: &[Value], name: &str) -> Option<Value> {
    for p in props {
        if p.get("name").and_then(Value::as_str) == Some(name) {
            return p.get("value").and_then(|v| v.get("value")).cloned();
        }
    }
    None
}

fn collect_state(props: &[Value]) -> ElementState {
    ElementState {
        checked: ax_property_value(props, "checked"),
        expanded: ax_property_bool(props, "expanded"),
        pressed: ax_property_value(props, "pressed"),
        selected: ax_property_bool(props, "selected"),
        disabled: ax_property_bool(props, "disabled").unwrap_or(false),
    }
}

/// Index a flat `documents[0].nodes` columnar dump into a `backendNodeId →
/// bbox` map. DOMSnapshot's "captureSnapshot" returns its data column-wise
/// for compactness; we walk `nodes.backendNodeId[i]` and `layout.nodeIndex[j] /
/// layout.bounds[j]` to join them.
fn index_dom_snapshot_bboxes(dom: &Value) -> HashMap<i64, Rect> {
    let mut out = HashMap::new();
    let docs = match dom.get("documents").and_then(Value::as_array) {
        Some(d) => d,
        None => return out,
    };
    if docs.is_empty() {
        return out;
    }
    let doc = &docs[0];
    let backend_ids = match doc
        .get("nodes")
        .and_then(|n| n.get("backendNodeId"))
        .and_then(Value::as_array)
    {
        Some(a) => a,
        None => return out,
    };
    let layout = match doc.get("layout") {
        Some(l) => l,
        None => return out,
    };
    let node_index = match layout.get("nodeIndex").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    let bounds = match layout.get("bounds").and_then(Value::as_array) {
        Some(a) => a,
        None => return out,
    };
    for (i, ni) in node_index.iter().enumerate() {
        let node_idx = ni.as_u64().map(|v| v as usize);
        let b = bounds.get(i).and_then(Value::as_array);
        if let (Some(idx), Some(rect)) = (node_idx, b) {
            if rect.len() < 4 {
                continue;
            }
            let backend = backend_ids.get(idx).and_then(Value::as_i64);
            if let Some(bid) = backend {
                let r = Rect {
                    x: rect[0].as_f64().unwrap_or(0.0),
                    y: rect[1].as_f64().unwrap_or(0.0),
                    w: rect[2].as_f64().unwrap_or(0.0),
                    h: rect[3].as_f64().unwrap_or(0.0),
                };
                out.insert(bid, r);
            }
        }
    }
    out
}

/// Determine if an element is "interactable" given role + state.
pub(crate) fn is_interactable(role: &str, state: &ElementState) -> bool {
    if state.disabled {
        return false;
    }
    let r = role.to_ascii_lowercase();
    INTERACTABLE_ROLES.iter().any(|x| *x == r) || r == "menuitemradio"
}

/// Compute `sibling_index_within_same_role` for every node based on the AX
/// tree topology. Returns a map of `nodeId → (parent_role, sibling_index)`.
fn compute_sibling_indices(
    nodes: &[&Value],
    by_id: &BTreeMap<&str, usize>,
) -> HashMap<String, (String, u32)> {
    let mut out: HashMap<String, (String, u32)> = HashMap::new();
    for n in nodes {
        let id = match n.get("nodeId").and_then(Value::as_str) {
            Some(s) => s,
            None => continue,
        };
        let parent_role = n
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|pid| by_id.get(pid))
            .and_then(|&pi| nodes.get(pi))
            .and_then(|p| ax_value_str(p, "role"))
            .unwrap_or_default();

        // Count earlier siblings with the same role under the same parent.
        let my_role = ax_value_str(n, "role").unwrap_or_default();
        let parent_id_opt = n.get("parentId").and_then(Value::as_str);
        let mut idx = 0u32;
        if let Some(pid) = parent_id_opt {
            if let Some(&pi) = by_id.get(pid) {
                if let Some(parent) = nodes.get(pi) {
                    if let Some(child_ids) = parent.get("childIds").and_then(Value::as_array) {
                        for cid_v in child_ids {
                            if let Some(cid) = cid_v.as_str() {
                                if cid == id {
                                    break;
                                }
                                if let Some(&ci) = by_id.get(cid) {
                                    if let Some(child) = nodes.get(ci) {
                                        let cr = ax_value_str(child, "role").unwrap_or_default();
                                        if cr == my_role {
                                            idx += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        out.insert(id.to_string(), (parent_role, idx));
    }
    out
}

/// Merge an AX tree with a DOMSnapshot into a [`Snapshot`].
pub fn merge(
    ax: &Value,
    dom: &Value,
    url: impl Into<String>,
    title: impl Into<String>,
) -> Result<Snapshot> {
    let nodes_raw = ax
        .get("nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect::<Vec<&Value>>())
        .unwrap_or_default();
    let by_id: BTreeMap<&str, usize> = nodes_raw
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.get("nodeId").and_then(Value::as_str).map(|s| (s, i)))
        .collect();
    let bboxes = index_dom_snapshot_bboxes(dom);
    let sibling_idx = compute_sibling_indices(&nodes_raw, &by_id);

    let mut elements: Vec<Element> = Vec::new();
    for (i, n) in nodes_raw.iter().enumerate() {
        let role = ax_value_str(n, "role").unwrap_or_default();
        let name = ax_value_str(n, "name").unwrap_or_default();
        if role.is_empty() && name.is_empty() {
            continue;
        }
        let id = match n.get("nodeId").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let backend_node_id = n
            .get("backendDOMNodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let value = ax_value_str(n, "value");
        let description = ax_value_str(n, "description");
        let props_default = Vec::new();
        let props = n
            .get("properties")
            .and_then(Value::as_array)
            .unwrap_or(&props_default);
        let state = collect_state(props);
        let interactable = is_interactable(&role, &state);

        let bbox = bboxes.get(&backend_node_id).copied();
        // Hidden filter: zero-area bbox or no bbox at all + not interactable
        // → skip non-actionable nodes (cuts the list down to the ~hundreds).
        let zero_area = bbox.map(|r| r.w <= 0.0 || r.h <= 0.0).unwrap_or(true);
        if !interactable && (zero_area || (role != "heading" && role != "img")) {
            // Keep some non-interactable but visually meaningful roles.
            // Drop anonymous filler nodes outright.
            if role.is_empty() {
                continue;
            }
        }

        let (parent_role, sib_idx) = sibling_idx
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (String::new(), 0));
        let stable_id = StableId::compute(&role, &name, &parent_role, sib_idx);

        let index = elements.len() as u32;
        elements.push(Element {
            index,
            r#ref: format!("e{index}"),
            role,
            name,
            value,
            description,
            state,
            bbox,
            interactable,
            frame_id: String::new(),
            stable_id,
            backend_node_id,
        });
        let _ = i;
    }

    Ok(Snapshot {
        elements,
        tree: ax.get("nodes").cloned(),
        url: url.into(),
        title: title.into(),
        ..Snapshot::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ax_node(
        id: &str,
        parent: Option<&str>,
        role: &str,
        name: &str,
        backend: i64,
        children: &[&str],
    ) -> Value {
        let mut v = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "internalRole", "value": role},
            "name": {"type": "computedString", "value": name},
            "backendDOMNodeId": backend,
            "childIds": children,
            "properties": [],
        });
        if let Some(p) = parent {
            v["parentId"] = json!(p);
        }
        v
    }

    fn dom_with_bboxes(boxes: &[(i64, [f64; 4])]) -> Value {
        let backend_ids: Vec<i64> = boxes.iter().map(|(b, _)| *b).collect();
        let layout_indices: Vec<usize> = (0..boxes.len()).collect();
        let bounds: Vec<Vec<f64>> = boxes.iter().map(|(_, r)| r.to_vec()).collect();
        json!({
            "documents": [{
                "nodes": { "backendNodeId": backend_ids },
                "layout": {
                    "nodeIndex": layout_indices,
                    "bounds": bounds,
                },
            }]
        })
    }

    #[test]
    fn merge_emits_one_button() {
        let ax = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "page", 1, &["2"]),
                ax_node("2", Some("1"), "button", "Submit", 42, &[]),
            ]
        });
        let dom = dom_with_bboxes(&[(42, [10.0, 20.0, 30.0, 40.0])]);
        let snap = merge(&ax, &dom, "http://x/", "x").unwrap();
        let buttons: Vec<&Element> = snap
            .elements
            .iter()
            .filter(|e| e.role == "button")
            .collect();
        assert_eq!(buttons.len(), 1);
        let b = buttons[0];
        assert_eq!(b.r#ref, format!("e{}", b.index));
        assert_eq!(b.name, "Submit");
        assert!(b.interactable);
        assert_eq!(
            b.bbox,
            Some(Rect {
                x: 10.0,
                y: 20.0,
                w: 30.0,
                h: 40.0
            })
        );
        assert_eq!(b.backend_node_id, 42);
    }

    #[test]
    fn stable_id_does_not_change_when_unrelated_subtree_reorders() {
        // Tree A: focal button is sibling 0 under form
        let ax_a = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "page", 1, &["2", "9"]),
                ax_node("2", Some("1"), "form", "f", 2, &["3"]),
                ax_node("3", Some("2"), "button", "Save", 30, &[]),
                ax_node("9", Some("1"), "navigation", "nav", 9, &[]),
            ]
        });
        // Tree B: same form/button but with extra unrelated nav siblings.
        let ax_b = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "page", 1, &["7", "2", "9"]),
                ax_node("7", Some("1"), "navigation", "navx", 7, &[]),
                ax_node("2", Some("1"), "form", "f", 2, &["3"]),
                ax_node("3", Some("2"), "button", "Save", 30, &[]),
                ax_node("9", Some("1"), "navigation", "nav", 9, &[]),
            ]
        });
        let dom = dom_with_bboxes(&[(30, [0.0, 0.0, 10.0, 10.0])]);
        let a = merge(&ax_a, &dom, "/", "a").unwrap();
        let b = merge(&ax_b, &dom, "/", "b").unwrap();
        let ba = a.elements.iter().find(|e| e.role == "button").unwrap();
        let bb = b.elements.iter().find(|e| e.role == "button").unwrap();
        assert_eq!(ba.stable_id, bb.stable_id);
    }

    #[test]
    fn stable_id_changes_when_sibling_of_same_role_inserted_before() {
        let ax_a = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "p", 1, &["2"]),
                ax_node("2", Some("1"), "form", "f", 2, &["3"]),
                ax_node("3", Some("2"), "button", "Save", 30, &[]),
            ]
        });
        let ax_b = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "p", 1, &["2"]),
                ax_node("2", Some("1"), "form", "f", 2, &["4", "3"]),
                ax_node("4", Some("2"), "button", "Cancel", 40, &[]),
                ax_node("3", Some("2"), "button", "Save", 30, &[]),
            ]
        });
        let dom = dom_with_bboxes(&[(30, [0.0, 0.0, 10.0, 10.0]), (40, [0.0, 0.0, 10.0, 10.0])]);
        let a = merge(&ax_a, &dom, "/", "a").unwrap();
        let b = merge(&ax_b, &dom, "/", "b").unwrap();
        let save_a = a.elements.iter().find(|e| e.name == "Save").unwrap();
        let save_b = b.elements.iter().find(|e| e.name == "Save").unwrap();
        // Save shifted from sibling-of-role 0 to 1, hash must change.
        assert_ne!(save_a.stable_id, save_b.stable_id);
    }

    #[test]
    fn collisions_allowed_two_identical_buttons_share_hash() {
        let ax = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "p", 1, &["2", "3"]),
                ax_node("2", Some("1"), "form", "f", 2, &["10"]),
                ax_node("3", Some("1"), "form", "f", 3, &["20"]),
                ax_node("10", Some("2"), "button", "OK", 100, &[]),
                ax_node("20", Some("3"), "button", "OK", 200, &[]),
            ]
        });
        let dom = dom_with_bboxes(&[(100, [0.0, 0.0, 10.0, 10.0]), (200, [0.0, 0.0, 10.0, 10.0])]);
        let snap = merge(&ax, &dom, "/", "p").unwrap();
        let oks: Vec<&Element> = snap.elements.iter().filter(|e| e.name == "OK").collect();
        assert_eq!(oks.len(), 2);
        assert_eq!(oks[0].stable_id, oks[1].stable_id);
        // index disambiguates within snapshot.
        assert_ne!(oks[0].index, oks[1].index);
    }

    #[test]
    fn refs_are_dense_and_match_index() {
        let ax = json!({
            "nodes": [
                ax_node("1", None, "WebArea", "p", 1, &["2", "3", "4"]),
                ax_node("2", Some("1"), "button", "A", 100, &[]),
                ax_node("3", Some("1"), "button", "B", 200, &[]),
                ax_node("4", Some("1"), "button", "C", 300, &[]),
            ]
        });
        let dom = dom_with_bboxes(&[
            (100, [0.0, 0.0, 1.0, 1.0]),
            (200, [0.0, 0.0, 1.0, 1.0]),
            (300, [0.0, 0.0, 1.0, 1.0]),
        ]);
        let snap = merge(&ax, &dom, "/", "p").unwrap();
        for (i, e) in snap.elements.iter().enumerate() {
            assert_eq!(e.index, i as u32);
            assert_eq!(e.r#ref, format!("e{i}"));
        }
    }
}
