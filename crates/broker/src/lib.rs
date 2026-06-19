//! # broker
//!
//! Long-running daemon multiplexing N session clients onto Chromium per the
//! one-for-all spec.
//!
//! ## Process role (SPEC D7)
//!
//! Opportunistic singleton: first client to acquire `flock` on
//! `~/.one-for-all/broker.lock` runs as broker; subsequent processes
//! detect the lock and connect as clients to `~/.one-for-all/broker.sock`. The
//! `mcp-server` binary spawns the broker out of band when it sees the lock
//! is free; the broker also runs as a launchd-managed daemon for graceful
//! lifecycle.
//!
//! ## Threading (SPEC D16)
//!
//! Tokio multi-thread runtime. One actor per: socket accept loop, per-conn
//! task, per-session lifecycle task, focus-guardian (in browser-engine).
//! All sub-tasks are spawned on the broker's runtime; mpsc channels are
//! bounded per the spec table.
//!
//! ## Why every module is `pub`
//!
//! The integration tests in `tests/` need to drive the registry and router
//! end-to-end without the binary's main loop. Public modules let tests
//! construct a broker `State` and exercise it directly.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod events;
pub mod lifecycle;
pub mod protocol;
pub mod recovery;
pub mod registry;
pub mod router;
pub mod server;
pub mod trace_drivers;

pub use lifecycle::{IdleConfig, SessionLifecycle};
pub use protocol::{ErrorCode, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ServerEvent};
pub use registry::{SessionEntry, SessionRegistry};

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;

use observability::metrics::Registry as MetricsRegistry;
use observability::trace::TraceRegistry;
use tokio::task::JoinHandle;

/// SPEC §11 R12 default — soft cap of concurrent registered sessions per
/// broker. Agents may set their own via `--max-sessions`.
pub const DEFAULT_MAX_SESSIONS: usize = 16;

/// Global broker state shared across the accept loop, per-conn tasks, and
/// per-session tasks.
pub struct State {
    pub registry: Arc<SessionRegistry>,
    pub events: Arc<events::EventBus>,
    pub idle: IdleConfig,
    /// Path to the Chromium binary, resolved at startup. `None` until
    /// `chromium-fetcher` succeeds.
    pub chromium_binary: parking_lot::Mutex<Option<std::path::PathBuf>>,
    pub user_data_root: std::path::PathBuf,
    /// Wall-clock at process start; powers `_internal.ping` / `_internal.status`.
    pub started_at: Instant,
    /// Metrics registry shared with browser-engine + observability tooling.
    pub metrics: MetricsRegistry,
    /// SPEC §10 M10 — per-session trace writer registry.
    pub traces: TraceRegistry,
    /// Total successful JSON-RPC calls; surfaced via `_internal.metrics`.
    pub request_counter: std::sync::atomic::AtomicU64,
    pub error_counter: std::sync::atomic::AtomicU64,
    /// SPEC §10 M4 / N17 — hard ceiling on concurrent registered sessions.
    /// `session.register` returns `-32012 SessionLimitExceeded` once the
    /// registry is full. Set via `--max-sessions` at startup; defaults to
    /// [`DEFAULT_MAX_SESSIONS`].
    pub max_sessions: usize,
    /// N17 — count of `session.register` requests rejected because the cap
    /// was already reached.
    pub session_register_rejected_cap: std::sync::atomic::AtomicU64,
    /// N17 — registrations that have reserved a session slot but have not yet
    /// finished Browser launch + registry insert. Combined with `registry.len()`
    /// under `register_gate` so concurrent registers cannot oversubscribe the
    /// cap while a launch is still in flight.
    pub pending_session_registrations: AtomicUsize,
    /// CR-1 — JoinHandles for every spawned `recovery::run` watcher. The
    /// shutdown drain in `main.rs` awaits each with a 2 s timeout so a
    /// respawn-in-flight cannot race teardown.
    pub recovery_handles: parking_lot::Mutex<Vec<JoinHandle<()>>>,
    /// N17 — serializes the cap check + insert in `handle_session_register`
    /// so two concurrent registers cannot both squeeze through at the
    /// boundary.
    pub register_gate: tokio::sync::Mutex<()>,
}

impl State {
    pub fn new(idle: IdleConfig, user_data_root: std::path::PathBuf) -> Arc<Self> {
        Self::new_with_caps(idle, user_data_root, DEFAULT_MAX_SESSIONS)
    }

    pub fn new_with_caps(
        idle: IdleConfig,
        user_data_root: std::path::PathBuf,
        max_sessions: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(SessionRegistry::new()),
            events: events::EventBus::new(),
            idle,
            chromium_binary: parking_lot::Mutex::new(None),
            user_data_root,
            started_at: Instant::now(),
            metrics: MetricsRegistry::new(),
            traces: TraceRegistry::new(),
            request_counter: std::sync::atomic::AtomicU64::new(0),
            error_counter: std::sync::atomic::AtomicU64::new(0),
            max_sessions: max_sessions.max(1),
            session_register_rejected_cap: std::sync::atomic::AtomicU64::new(0),
            pending_session_registrations: AtomicUsize::new(0),
            recovery_handles: parking_lot::Mutex::new(Vec::new()),
            register_gate: tokio::sync::Mutex::new(()),
        })
    }
}
