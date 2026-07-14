use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TermError {
    #[error("terminal session not found: {0}")]
    SessionNotFound(String),
    #[error("invalid terminal dimensions rows={rows} cols={cols}")]
    InvalidSize { rows: u16, cols: u16 },
    #[error("missing sandbox profile for sandboxed terminal spawn")]
    MissingSandboxProfile,
    #[error("shell path is empty")]
    EmptyShell,
    #[error("cwd escapes sandbox rootfs: {0}")]
    CwdOutsideRootfs(String),
    #[error("cwd does not exist in sandbox rootfs: {0}")]
    CwdNotFound(String),
    #[error("mouse tracking is disabled for this terminal session")]
    MouseTrackingDisabled,
    #[error(
        "mouse coordinates are out of bounds for rows={rows} cols={cols}: row={row} col={col}"
    )]
    MouseOutOfBounds {
        rows: u16,
        cols: u16,
        row: u16,
        col: u16,
    },
    #[error("terminal output is not valid utf-8 for this operation")]
    InvalidUtf8,
    #[error("unsupported signal on this platform: {0}")]
    UnsupportedSignal(String),
    #[error("io: {0}")]
    Io(String),
    #[error("spawn: {0}")]
    Spawn(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermSessionId(pub String);

impl TermSessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TermSessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TermSessionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSandbox {
    pub rootfs: std::path::PathBuf,
    pub user_data_dir: std::path::PathBuf,
    pub profile_path: std::path::PathBuf,
    pub seed_plan_path: std::path::PathBuf,
    pub inherit: Vec<sandbox::InheritSpec>,
    pub network_outbound: bool,
    pub native_ax_allowed: bool,
    pub enforced: bool,
}

#[derive(Debug, Clone)]
pub struct SpawnTerminalRequest {
    pub shell: String,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub sandbox: Option<SessionSandbox>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnTerminalResult {
    pub session_id: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone)]
pub struct TermReadChunk {
    pub data: Vec<u8>,
    pub eof: bool,
    pub dropped_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermCursor {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TermColor {
    Default,
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermStyle {
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermAttrRun {
    pub start_col: u16,
    pub end_col: u16,
    pub style: TermStyle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermSnapshotRow {
    pub row: u16,
    pub text: String,
    pub wrapped: bool,
    pub attrs: Vec<TermAttrRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermScrollbackLine {
    pub index: usize,
    pub text: String,
    pub wrapped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TermMouseMode {
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TermMouseEncoding {
    Default,
    Utf8,
    Sgr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermAltScreenState {
    pub active: bool,
    pub mouse_mode: TermMouseMode,
    pub mouse_encoding: TermMouseEncoding,
    pub cursor_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermExitState {
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermSnapshot {
    pub snapshot_seq: u64,
    pub session_id: String,
    pub rows: u16,
    pub cols: u16,
    pub cursor: TermCursor,
    pub alt_screen_active: bool,
    pub mouse_mode: TermMouseMode,
    pub mouse_encoding: TermMouseEncoding,
    pub cursor_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    pub visible_rows: Vec<TermSnapshotRow>,
    pub exit: TermExitState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseEventRequest {
    pub row: u16,
    pub col: u16,
    pub kind: MouseEventKind,
    #[serde(default)]
    pub button: Option<MouseButton>,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub ctrl: bool,
}

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output(TermOutputEvent),
    Exit(TermExitEvent),
}

#[derive(Debug, Clone)]
pub struct TermOutputEvent {
    pub session_id: String,
    pub seq: u64,
    pub data: Vec<u8>,
    pub dropped_bytes: u64,
    pub eof: bool,
}

#[derive(Debug, Clone)]
pub struct TermExitEvent {
    pub session_id: String,
    pub exit: TermExitState,
}
