//! Per-session registry. **One [`Browser`] per session** (SPEC D2/D3).
//!
//! ## Why [`arc_swap::ArcSwap`] for the live Browser handle
//!
//! Every JSON-RPC call on the connection's hot path resolves
//! `entry.browser` to dispatch a tool call (router.rs:74). The M5 console /
//! exception forwarders, `_internal.status`, and the lifecycle drainer all
//! read it concurrently. The only writer is the SPEC §10 M4 crash-recovery
//! task, which fires at most once per Chromium SIGSEGV/OOM in the 30 s
//! activity window — minutes apart in steady state, never during normal
//! traffic.
//!
//! `ArcSwap` makes that asymmetry exact: `load()` is wait-free RCU
//! (single atomic load, no cmpxchg, no contention against other readers),
//! and `store()` is the rare atomic-swap path. A `Mutex<Arc<Browser>>`
//! would serialize every reader on a lock that the writer touches once an
//! eternity, costing a cmpxchg per call for no benefit.
//! `parking_lot::RwLock<Arc<Browser>>` is closer but still has reader-side
//! ordering work that `ArcSwap`'s strict load doesn't.
//!
//! Readers therefore call `entry.browser.load()` (returns a
//! `Guard<Arc<Browser>>` which derefs to `&Browser`) or `load_full()`
//! (returns an `Arc<Browser>` they can carry across `.await` points). No
//! `unwrap()` on either — both are infallible.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use browser_engine::{Browser, ProxyConfig};
use native_control::AppController;
use observability::metrics::Registry as MetricsRegistry;
use terminal_control::{SessionSandbox, TerminalController};

use system_control::fsevents::WatchHandle as FsWatchHandle;

use crate::events::ClientEvent;
use crate::protocol::ServerEvent;

/// Minimum spacing between "dropping outbound client event" warnings for a
/// single session. Drops are counted unconditionally in the per-session
/// `outbound_drop_count` metric; this only throttles the log line so a
/// saturated queue cannot flood the broker log.
const OUTBOUND_DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSessionState {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub app_blocklist: Vec<String>,
    #[serde(default)]
    pub redact_patterns: Vec<String>,
    #[serde(default = "default_network_outbound")]
    pub network_outbound: bool,
    #[serde(default)]
    pub native_ax_allowed: bool,
    #[serde(default)]
    pub trace: bool,
    #[serde(default)]
    pub trace_hmac_key: Option<String>,
    #[serde(default)]
    pub context_label: Option<String>,
    #[serde(default = "default_persist_context")]
    pub persist_context: bool,
}

fn default_network_outbound() -> bool {
    true
}

fn default_persist_context() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct DurableNetworkObserve {
    pub subscription_id: String,
    pub tab_id: String,
    pub filter: Option<String>,
}

/// Per-session entry. Holds the live Browser (swappable on crash recovery)
/// and the writer channel back to the connected MCP client.
pub struct SessionEntry {
    pub session_id: String,
    pub metrics: MetricsRegistry,
    /// SPEC §10 M4 — atomically replaceable on Chromium crash recovery.
    /// Reader path: `entry.browser.load()` → `Guard<Arc<Browser>>` derefs
    /// to `&Browser`. Writer path: `entry.browser.store(Arc::new(b))`,
    /// only called from `recovery::run`.
    pub browser: ArcSwap<Browser>,
    pub created_at: Instant,
    pub created_at_unix_ms: u64,
    pub last_activity_ms: AtomicU64,
    pub conn_tx: Mutex<Option<mpsc::Sender<ClientEvent>>>,
    /// Last time we warned about outbound client-queue drops. Rate-limits the
    /// warning path so a stuck client cannot spam the logs.
    pub last_outbound_drop_warn_at: Mutex<Option<Instant>>,
    /// SPEC §10 M10 — true when this session was registered with `trace=true`
    /// and a [`observability::trace::TraceWriter`] is attached to its
    /// [`Browser`] via `attach_trace_sink`. Surfaced to the doctor / ofa-trace
    /// tooling so they can locate active sessions without poking at the
    /// (process-private) browser handle.
    pub trace_enabled: AtomicBool,
    /// SPEC §10 M10 — JoinHandles for the per-tab 500 ms DOM-snapshot
    /// drivers. Stored so shutdown can `abort()` them deterministically
    /// (per the §10 quality gate forbidding spawn-without-handle).
    pub trace_drivers: Mutex<Vec<JoinHandle<()>>>,
    /// SPEC §10 M5 / N27 — JoinHandles for the per-page event forwarders
    /// (`forward_console`, `forward_exceptions`, plus the CR-4 RESCOPED
    /// tab/network/dialog/download forwarders). Stored so
    /// `session.unregister` can `abort()` them deterministically rather
    /// than letting them outlive the session.
    pub forwarder_handles: Mutex<Vec<JoinHandle<()>>>,
    /// SPEC §7 / N34 — active tool-created `net.observe` subscriptions that
    /// must be rebound to restored pages after crash recovery.
    pub durable_network_observe: Mutex<Vec<DurableNetworkObserve>>,
    /// Per-session lifecycle FSM handle. Stored on the entry so reconnecting
    /// clients can reuse the same drain/shutdown task instead of spawning a
    /// second lifecycle manager for an already-existing session.
    pub lifecycle: Mutex<Option<crate::lifecycle::SessionLifecycle>>,
    /// SPEC §11 V4 — per-tab vision pipelines. Empty when
    /// `vision = off`. Lazy-built when `vision = on_demand`, eager-built
    /// in `tab.open` when `vision = continuous`.
    pub vision_pipelines: DashMap<String, Arc<vision::VisionPipeline>>,
    /// SPEC §11 V4 — vision config baked into the session at
    /// `browser.context.create` time.
    pub vision_config: parking_lot::RwLock<vision::VisionConfig>,
    /// SPEC §11 V4 — shared vision histograms (capture/diff/ocr/vlm/
    /// pipeline_total/find_text). Surfaced via `_internal.metrics`.
    pub vision_metrics: vision::Histograms,
    /// SPEC §11 V2 — per-session native-control state. Caches the most
    /// recent `AppSnapshot` per bundle id and resolves refs against it.
    pub app_controller: Arc<AppController>,
    /// SPEC §12 U9 — per-session PTY registry and terminal lifecycle owner.
    pub terminal_controller: Arc<TerminalController>,
    /// SPEC §12 U9 + U13 — exact sandbox state reused by PTY children and
    /// crash-recovery relaunches. None when sandboxing is disabled.
    pub session_sandbox: parking_lot::RwLock<Option<SessionSandbox>>,
    /// SPEC §12 U8 — active session-owned FSEvents watches. Dropped on
    /// unregister / lifecycle shutdown so no watch outlives the session.
    pub system_watches: Mutex<Vec<FsWatchHandle>>,
    /// SPEC §12 U3 — proxy config staged via `net.proxy`. Applied on the
    /// next Browser::launch for this session (initial launch if set before
    /// register-time support ever expands, and crash recovery relaunch today).
    pub staged_proxy: parking_lot::RwLock<Option<ProxyConfig>>,
    /// User-facing browser context label last set via `browser.context.create`.
    pub context_label: parking_lot::RwLock<Option<String>>,
    /// Whether session storage should persist on shutdown. Defaults true per
    /// v1 semantics; `browser.context.create {persist:false}` flips it.
    pub persist_context: AtomicBool,
    /// Capabilities granted at `session.register`. SPEC §11 V3 sandbox
    /// default-deny: `app.*` requires `"native"`.
    pub capabilities: parking_lot::RwLock<BTreeSet<String>>,
}

impl SessionEntry {
    pub fn persisted_state_path(session_root: &Path) -> PathBuf {
        session_root.join("session-state.json")
    }

    pub fn load_persisted_state(session_root: &Path) -> Option<PersistedSessionState> {
        let path = Self::persisted_state_path(session_root);
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn store_persisted_state(
        &self,
        session_root: &Path,
        state: &PersistedSessionState,
    ) -> anyhow::Result<()> {
        let path = Self::persisted_state_path(session_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec(state)?)?;
        Ok(())
    }

    pub fn store_staged_proxy(
        &self,
        session_root: &Path,
        proxy: Option<ProxyConfig>,
    ) -> anyhow::Result<()> {
        *self.staged_proxy.write() = proxy.clone();
        let path = Self::staged_proxy_path(session_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match proxy {
            Some(cfg) => fs::write(path, serde_json::to_vec(&cfg)?)?,
            None => {
                if let Err(e) = fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn staged_proxy_path(session_root: &Path) -> PathBuf {
        session_root.join("staged-proxy.json")
    }

    /// Read back the proxy staged by [`Self::store_staged_proxy`]. Returns
    /// `None` when the session has no staged proxy, when the file was removed
    /// by a `net.proxy` clear, or when the payload no longer deserializes —
    /// a session must still rehydrate if its staged proxy is unreadable.
    pub fn load_persisted_proxy(session_root: &Path) -> Option<ProxyConfig> {
        let path = Self::staged_proxy_path(session_root);
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn new(session_id: String, browser: Browser, metrics: MetricsRegistry) -> Self {
        Self {
            session_id,
            metrics,
            browser: ArcSwap::from_pointee(browser),
            created_at: Instant::now(),
            created_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            last_activity_ms: AtomicU64::new(0),
            conn_tx: Mutex::new(None),
            last_outbound_drop_warn_at: Mutex::new(None),
            trace_enabled: AtomicBool::new(false),
            trace_drivers: Mutex::new(Vec::new()),
            forwarder_handles: Mutex::new(Vec::new()),
            durable_network_observe: Mutex::new(Vec::new()),
            lifecycle: Mutex::new(None),
            vision_pipelines: DashMap::new(),
            vision_config: parking_lot::RwLock::new(vision::VisionConfig::default()),
            vision_metrics: vision::Histograms::new(),
            app_controller: Arc::new(AppController::new()),
            terminal_controller: Arc::new(TerminalController::default()),
            session_sandbox: parking_lot::RwLock::new(None),
            system_watches: Mutex::new(Vec::new()),
            staged_proxy: parking_lot::RwLock::new(None),
            context_label: parking_lot::RwLock::new(None),
            persist_context: AtomicBool::new(true),
            capabilities: parking_lot::RwLock::new(BTreeSet::new()),
        }
    }

    /// True if the session was registered with `capabilities: ["native"]`
    /// (or any superset). Required for every `app.*` call.
    pub fn has_native_capability(&self) -> bool {
        self.capabilities.read().contains("native")
    }

    /// True if the session was registered with the named capability.
    /// Used by SPEC §12 U6 focus-stealing gates (`focus_steal`).
    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.read().contains(name)
    }

    pub fn set_capabilities(&self, caps: &[String]) {
        let mut w = self.capabilities.write();
        w.clear();
        for c in caps {
            w.insert(c.clone());
        }
    }

    pub fn touch(&self) {
        let elapsed = self.created_at.elapsed().as_millis() as u64;
        self.last_activity_ms.store(elapsed, Ordering::Relaxed);
    }

    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }

    pub fn last_activity_age_ms(&self) -> u64 {
        self.created_at
            .elapsed()
            .as_millis()
            .saturating_sub(self.last_activity_ms() as u128) as u64
    }

    /// Bind this session to a connected client's writer task. Replaces any
    /// previous binding (e.g. on reconnect).
    pub fn bind_conn(&self, tx: mpsc::Sender<ClientEvent>) {
        *self.conn_tx.lock() = Some(tx);
    }

    pub fn unbind_conn(&self) {
        *self.conn_tx.lock() = None;
    }

    /// Try to push a server event to the bound client. Returns `false` if
    /// no client is bound or the send failed.
    pub fn try_push(&self, event: ServerEvent) -> bool {
        self.try_push_client_event(ClientEvent::Notify(event))
    }

    /// Try to push an arbitrary outbound client event to the bound client.
    /// Used by the V5 binary `vision.frame` fast path in addition to the
    /// classic JSON notify envelope.
    pub fn try_push_client_event(&self, event: ClientEvent) -> bool {
        let Some(tx) = self.conn_tx.lock().clone() else {
            return false;
        };
        match tx.try_send(event) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.metrics
                    .session(&self.session_id)
                    .outbound_drop_count
                    .fetch_add(1, Ordering::Relaxed);
                self.warn_outbound_drop("full");
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.metrics
                    .session(&self.session_id)
                    .outbound_drop_count
                    .fetch_add(1, Ordering::Relaxed);
                self.warn_outbound_drop("closed");
                false
            }
        }
    }

    /// SPEC §10 M10 — register a 500 ms DOM-snapshot driver task. The
    /// returned handle is owned by the entry and aborted on shutdown.
    fn warn_outbound_drop(&self, state: &str) {
        let mut last_warn = self.last_outbound_drop_warn_at.lock();
        let now = Instant::now();
        let should_warn = last_warn
            .as_ref()
            .map(|last| now.duration_since(*last) >= OUTBOUND_DROP_WARN_INTERVAL)
            .unwrap_or(true);
        if should_warn {
            *last_warn = Some(now);
            warn!(
                session_id = %self.session_id,
                queue_state = state,
                "dropping outbound client event"
            );
        }
    }

    /// SPEC §10 M10 — register a 500 ms DOM-snapshot driver task. The
    /// returned handle is owned by the entry and aborted on shutdown.
    pub fn push_trace_driver(&self, h: JoinHandle<()>) {
        self.trace_drivers.lock().push(h);
    }

    /// SPEC §10 M5 / N27 — register a per-page event-forwarder JoinHandle.
    /// Stored so `session.unregister` can abort it before the underlying
    /// page closes. Idempotent against double-push (each forwarder is one
    /// task per (page, event-stream) pair, so callers MUST push exactly
    /// one handle per spawn).
    pub fn push_forwarder(&self, h: JoinHandle<()>) {
        self.forwarder_handles.lock().push(h);
    }

    pub fn upsert_durable_network_observe(&self, observe: DurableNetworkObserve) {
        let mut entries = self.durable_network_observe.lock();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.subscription_id == observe.subscription_id)
        {
            *existing = observe;
        } else {
            entries.push(observe);
        }
    }

    pub fn durable_network_observe(&self) -> Vec<DurableNetworkObserve> {
        self.durable_network_observe.lock().clone()
    }

    pub fn clear_durable_network_observe(&self) {
        self.durable_network_observe.lock().clear();
    }

    /// Abort every registered M5 forwarder. Idempotent — handles that
    /// already finished (because their broadcast sender dropped) are
    /// abort()ed as no-ops. Called from `session.unregister`.
    pub fn abort_forwarders(&self) {
        for h in self.forwarder_handles.lock().drain(..) {
            h.abort();
        }
    }

    /// Abort every registered trace driver. Idempotent.
    pub fn abort_trace_drivers(&self) {
        for h in self.trace_drivers.lock().drain(..) {
            h.abort();
        }
    }

    /// Close every live terminal session owned by this broker session.
    pub async fn shutdown_terminals(&self) {
        self.terminal_controller.shutdown_all().await;
    }

    /// Register a session-owned FSEvents watch handle for later teardown.
    pub fn register_system_watch(&self, handle: FsWatchHandle) {
        self.system_watches.lock().push(handle);
    }

    /// Drop every registered FSEvents watch. Dropping the handle stops the
    /// underlying runloop thread and aborts its forwarder task.
    pub fn shutdown_system_watches(&self) {
        self.system_watches.lock().clear();
    }
}

/// Concurrent registry of session_id → SessionEntry. Cheap to clone the
/// returning `Arc<SessionEntry>` after lookup.
pub struct SessionRegistry {
    by_id: DashMap<String, Arc<SessionEntry>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            by_id: DashMap::new(),
        }
    }

    pub fn insert(&self, entry: Arc<SessionEntry>) {
        self.by_id.insert(entry.session_id.clone(), entry);
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<SessionEntry>> {
        self.by_id.get(session_id).map(|v| Arc::clone(&*v))
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<SessionEntry>> {
        self.by_id.remove(session_id).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (String, Arc<SessionEntry>)> + '_ {
        self.by_id
            .iter()
            .map(|kv| (kv.key().clone(), Arc::clone(kv.value())))
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
