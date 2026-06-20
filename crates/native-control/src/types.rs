//! Public types — wire-shape mirror of `browser_engine::Element` with native-
//! app context (`app_id`, `pid`, `bundle_id`) replacing `frame_id`.
//!
//! SPEC §11 V2: "AX tree mirrors the web AX tree we already render" —
//! `ElementState` and `BBox` are byte-identical to `browser_engine`'s shapes
//! so a downstream agent treats refs from `page.snapshot` and `app.snapshot`
//! interchangeably.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Bundle id (e.g. `com.apple.calculator`). Stable across launches.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AppId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AppId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// One app in [`crate::list_apps`]'s output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppHandle {
    pub bundle_id: String,
    pub pid: i32,
    pub name: String,
    /// True if this app's pid matches `NSWorkspace.frontmostApplication`.
    pub has_focus: bool,
}

/// Bounding box in screen pixels (top-left origin, matches CGRect convention).
///
/// Same wire shape as `browser_engine::Element::bbox` so a single agent
/// frontend renders both without a translation layer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl BBox {
    pub fn is_zero_area(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// Per-element accessibility state. Same shape as `browser_engine::ElementState`.
///
/// All fields default to `None` / `false` when the AX attribute is absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElementState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(default)]
    pub disabled: bool,
}

/// One node in [`AppSnapshot::elements`]. Mirrors `browser_engine::Element`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppElement {
    pub index: usize,
    /// Per-snapshot label, e.g. `"e0"`, `"e1"`, … Scoped to
    /// `(app_id, snapshot_seq)`.
    #[serde(rename = "ref")]
    pub element_ref: String,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub state: ElementState,
    pub bbox: BBox,
    pub interactable: bool,
    /// Bundle id this element belongs to.
    pub app_id: String,
    /// SPEC §1 D14 hash —
    /// `sha256(role | 0x1F | name | 0x1F | parent_role | 0x1F | sibling_index)`.
    /// Byte-identical to `browser_engine::Element::stable_id`. Survives
    /// non-structural reflows; a downstream agent can persist this across
    /// snapshots while `element_ref` is per-snapshot.
    pub stable_id: String,

    /// Internal: AX path from app root used to re-locate the element on
    /// action dispatch. Skipped from serialization (broker-internal only).
    #[serde(skip)]
    pub(crate) ax_path: Vec<u32>,
}

/// Snapshot of one application's AX tree. Returned by `app.snapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub snapshot_seq: u64,
    pub app_id: String,
    pub bundle_id: String,
    pub pid: i32,
    pub title: String,
    /// Ref of the focused element, if any. Mirrors `browser_engine::Snapshot::focused_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_ref: Option<String>,
    pub elements: Vec<AppElement>,
    /// Best-effort hierarchical projection of the AX tree (for debugging /
    /// agent context). Schema is not load-bearing — refs are.
    pub tree: serde_json::Value,
    /// Set when the walker hit a depth or node-count cap. Never set silently —
    /// the walker also emits a `tracing::warn` on truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_at: Option<&'static str>,
}

/// Errors from this crate. Map cleanly to broker `RouterError` codes.
#[derive(Debug, Error)]
pub enum NativeControlError {
    /// `AXIsProcessTrusted()` returned false. The broker translates this to
    /// JSON-RPC `-32009 PermissionDenied` with `data.settings_url` populated.
    #[error("Accessibility permission missing — open System Settings → Privacy & Security → Accessibility ({settings_url}) and grant the broker")]
    PermissionMissing { settings_url: &'static str },

    /// Screen Recording permission is required for Notes / QuickLook AX text.
    /// The settings deeplink jumps to the Screen Recording pane.
    #[error("Screen Recording permission missing — open System Settings → Privacy & Security → Screen Recording ({settings_url}) and grant the broker")]
    ScreenRecordingMissing { settings_url: &'static str },

    /// No running application matched `bundle_id`. Maps to `-32002 TabNotFound`
    /// (target-not-found semantics; we reuse the page-side code so existing
    /// clients understand it).
    #[error("no running application with bundle id {bundle_id:?}")]
    AppNotFound { bundle_id: String },

    /// Element ref came from an older snapshot. Maps to `-32004 ElementStale`.
    #[error("ref {r:?} not present in current snapshot")]
    RefStale { r: String },

    /// Element exists but has zero-area bbox or no actionable AX action.
    /// Maps to `-32005 ElementNotActionable`.
    #[error("element {r:?} is not actionable: {reason}")]
    NotActionable { r: String, reason: &'static str },

    /// Underlying AX FFI returned a non-success code. The numeric value is
    /// the `AXError` discriminant from `ApplicationServices/HIServices.h`.
    #[error("AX call failed (AXError={0})")]
    AxError(i32),

    /// AppleScript execution failed.
    #[error("AppleScript: {msg}")]
    AppleScript { msg: String },

    /// `app_eval` body contained an activate-target verb that would steal
    /// focus from the user. SPEC §5 enforcement.
    #[error("AppleScript bodies that activate the target app are forbidden by SPEC §5 ({reason})")]
    ActivateForbidden { reason: &'static str },

    /// SPEC §12 U13 — bundle id is in the session's `app_blocklist`.
    #[error("app {bundle_id:?} is blocked by session policy (app_blocklist)")]
    Blocked { bundle_id: String },

    /// SPEC §12 U13 — focus-stealing verb (window.raise, dock.click on
    /// not-yet-running app, spotlight.open) requires `capabilities:["focus_steal"]`
    /// AND the call's `confirm: true` flag.
    #[error(
        "focus-stealing action requires capabilities:[\"focus_steal\"] + confirm=true: {what}"
    )]
    FocusStealForbidden { what: &'static str },

    /// SPEC §12 U6 — A private symbol (CGS_*, TIS*, DFR*) was unavailable.
    /// Either the symbol was removed in this macOS version OR the crate was
    /// built without `private-apis` feature.
    #[error("private API unavailable: {what}")]
    PrivateApiUnavailable { what: &'static str },

    /// Core Graphics Services private API returned a non-zero error code.
    #[error("CoreGraphics Services error: {0}")]
    Cgs(i32),

    /// Text Input Source services error.
    #[error("Text Input Source error: {0}")]
    Tis(i32),

    /// I/O error wrapping the `osascript` subprocess.
    #[error("osascript subprocess: {0}")]
    Io(String),

    /// Generic IO timeout (osascript stalled etc).
    #[error("timeout: {0}")]
    Timeout(&'static str),

    /// Internal invariant violated.
    #[error("internal: {0}")]
    Internal(String),

    /// Crate is a no-op stub on this platform.
    #[error("native-control is only implemented on macOS")]
    UnsupportedPlatform,
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — auxiliary types for menu / dock / window / spaces / etc.
// ---------------------------------------------------------------------------

/// One menu item in [`crate::menu::list`]'s output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuItem {
    /// Path of titles from the menu bar root, e.g. `["File", "Open…"]`.
    pub path: Vec<String>,
    /// Concatenated text for display, e.g. `"File > Open…"`.
    pub display: String,
    /// True if the item dispatches an action (i.e. has `kAXPressAction`).
    pub interactable: bool,
    /// True if the item is currently disabled (kAXEnabledAttribute = false).
    pub disabled: bool,
    /// Keyboard equivalent, if exposed via kAXMenuItemCmdChar+modifiers.
    /// Best-effort string; absent fields produce an empty string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
}

/// One window handle in [`crate::window::list`]'s output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowHandle {
    /// Per-window opaque id scoped to (bundle_id, snapshot). Format `"w{idx}"`.
    pub window_id: String,
    pub bundle_id: String,
    pub title: String,
    pub bbox: BBox,
    pub minimized: bool,
    pub fullscreen: bool,
    pub main: bool,
}

/// One dock item in [`crate::dock::list`]'s output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockItem {
    /// Per-snapshot opaque id, format `"d{idx}"`.
    pub dock_id: String,
    pub label: String,
    /// True if this dock item represents a currently-running app.
    pub running: bool,
    /// Best-effort bundle id resolved from the dock item title; may be empty
    /// for non-app entries (folders, files, separators).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bundle_id: String,
}

/// One Mission Control space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub space_id: u64,
    pub name: String,
    pub active: bool,
}

/// SPEC §12 U7 — clipboard payload kinds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardKind {
    String,
    Files,
    Image,
    Other,
}

/// One snapshotted clipboard entry. Values are NOT inlined for binary types;
/// callers issue a typed read (`clipboard.read_image`) to fetch payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardItem {
    /// `[NSPasteboard generalPasteboard].changeCount` value when this entry
    /// was captured. Strictly increasing per pasteboard.
    pub change_count: i64,
    pub timestamp_ms: u64,
    /// Apple UTIs present in the entry (e.g. `["public.utf8-plain-text"]`).
    pub types: Vec<String>,
    /// Primary kind (best-fit single-classification).
    pub kind: ClipboardKind,
    /// Inline text payload, if `kind == String` and not redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// File paths, if `kind == Files`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// True if a privacy redaction rule fired against this entry.
    /// Inline `text` is omitted when redacted.
    #[serde(default)]
    pub redacted: bool,
}

/// SPEC §12 U13 — per-session privacy policy. Mirror of the protocol-level
/// `redact_patterns` / `app_blocklist` from `session.register`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivacyPolicy {
    /// Substring-any list of bundle ids that the session is forbidden to
    /// touch via any `app.*` call. No globbing.
    #[serde(default)]
    pub app_blocklist: Vec<String>,
    /// Regex patterns that, when matched against clipboard text or AppElement
    /// values, redact the entry. Compiled once and cached on the controller.
    #[serde(default)]
    pub redact_patterns: Vec<String>,
}

// ---------------------------------------------------------------------------
// SPEC §12 — AX events (`app.subscribe`)
// ---------------------------------------------------------------------------

/// One AX event delivered to a subscriber.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxEvent {
    pub bundle_id: String,
    pub topic: AxEventTopic,
    pub timestamp_ms: u64,
    /// Best-effort element ref against the most-recent snapshot, if the
    /// observer can match the element. Absent when no snapshot exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_ref: Option<String>,
    /// Element role, name, value snapshot (if available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Topics supported by `app.subscribe`. Mirror of the AX notification
/// constants we observe.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AxEventTopic {
    /// `kAXValueChangedNotification` — text fields, sliders, etc.
    ValueChanged,
    /// `kAXFocusedUIElementChangedNotification`.
    FocusedChanged,
    /// `kAXWindowCreatedNotification`.
    WindowCreated,
    /// `kAXUIElementDestroyedNotification` (windows only).
    WindowDestroyed,
    /// `kAXSelectedTextChangedNotification`.
    SelectionChanged,
}

impl AxEventTopic {
    /// Map a snake-case topic string from JSON into a topic enum.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "value_changed" => AxEventTopic::ValueChanged,
            "focused_changed" => AxEventTopic::FocusedChanged,
            "window_created" => AxEventTopic::WindowCreated,
            "window_destroyed" => AxEventTopic::WindowDestroyed,
            "selection_changed" => AxEventTopic::SelectionChanged,
            _ => return None,
        })
    }

    /// All supported topics — used to advertise subscribe coverage.
    pub fn all() -> &'static [AxEventTopic] {
        &[
            AxEventTopic::ValueChanged,
            AxEventTopic::FocusedChanged,
            AxEventTopic::WindowCreated,
            AxEventTopic::WindowDestroyed,
            AxEventTopic::SelectionChanged,
        ]
    }
}

/// Opaque subscription handle. Drop = unsubscribe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxSubscription {
    pub subscription_id: String,
    pub bundle_id: String,
    pub topics: Vec<AxEventTopic>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bbox_zero_area_detection() {
        assert!(BBox {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0
        }
        .is_zero_area());
        assert!(BBox {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 10.0
        }
        .is_zero_area());
        assert!(BBox {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 0.0
        }
        .is_zero_area());
        assert!(!BBox {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0
        }
        .is_zero_area());
        assert!(BBox {
            x: 0.0,
            y: 0.0,
            w: -1.0,
            h: 1.0
        }
        .is_zero_area());
    }

    #[test]
    fn app_id_roundtrips() {
        let a = AppId::from("com.apple.calculator");
        assert_eq!(a.as_str(), "com.apple.calculator");
        let s: String = a.0.clone();
        assert_eq!(AppId::from(s).as_str(), "com.apple.calculator");
    }

    #[test]
    fn app_element_serializes_with_ref_alias() {
        let e = AppElement {
            index: 0,
            element_ref: "e0".into(),
            role: "AXButton".into(),
            name: "5".into(),
            value: None,
            description: None,
            state: ElementState::default(),
            bbox: BBox {
                x: 1.0,
                y: 2.0,
                w: 30.0,
                h: 30.0,
            },
            interactable: true,
            app_id: "com.apple.calculator".into(),
            stable_id: "deadbeef".into(),
            ax_path: vec![0, 1, 2],
        };
        let v = serde_json::to_value(&e).unwrap();
        // `ref` is the wire name; `element_ref` is the Rust field name.
        assert_eq!(v["ref"], json!("e0"));
        assert_eq!(v["role"], json!("AXButton"));
        assert_eq!(v["name"], json!("5"));
        assert_eq!(v["app_id"], json!("com.apple.calculator"));
        // `ax_path` must be skipped — broker-internal.
        assert!(v.get("ax_path").is_none());
    }
}
