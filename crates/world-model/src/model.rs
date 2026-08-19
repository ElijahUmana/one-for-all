use native_control::{AppHandle, AxEvent, WindowHandle};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use system_control::types::Display;
use system_control::FsEvent;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Coherence {
    Coherent,
    /// Structure and pixels came from different generations / epochs.
    Skewed {
        stale_ms: u64,
        reason: String,
    },
    /// One stream is unavailable; readers still get the rest of the world.
    Degraded {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotSource {
    AxStructure,
    Capture,
    Lifecycle,
    WindowServer,
    CursorInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CursorState {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InputEventKind {
    MouseMove,
    MouseDown,
    MouseUp,
    KeyDown,
    KeyUp,
    Scroll,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputEvent {
    pub at_ms: u64,
    pub kind: InputEventKind,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocusedWindow {
    pub bundle_id: String,
    pub window_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotSummary {
    pub snapshot_seq: u64,
    pub focused_ref: Option<String>,
    pub title: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrameRef {
    pub shm_path: String,
    pub slot_seq: u64,
    pub slot_index: u32,
    pub offset: u32,
    pub len: u32,
    pub ts_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AxEventSummary {
    pub bundle_id: String,
    pub topic: String,
    pub timestamp_ms: u64,
    pub role: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsEventSummary {
    pub watch_id: String,
    pub path: String,
    pub flags: Vec<String>,
    pub event_id: u64,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppWorld {
    pub handle: AppHandle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<WindowHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayWorld {
    pub display: Display,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_frame: Option<FrameRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub generation: u64,
    pub captured_at_ms: u64,
    #[serde(default)]
    pub sources: Vec<SnapshotSource>,
    pub coherence: Coherence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppWorld>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub displays: Vec<DisplayWorld>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_window: Option<FocusedWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_ax_events: Vec<AxEventSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_fs_events: Vec<FsEventSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_input: Vec<InputEvent>,
}

impl WorldSnapshot {
    pub fn empty() -> Self {
        Self {
            generation: 0,
            captured_at_ms: now_ms(),
            sources: Vec::new(),
            coherence: Coherence::Degraded {
                reason: "world model not yet primed".to_string(),
            },
            apps: Vec::new(),
            displays: Vec::new(),
            focused_window: None,
            cursor: None,
            recent_ax_events: Vec::new(),
            recent_fs_events: Vec::new(),
            recent_input: Vec::new(),
        }
    }
}

impl From<AxEvent> for AxEventSummary {
    fn from(value: AxEvent) -> Self {
        Self {
            bundle_id: value.bundle_id,
            topic: format!("{:?}", value.topic),
            timestamp_ms: value.timestamp_ms,
            role: value.role,
            name: value.name,
        }
    }
}

impl From<FsEvent> for FsEventSummary {
    fn from(value: FsEvent) -> Self {
        Self {
            watch_id: value.watch_id,
            path: value.path.to_string_lossy().into_owned(),
            flags: value
                .flags
                .into_iter()
                .map(|f| format!("{:?}", f))
                .collect(),
            event_id: value.event_id,
            at_ms: (value.ts_ns / 1_000_000) as u64,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
