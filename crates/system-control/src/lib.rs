//! SPEC §12 U8 — host system device surface.
//!
//! Every public function lives in a topic submodule (`audio`, `camera`,
//! `screen`, `process`, `network`, `fsevents`, …). The crate is structured to
//! mirror `native-control`:
//!
//!  * macOS-only modules are gated behind `#[cfg(target_os = "macos")]`.
//!  * Cross-platform modules (`spotlight`, `metadata`, `process`, `network`)
//!    still use platform-specific implementations but expose a stable Rust
//!    surface for non-macos builds via [`stub`].
//!  * Every fn returns [`SystemError`] on failure; the broker maps each
//!    variant to a JSON-RPC error code in `crates/broker/src/router.rs`.
//!
//! # Capability gate
//!
//! The broker checks [`Capability`] strings on the session before dispatching
//! to any handler in this crate (see `require_system` in `router.rs`). Five
//! capability strings are recognised: `camera`, `mic`, `screen`, `bluetooth`,
//! `raw_usb`. Tools without an entry here (process, network, battery,
//! spotlight, metadata, fsevents, USB-list) require no TCC-gated capability,
//! but they still need the session to have explicitly opted in to a
//! `system` capability so an agent registered with no caps cannot probe the
//! host. See `Capability::None`.
//!
//! # Streaming
//!
//! The fsevents tool returns a `watch_id` synchronously and emits subsequent
//! file-system change notifications through the [`NotificationSink`] trait.
//! The broker plugs an adapter that wraps each event into a JSON-RPC
//! `event/notify` per SPEC §10 M10.
//!
//! # Sandbox boundary
//!
//! Even when an agent holds the right capability string, the kernel sandbox
//! profile may reject a system call. `SbplParams` (in the `sandbox` crate)
//! grows one flag per capability; the broker mirrors the session caps into
//! the SBPL when generating per-session profiles.

#![deny(rust_2018_idioms)]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]

pub mod permission;
pub mod types;

#[cfg(target_os = "macos")]
mod cf_owned;

#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod audio_capture;
#[cfg(target_os = "macos")]
pub mod battery;
#[cfg(target_os = "macos")]
pub mod bluetooth;
#[cfg(target_os = "macos")]
pub mod camera;
#[cfg(target_os = "macos")]
pub mod fsevents;
#[cfg(target_os = "macos")]
pub mod screen;
#[cfg(target_os = "macos")]
pub mod usb;

// These three are subprocess-driven and work everywhere mdfind/lsof/ifconfig
// exist. We still cfg-gate by `target_os` for path correctness inside the
// modules themselves.
pub mod metadata;
pub mod network;
pub mod process;
pub mod spotlight;

#[cfg(not(target_os = "macos"))]
mod stub;

pub use types::{Capability, FsEvent, FsEventFlag, SystemError, SystemResult};

/// Sink for streaming events (currently fsevents). The broker provides an
/// implementation that wraps each event into a JSON-RPC `event/notify` and
/// pushes through `SessionEntry::try_push`. Modeled on
/// `vision::subscribe::NotificationSink`.
pub trait NotificationSink: Send + Sync + 'static {
    fn notify(&self, payload: serde_json::Value);
}

/// Globally-unique watch id for fsevents.
pub type WatchId = String;
