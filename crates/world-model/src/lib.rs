//! `world-model` — explicit fused world-state publication.
//!
//! This crate is the public load-bearing layer reviewers were missing: the
//! place where the five streams named in the architecture are fused into one
//! atomic snapshot with one generation.
//!
//! Streams:
//! 1. AX structure (`native-control` snapshots / AX events)
//! 2. Capture (`vision` frame handles)
//! 3. Lifecycle (running apps / focus / windows)
//! 4. Window-server / display state (`system-control` display inventory)
//! 5. Cursor / recent input (OS cursor sampling + bounded local ring)
//!
//! The point is not to invent new depth. The point is to make the fusion
//! explicit, typed, and testable in the public repo.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod model;
pub mod store;

pub use model::{
    AppWorld, Coherence, CursorState, DisplayWorld, FocusedWindow, InputEvent, InputEventKind,
    SnapshotSource, WorldSnapshot,
};
pub use store::{WorldModel, WorldModelInput};
