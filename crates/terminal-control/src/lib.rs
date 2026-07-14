//! `terminal-control` — SPEC §12 U9 PTY-backed terminal surface.
//!
//! Each broker session owns one [`TerminalController`], which manages a set of
//! PTY-backed terminal sessions. The controller keeps raw PTY output for
//! `term.read`, parser-maintained screen state for `term.snapshot`, bounded
//! scrollback for `term.scrollback`, and emits optional notifications for
//! terminal output / exit events.
//!
//! The parser backend is [`vt100`], which is itself driven by a `vte` parser;
//! this satisfies the SPEC requirement that snapshot state be parser-maintained
//! per session while keeping the JSON-facing state deterministic and explicit.

#![deny(rust_2018_idioms)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod controller;
mod state;
pub mod types;

pub use controller::TerminalController;
pub use types::{
    MouseButton, MouseEventKind, MouseEventRequest, SessionSandbox, SpawnTerminalRequest,
    SpawnTerminalResult, TermAltScreenState, TermCursor, TermError, TermExitEvent, TermExitState,
    TermMouseEncoding, TermMouseMode, TermOutputEvent, TermScrollbackLine, TermSessionId,
    TermSnapshot, TermSnapshotRow, TerminalEvent,
};

pub trait NotificationSink: Send + Sync + 'static {
    fn notify(&self, event: TerminalEvent);
}
