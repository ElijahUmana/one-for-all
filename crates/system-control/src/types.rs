//! Public types: capabilities, errors, DTOs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Per-session capability flag for a U8 sub-surface.
///
/// String reprs are stable (broker contract): `camera`, `mic`, `screen`,
/// `bluetooth`, `raw_usb`. The variant `None` is used for tools that don't
/// require a TCC-gated capability (process list, network info, battery,
/// spotlight, metadata, fsevents).
///
/// IMPORTANT: even `Capability::None` tools still require the session to be
/// registered with at least the bare `"system"` capability — a session
/// registered with no capabilities at all (the default) cannot reach any
/// `system.*` handler. That bare gate is enforced in `require_system` in
/// `crates/broker/src/router.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// `Camera` — needed for `system.camera.*`.
    Camera,
    /// `Microphone` — needed for `system.mic.capture` and any audio-input
    /// path of `system.audio.*`.
    Microphone,
    /// `Screen Recording` — needed for `system.screen.capture_region`,
    /// `system.audio.capture_to_file` (system audio via SCK).
    Screen,
    /// `Bluetooth` — needed for `system.bluetooth.*`.
    Bluetooth,
    /// `RawUsb` — needed for `system.usb.devices` enumeration. Not TCC-gated
    /// on macOS (no system prompt) but still capability-gated to keep an
    /// agent without explicit grant out of the IOKit USB stack.
    RawUsb,
    /// No additional TCC permission required — only the bare session
    /// capability is checked.
    None,
}

impl Capability {
    /// Stable string used in `session.register {capabilities: [...]}`.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Capability::Camera => "camera",
            Capability::Microphone => "mic",
            Capability::Screen => "screen",
            Capability::Bluetooth => "bluetooth",
            Capability::RawUsb => "raw_usb",
            Capability::None => "system",
        }
    }

    /// macOS Privacy & Security pane deeplink for this capability.
    #[must_use]
    pub const fn settings_deeplink(&self) -> &'static str {
        match self {
            Capability::Camera => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
            }
            Capability::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Capability::Screen => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Capability::Bluetooth => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth"
            }
            Capability::RawUsb | Capability::None => {
                "x-apple.systempreferences:com.apple.preference.security"
            }
        }
    }
}

/// Common error shape for every U8 fn. Mirrors `NativeControlError` so the
/// broker can map both with the same dispatch helper.
#[derive(Debug, Error)]
pub enum SystemError {
    /// The OS-level permission has not been granted for this capability.
    /// `settings_url` is the System Settings deeplink the broker hands back
    /// in the JSON-RPC error `data` payload.
    #[error("permission missing: {capability:?}")]
    PermissionMissing {
        capability: Capability,
        settings_url: &'static str,
    },

    /// A device id, path, pid, or interface name was not present.
    #[error("not found: {0}")]
    NotFound(String),

    /// Caller passed a malformed argument (negative dimensions, bad pid, etc).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The macOS API itself returned a non-zero status. `code` is the raw
    /// `OSStatus` / `IOReturn` / `errno`. `domain` names the API family.
    #[error("{domain} error: code={code}")]
    Os { domain: &'static str, code: i64 },

    /// Underlying I/O error (file write, socket).
    #[error("io: {0}")]
    Io(String),

    /// A subprocess (mdfind, mdls, lsof) exited non-zero.
    #[error("subprocess: {0}")]
    Subprocess(String),

    /// Operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Any internal invariant violation that doesn't fit the above.
    #[error("internal: {0}")]
    Internal(String),

    /// The platform isn't macOS — most of U8 is macOS-only.
    #[error("unsupported on this platform")]
    UnsupportedPlatform,
}

pub type SystemResult<T> = std::result::Result<T, SystemError>;

impl From<std::io::Error> for SystemError {
    fn from(e: std::io::Error) -> Self {
        SystemError::Io(e.to_string())
    }
}

// ---------- DTOs ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: u32,
    pub uid: String,
    pub name: String,
    pub direction: AudioDirection,
    pub is_default: bool,
    pub sample_rate: f64,
    pub channels: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Display {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub is_main: bool,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BluetoothDevice {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i32>,
    pub paired: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbDevice {
    pub vendor_id: u16,
    pub product_id: u16,
    pub vendor_name: Option<String>,
    pub product_name: Option<String>,
    pub serial: Option<String>,
    pub speed: Option<String>,
    pub location_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// `true` if a battery is present at all (Mac mini / Mac Pro = false).
    pub has_battery: bool,
    /// 0.0–1.0; `None` when no battery.
    pub level: Option<f32>,
    pub is_charging: Option<bool>,
    pub is_charged: Option<bool>,
    pub time_to_empty_min: Option<i32>,
    pub time_to_full_min: Option<i32>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub display_name: Option<String>,
    pub kind: String,
    pub mac: Option<String>,
    pub mtu: Option<u32>,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkRoute {
    pub destination: String,
    pub gateway: Option<String>,
    pub netmask: Option<String>,
    pub interface: Option<String>,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConnection {
    pub pid: i32,
    pub command: String,
    pub protocol: String,
    pub local: String,
    pub remote: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    pub start_time_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub path: String,
    pub start_time_unix_ms: i64,
    pub cpu_user_us: u64,
    pub cpu_system_us: u64,
    pub vsize_bytes: u64,
    pub rss_bytes: u64,
}

/// Bitset flag for a single fsevent batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEventFlag {
    Created,
    Removed,
    Renamed,
    Modified,
    InodeMetaModified,
    OwnerChanged,
    XattrChanged,
    IsFile,
    IsDir,
    IsSymlink,
    MountPoint,
    UnmountPoint,
    HistoryDone,
    RootChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEvent {
    pub watch_id: String,
    pub path: PathBuf,
    pub flags: Vec<FsEventFlag>,
    pub event_id: u64,
    pub ts_ns: u128,
}
