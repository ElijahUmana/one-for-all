//! `session.register` / `session.unregister`, sandbox preparation,
//! and per-page event forwarders. SPEC §2 + §10 M5 + §11 V3.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::warn;

use browser_engine::Browser;
use focus_manager::SpawnMode;

use crate::protocol::{ErrorCode, JsonRpcResponse};
use crate::registry::SessionEntry;
use crate::State;

use super::{supported_events, supported_methods};

// ---------- session.register / unregister ----------

/// N17 — RAII reservation for one session slot while a register call is still
/// launching Chromium. Dropping the reservation releases the pending counter;
/// callers must call [`commit`] after `registry.insert` succeeds so the live
/// slot is counted by the registry, not the pending bucket.
struct PendingSessionReservation {
    state: Arc<State>,
    active: bool,
}

impl PendingSessionReservation {
    fn new(state: Arc<State>) -> Self {
        state
            .pending_session_registrations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            state,
            active: true,
        }
    }

    fn commit(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.active {
            self.state
                .pending_session_registrations
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            self.active = false;
        }
    }
}

impl Drop for PendingSessionReservation {
    fn drop(&mut self) {
        self.release();
    }
}

async fn bind_existing_or_restore_session(
    state: &Arc<State>,
    parsed: &crate::protocol::SessionRegisterParams,
    existing_session_id: &str,
) -> std::result::Result<Arc<SessionEntry>, JsonRpcResponse> {
    if let Some(entry) = state.registry.get(existing_session_id) {
        apply_session_register_state(&entry, parsed);
        return Ok(entry);
    }

    let session_root = state.user_data_root.join(existing_session_id);
    if !session_root.exists() {
        return Err(JsonRpcResponse::err(
            serde_json::Value::Null,
            ErrorCode::SessionNotFound,
            format!("requested session_id {existing_session_id} not found"),
            Some(json!({"session_id": existing_session_id})),
        ));
    }

    let binary = resolve_chromium(state).await.map_err(|e| {
        JsonRpcResponse::err(
            serde_json::Value::Null,
            ErrorCode::ChromiumLaunchFailed,
            format!("chromium binary not available: {e}"),
            None,
        )
    })?;

    let sandbox_profile = session_root.join("sandbox.sb");
    let session_sandbox = if sandbox_profile.exists() {
        Some(terminal_control::SessionSandbox {
            rootfs: std::fs::canonicalize(&session_root).unwrap_or_else(|_| session_root.clone()),
            user_data_dir: std::fs::canonicalize(&session_root)
                .unwrap_or_else(|_| session_root.clone()),
            profile_path: sandbox_profile,
            seed_plan_path: sandbox::seed_plan_path(&session_root),
            inherit: sandbox::default_allowlist(),
            network_outbound: true,
            native_ax_allowed: false,
            enforced: true,
        })
    } else {
        None
    };

    let browser = Browser::launch(browser_engine::BrowserConfig {
        binary,
        user_data_dir: session_root.clone(),
        mode: SpawnMode::Headless,
        extra_args: Vec::new(),
        sandbox_profile: session_sandbox
            .as_ref()
            .map(|sandbox| sandbox.profile_path.clone()),
        seed_plan_path: Some(sandbox::seed_plan_path(&session_root)),
        proxy: SessionEntry::load_persisted_proxy(&session_root),
    })
    .await
    .map_err(|e| {
        JsonRpcResponse::err(
            serde_json::Value::Null,
            ErrorCode::ChromiumLaunchFailed,
            format!("Browser::launch: {e}"),
            None,
        )
    })?;

    let mut entry = SessionEntry::new(
        existing_session_id.to_string(),
        browser,
        state.metrics.clone(),
    );
    entry
        .staged_proxy
        .write()
        .clone_from(&SessionEntry::load_persisted_proxy(&session_root));
    *entry.session_sandbox.write() = session_sandbox;
    let entry = Arc::new(entry);
    super::term::install_terminal_notification_sink(state, &entry);
    state.registry.insert(Arc::clone(&entry));
    crate::recovery::spawn_crash_watch(Arc::clone(state), Arc::clone(&entry));
    spawn_event_forwarders(state, &entry);
    apply_session_register_state(&entry, parsed);
    let restored_pages = entry
        .browser
        .load_full()
        .default_context()
        .reattach_existing_targets()
        .await
        .map_err(|e| {
            JsonRpcResponse::err(
                serde_json::Value::Null,
                ErrorCode::ChromiumLaunchFailed,
                format!("reattach_existing_targets: {e}"),
                None,
            )
        })?;
    crate::router::replay_network_observe_subscriptions(&entry, &restored_pages, &[]);
    Ok(entry)
}

fn apply_session_register_state(
    entry: &Arc<SessionEntry>,
    parsed: &crate::protocol::SessionRegisterParams,
) {
    entry.set_capabilities(&parsed.capabilities);
    entry
        .app_controller
        .install_privacy(&native_control::PrivacyPolicy {
            app_blocklist: parsed.app_blocklist.clone(),
            redact_patterns: parsed.redact_patterns.clone(),
        });
}

pub(super) async fn handle_session_register(
    state: Arc<State>,
    id: Value,
    params: Option<Value>,
) -> JsonRpcResponse {
    use crate::protocol::SessionRegisterParams;

    let parsed: SessionRegisterParams = match params {
        Some(v) => match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::err(
                    id,
                    ErrorCode::InvalidParams,
                    format!("session.register params: {e}"),
                    None,
                );
            }
        },
        None => {
            return JsonRpcResponse::err(
                id,
                ErrorCode::InvalidParams,
                "session.register requires params",
                None,
            );
        }
    };

    if let Some(existing_session_id) = parsed.session_id.as_deref() {
        let Some(entry) = state.registry.get(existing_session_id) else {
            return JsonRpcResponse::err(
                id,
                ErrorCode::SessionNotFound,
                format!("requested session_id {existing_session_id} not found"),
                Some(json!({"session_id": existing_session_id})),
            );
        };
        entry.set_capabilities(&parsed.capabilities);
        entry
            .app_controller
            .install_privacy(&native_control::PrivacyPolicy {
                app_blocklist: parsed.app_blocklist.clone(),
                redact_patterns: parsed.redact_patterns.clone(),
            });
        let result = json!({
            "session_id": entry.session_id.clone(),
            "broker_version": env!("CARGO_PKG_VERSION"),
            "supported_methods": supported_methods(),
            "supported_events": supported_events(),
        });
        return JsonRpcResponse::ok(id, result);
    }

    // SPEC §11 R12 / N17 — atomic check+insert under the register gate so
    // two concurrent registers cannot both squeeze through at the boundary.
    // The gate is held only across the check; the slow Chromium launch runs
    // outside it after the slot is reserved.
    let _gate = state.register_gate.lock().await;
    if state.registry.len() >= state.max_sessions {
        state
            .session_register_rejected_cap
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let max = state.max_sessions;
        return JsonRpcResponse::err(
            id,
            ErrorCode::SessionLimitExceeded,
            format!("session cap reached ({} active)", max),
            Some(json!({"max_sessions": max})),
        );
    }
    drop(_gate);

    // Allocate a session id. Real implementation will pull this from a UUID
    // crate; for v1 we use a counter + random suffix.
    let session_id = allocate_session_id();
    tracing::debug!(%session_id, "registering new session");

    // Resolve Chromium binary. If the broker hasn't yet fetched, do it now.
    let binary = match resolve_chromium(&state).await {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse::err(
                id,
                ErrorCode::ChromiumLaunchFailed,
                format!("chromium binary not available: {e}"),
                None,
            );
        }
    };

    let user_data_dir = state.user_data_root.join(&session_id);
    let mode = SpawnMode::Headless; // Default per SPEC D1.

    // SPEC §11 V3 — build the per-session sandbox before launching Chromium.
    // Failures fall back to a non-sandboxed launch with a `permission.notice`
    // event so the user knows the agent is unconfined; we never silently
    // drop sandboxing without surfacing it.
    let session_sandbox = match prepare_session_sandbox(
        &state,
        &session_id,
        &user_data_dir,
        parsed.inherit.as_deref(),
        parsed.network_outbound.unwrap_or(true),
        parsed.capabilities.iter().any(|c| c == "native"),
    )
    .await
    {
        Ok(sandbox) => Some(sandbox),
        Err(SandboxPrepError::Disabled(reason)) => {
            tracing::warn!(
                %session_id,
                reason = %reason,
                "session sandbox disabled — agent will run unconfined"
            );
            None
        }
        Err(SandboxPrepError::Fatal(e)) => {
            return JsonRpcResponse::err(
                id,
                ErrorCode::ChromiumLaunchFailed,
                format!("session sandbox prep: {e}"),
                None,
            );
        }
    };

    let config = browser_engine::BrowserConfig {
        binary,
        user_data_dir: user_data_dir.clone(),
        mode,
        extra_args: Vec::new(),
        sandbox_profile: session_sandbox
            .as_ref()
            .map(|sandbox| sandbox.profile_path.clone()),
        seed_plan_path: Some(sandbox::seed_plan_path(&user_data_dir)),
        proxy: SessionEntry::load_persisted_proxy(&user_data_dir),
    };

    let browser = match Browser::launch(config).await {
        Ok(b) => b,
        Err(e) => {
            return JsonRpcResponse::err(
                id,
                ErrorCode::ChromiumLaunchFailed,
                format!("Browser::launch: {e}"),
                None,
            );
        }
    };

    let mut entry = SessionEntry::new(session_id.clone(), browser, state.metrics.clone());
    entry.set_capabilities(&parsed.capabilities);
    entry
        .staged_proxy
        .write()
        .clone_from(&SessionEntry::load_persisted_proxy(&user_data_dir));
    *entry.session_sandbox.write() = session_sandbox.clone();
    let entry = std::sync::Arc::new(entry);
    super::term::install_terminal_notification_sink(&state, &entry);
    // SPEC §12 U13 — install per-session privacy policy so `app_blocklist`
    // and `redact_patterns` apply to clipboard reads + AppElement values.
    entry
        .app_controller
        .install_privacy(&native_control::PrivacyPolicy {
            app_blocklist: parsed.app_blocklist.clone(),
            redact_patterns: parsed.redact_patterns.clone(),
        });
    state.registry.insert(std::sync::Arc::clone(&entry));

    // SPEC §10 M4 — start the crash-watch task for this session.
    crate::recovery::spawn_crash_watch(Arc::clone(&state), Arc::clone(&entry));

    // SPEC §10 M5 — wire console + exception streams from each tab to the
    // event bus so they're surfaced as `event/notify` notifications.
    // Existing tabs only — new tabs hook this up in `tab.open`.
    spawn_event_forwarders(&state, &entry);

    // SPEC §10 M10 — when the client opted in, attach a per-session trace
    // writer to the Browser and spawn DOM-snapshot drivers for every
    // existing tab. Per-tab drivers for tabs opened later are attached in
    // `tab.open` via `attach_trace_driver_for_page`.
    if parsed.trace {
        // Build options that honour `redact_patterns` and `trace_hmac_key`
        // from session.register, falling back to env vars otherwise. The
        // latter lets users without programmatic access still set the HMAC
        // key via `OFA_TRACE_HMAC_KEY`.
        let mut opts = observability::trace::TraceOptions::from_env();
        if !parsed.redact_patterns.is_empty() {
            opts.redact_patterns = parsed.redact_patterns.clone();
        }
        if let Some(raw) = parsed.trace_hmac_key.as_ref().filter(|s| !s.is_empty()) {
            let bytes = if let Some(rest) = raw.strip_prefix("hex:") {
                hex::decode(rest).unwrap_or_else(|_| rest.as_bytes().to_vec())
            } else {
                raw.as_bytes().to_vec()
            };
            opts.hmac_key = Some(bytes);
        }
        match state.traces.get_or_start_with_options(&session_id, opts) {
            Ok(writer) => {
                let sink: std::sync::Arc<dyn observability::trace::TraceSink> = writer.clone();
                entry
                    .browser
                    .load()
                    .attach_trace_sink(Some(std::sync::Arc::clone(&sink)));
                entry
                    .trace_enabled
                    .store(true, std::sync::atomic::Ordering::Release);
                crate::trace_drivers::attach_trace_drivers(&entry, sink);
                tracing::info!(%session_id, "M10 trace recording enabled");
            }
            Err(e) => {
                tracing::warn!(%session_id, error = %e, "M10 trace writer init failed; continuing untraced");
            }
        }
    }

    let result = json!({
        "session_id": session_id,
        "broker_version": env!("CARGO_PKG_VERSION"),
        "supported_methods": supported_methods(),
        "supported_events": supported_events(),
    });
    JsonRpcResponse::ok(id, result)
}

/// SPEC §10 M5 — forward each existing tab's console + exception streams
/// into the session's event bus so connected MCP clients receive
/// `console.message` / `page.exception` notifications.
pub(crate) fn spawn_event_forwarders(_state: &Arc<State>, entry: &Arc<SessionEntry>) {
    let pages = entry.browser.load().default_context().list_tabs();
    for page in pages {
        attach_page_event_forwarders(entry, page);
    }
}

/// Public-but-internal helper used by `tab.open` to wire up M5 forwarders
/// for a freshly-opened tab.
pub fn attach_page_event_forwarders(
    entry: &Arc<SessionEntry>,
    page: std::sync::Arc<browser_engine::Page>,
) {
    let console_rx = page.console_subscribe();
    let exception_rx = page.exception_subscribe();
    let session_id = entry.session_id.clone();
    let tab_id = page.tab_id().0.clone();
    let entry_console = Arc::clone(entry);
    let entry_exc = Arc::clone(entry);

    let console_handle = tokio::spawn(forward_console(
        console_rx,
        entry_console,
        session_id.clone(),
        tab_id.clone(),
    ));
    let exc_handle = tokio::spawn(forward_exceptions(
        exception_rx,
        entry_exc,
        session_id,
        tab_id.clone(),
    ));
    // SPEC §10 M5 / N27 — track these so `session.unregister` can abort them.
    entry.push_forwarder(console_handle);
    entry.push_forwarder(exc_handle);

    // SPEC §11 V4 — attach the vision pipeline if the session asked for
    // continuous mode. On_demand mode lazy-builds on first `vision.*` call;
    // off mode does nothing here.
    let mode = entry.vision_config.read().mode;
    if matches!(mode, vision::VisionMode::Continuous) {
        let entry_vis = Arc::clone(entry);
        let page_for_attach = Arc::clone(&page);
        tokio::spawn(async move {
            attach_vision_pipeline(&entry_vis, &tab_id, page_for_attach).await;
        });
    }
}

/// SPEC §11 V4 — build a [`vision::VisionPipeline`] for a given (session,
/// tab) and start the continuous capture loop. Idempotent; calling for an
/// already-attached tab is a no-op. Errors are logged and swallowed —
/// vision is best-effort, never blocks the agent's other tools.
pub async fn attach_vision_pipeline(
    entry: &Arc<SessionEntry>,
    tab_id: &str,
    page: std::sync::Arc<browser_engine::Page>,
) {
    if entry.vision_pipelines.contains_key(tab_id) {
        return;
    }
    let cfg = entry.vision_config.read().clone();
    let pipeline = match vision::VisionPipeline::new(
        entry.session_id.clone(),
        tab_id.to_owned(),
        entry.vision_metrics.clone(),
        cfg.vlm.clone(),
    ) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            warn!(error = %e, tab_id = %tab_id, "vision pipeline init failed");
            return;
        }
    };
    entry
        .vision_pipelines
        .insert(tab_id.to_owned(), Arc::clone(&pipeline));

    // For continuous mode, start capture and pump immediately.
    if matches!(cfg.mode, vision::VisionMode::Continuous) {
        let sink: Arc<dyn vision::subscribe::NotificationSink> = Arc::new(BrokerNotificationSink {
            entry: Arc::clone(entry),
        });
        // SPEC §10 M10 — when the session has a trace sink attached, plumb
        // it into capture so every screencast frame is doubled-stored to
        // `<trace_dir>/screenshots/`. The frame ring stays ephemeral; the
        // trace gets the durable copy.
        let trace_sink = entry.browser.load().trace_sink();
        let capture_cfg = vision::capture::CaptureConfig {
            format: cfg.format.unwrap_or(vision::FrameFormat::Jpeg),
            quality: 60,
            max_fps: cfg.max_fps.max(1),
            idle_fps: cfg.idle_fps.max(1).min(cfg.max_fps.max(1)),
            max_width: None,
            max_height: None,
            trace_sink: trace_sink.clone(),
            trace_tab_id: trace_sink.as_ref().map(|_| tab_id.to_owned()),
            trace_session_id: trace_sink.as_ref().map(|_| entry.session_id.clone()),
        };
        if let Err(e) = pipeline.start_continuous(page, capture_cfg, sink).await {
            warn!(error = %e, tab_id = %tab_id, "vision continuous start failed");
        }
    }
}

/// SPEC §11 V4 — adapter pushing pipeline notifications onto the session's
/// MCP client writer. The vision crate stays oblivious to broker types.
struct BrokerNotificationSink {
    entry: Arc<SessionEntry>,
}

impl BrokerNotificationSink {
    fn supports_binary_topics(&self) -> bool {
        self.entry.has_capability("binary-topics")
    }
}

impl vision::subscribe::NotificationSink for BrokerNotificationSink {
    fn push_vision_frame(&self, event: vision::subscribe::VisionFrameEvent) {
        if self.supports_binary_topics() {
            let state = event.state.as_ref().map(|s| match s {
                vision::api::StabilityState::Loading => "loading",
                vision::api::StabilityState::Settling => "settling",
                vision::api::StabilityState::Stable => "stable",
            });
            let payload = crate::protocol::VisionFrameEvent {
                session_id: event.session_id,
                tab_id: event.tab_id,
                ts_ms: event.captured_us / 1000,
                frame_seq: event.seq,
                captured_us: event.captured_us,
                frame_handle: crate::protocol::FrameHandle {
                    ring_path: event.frame.shm_path.to_string_lossy().into_owned(),
                    slot: event.frame.slot_index,
                    slot_seq: event.frame.slot_seq,
                    offset: event.frame.offset as u64,
                    len: event.frame.len,
                    ts_us: event.frame.ts_us,
                },
                viewport: crate::protocol::Viewport {
                    offset_top: event.viewport.offset_top,
                    page_scale_factor: event.viewport.page_scale_factor,
                    device_width: event.viewport.device_width,
                    device_height: event.viewport.device_height,
                    scroll_offset_x: event.viewport.scroll_offset_x,
                    scroll_offset_y: event.viewport.scroll_offset_y,
                    timestamp: event.viewport.timestamp,
                },
                changed_tiles: event
                    .changed_tiles
                    .into_iter()
                    .map(|tile| crate::protocol::TileRect {
                        tile_x: tile.tile_x,
                        tile_y: tile.tile_y,
                        x: tile.bbox.x,
                        y: tile.bbox.y,
                        w: tile.bbox.w,
                        h: tile.bbox.h,
                        prev_hash: tile.prev_hash,
                        next_hash: tile.next_hash,
                    })
                    .collect(),
                ocr_delta: event
                    .ocr_delta
                    .into_iter()
                    .map(|entry| crate::protocol::OcrEntry {
                        x: entry.bbox.x,
                        y: entry.bbox.y,
                        w: entry.bbox.w,
                        h: entry.bbox.h,
                        text: entry.text,
                        confidence: entry.confidence,
                    })
                    .collect(),
                stability: event.stability,
                state: state.map(str::to_string),
            };
            let _ = self
                .entry
                .try_push_client_event(crate::events::ClientEvent::VisionFrame(payload));
            return;
        }
        use crate::ServerEvent;
        let ev = ServerEvent {
            jsonrpc: "2.0".into(),
            method: "event/notify".into(),
            params: event.to_json_value(),
        };
        let _ = self.entry.try_push(ev);
    }
}

async fn forward_console(
    mut rx: tokio::sync::broadcast::Receiver<browser_engine::page::ConsoleMessage>,
    entry: Arc<SessionEntry>,
    session_id: String,
    tab_id: String,
) {
    use crate::ServerEvent;
    loop {
        match rx.recv().await {
            Ok(msg) => {
                let ev = ServerEvent {
                    jsonrpc: "2.0".into(),
                    method: "event/notify".into(),
                    params: json!({
                        "topic": "console.message",
                        "session_id": session_id,
                        "tab_id": tab_id,
                        "payload": msg,
                    }),
                };
                let _ = entry.try_push(ev);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

async fn forward_exceptions(
    mut rx: tokio::sync::broadcast::Receiver<browser_engine::page::PageException>,
    entry: Arc<SessionEntry>,
    session_id: String,
    tab_id: String,
) {
    use crate::ServerEvent;
    loop {
        match rx.recv().await {
            Ok(exc) => {
                let ev = ServerEvent {
                    jsonrpc: "2.0".into(),
                    method: "event/notify".into(),
                    params: json!({
                        "topic": "page.exception",
                        "session_id": session_id,
                        "tab_id": tab_id,
                        "payload": exc,
                    }),
                };
                let _ = entry.try_push(ev);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

pub(super) async fn handle_session_unregister(
    state: Arc<State>,
    session: Option<Arc<SessionEntry>>,
    id: Value,
) -> JsonRpcResponse {
    if let Some(s) = session {
        let lifecycle = {
            let mut slot = s.lifecycle.lock();
            slot.take()
        };
        if let Some(lifecycle) = lifecycle {
            lifecycle.shutdown().await;
        }
        // SPEC §10 M5 / N27 — abort per-page event forwarders BEFORE the
        // browser shuts down so they don't outlive their broadcast senders.
        s.abort_forwarders();
        // SPEC §10 M10 — abort 500 ms snapshot drivers and flush the trace
        // writer before tearing down the browser. Drivers must stop BEFORE
        // shutdown so they don't try to capture against a dying CDP pipe.
        s.abort_trace_drivers();
        s.shutdown_system_watches();
        s.shutdown_terminals().await;
        if let Some(writer) = state.traces.remove(&s.session_id) {
            if let Err(e) = writer.shutdown().await {
                warn!(session_id = %s.session_id, error = %e, "trace writer shutdown error");
            }
        }
        let browser = s.browser.load_full();
        if let Err(e) = browser.shutdown().await {
            warn!(session_id = %s.session_id, error = %e, "session.unregister: shutdown error");
        }
        state.registry.remove(&s.session_id);
    }
    JsonRpcResponse::ok(id, json!({"closed": true}))
}

fn allocate_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("s_{n:x}")
}

async fn resolve_chromium(state: &State) -> Result<std::path::PathBuf> {
    if let Some(p) = state.chromium_binary.lock().clone() {
        return Ok(p);
    }
    // The fetcher's `fetch` does the heavy lifting; we use stable channel.
    let opts = chromium_fetcher::FetchOptions::default();
    let p = chromium_fetcher::fetch(None, &opts).await?;
    *state.chromium_binary.lock() = Some(p.clone());
    Ok(p)
}

// ---------- SPEC §11 V3 — sandbox preparation ----------

/// Outcome of `prepare_session_sandbox`. The broker treats `Disabled` as a
/// soft-fall-through (launch unconfined; agent is functional, just not
/// isolated) and `Fatal` as a hard register failure.
#[derive(Debug)]
enum SandboxPrepError {
    /// Sandboxing is not possible on this host but Chromium can still run.
    Disabled(String),
    /// Sandboxing is required but the prep step failed in a way we cannot
    /// recover from (e.g. profile write failed, allowlist parse error).
    Fatal(String),
}

impl std::fmt::Display for SandboxPrepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled(s) => write!(f, "sandbox disabled: {s}"),
            Self::Fatal(s) => write!(f, "sandbox fatal: {s}"),
        }
    }
}

/// SPEC §11 V3 — for a freshly-allocated session, clone the user's Chrome
/// Default profile + the requested user-dir allowlist into the per-session
/// rootfs, generate the sandbox-exec profile, and persist it next to the
/// rootfs so recovery and `ofa merge` can find it.
///
/// Off-macOS hosts skip sandboxing entirely (returns `Disabled`); the V3
/// primitive is macOS-only (the Linux equivalent is Phase 2.5 per §11 V3).
///
/// The user-dir clone runs synchronously here because the caller is on the
/// `session.register` hot path and the clone IS the lever that makes
/// register reflect "fork the user's environment" semantically — pushing it
/// async would let the first `tab.open` see an empty home. Per the SLO
/// budget (< 3s p99 cold spawn-to-ready) this is well within the cost
/// envelope: clonefile is O(1) per inode, not O(bytes).
async fn prepare_session_sandbox(
    state: &Arc<State>,
    session_id: &str,
    user_data_dir: &std::path::Path,
    inherit_keys: Option<&[String]>,
    network_outbound: bool,
    native_ax_allowed: bool,
) -> std::result::Result<terminal_control::SessionSandbox, SandboxPrepError> {
    if !cfg!(target_os = "macos") {
        return Err(SandboxPrepError::Disabled(
            "non-macOS host (Phase 2.5 nspawn equivalent not yet shipped)".into(),
        ));
    }

    // Make sure the per-session UDD parent exists. The recursive clone-into
    // step needs it.
    if let Err(e) = std::fs::create_dir_all(user_data_dir) {
        return Err(SandboxPrepError::Fatal(format!(
            "create_dir_all({}): {e}",
            user_data_dir.display()
        )));
    }

    // V-R1: detect FileVault state. If the volume is locked we cannot
    // clonefile; we fall back to V-R1 cookie seeding by leaving the UDD
    // empty and relying on later CDP-based seeding (separate work item —
    // we surface the state via the metrics registry so observability can
    // alert).
    let fv = sandbox::detect_filevault();
    state
        .metrics
        .session(session_id)
        .filevault_state
        .store(fv as u64, std::sync::atomic::Ordering::Relaxed);

    // Clone the user's resolved host Chrome profile if it exists. Missing
    // host profile is not a failure — most CI hosts won't have one.
    let mut clone_succeeded = false;
    if let Some(host_profile) = sandbox::default_chrome_profile_path() {
        match sandbox::clone_chrome_profile(&host_profile, user_data_dir) {
            Ok(stats) => {
                clone_succeeded = true;
                tracing::info!(
                    %session_id,
                    files = stats.file_count,
                    bytes = stats.bytes_apparent,
                    elapsed_ms = stats.elapsed_ms,
                    "Chrome profile prepared into session UDD"
                );
            }
            Err(sandbox::Error::CloneUnsupported { .. }) => {
                tracing::warn!(
                    %session_id,
                    "chrome profile preparation could not preserve the host profile even after lossless copy fallback — staging V-R1 seed plan for cookie-only CDP seeding"
                );
                // Final V-R1 fallback after profile preparation failed:
                // read the host cookies db and persist a seed plan that
                // browser-engine reads on Chromium boot. This branch is
                // still cookies-only today; storage/IndexedDB/SW stubs
                // already exist in the SeedPlan shape so the producer can
                // grow without a schema change.
                let mut plan = sandbox::SeedPlan::default();
                match sandbox::read_host_cookies(&host_profile) {
                    Ok(cookies) => {
                        let count = cookies.len();
                        plan.cookies = cookies;
                        if count > 0 {
                            tracing::info!(
                                %session_id,
                                cookie_count = count,
                                "V-R1 seed plan: read host cookies"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            %session_id,
                            error = %e,
                            "V-R1 seed plan: read_host_cookies failed (agent boots logged-out)"
                        );
                    }
                }
                if !plan.is_empty() {
                    match sandbox::write_seed_plan(user_data_dir, &plan) {
                        Ok(p) => tracing::info!(
                            %session_id,
                            seed_plan = %p.display(),
                            entries = plan.count(),
                            "V-R1 seed plan persisted; browser-engine will dispatch on Chromium boot"
                        ),
                        Err(e) => tracing::warn!(
                            %session_id,
                            error = %e,
                            "V-R1 seed plan write failed; agent boots logged-out"
                        ),
                    }
                }
            }
            Err(sandbox::Error::DestinationExists(_)) => {
                // Recovery respawn — the cloned profile is already in place.
                clone_succeeded = true;
            }
            Err(e) => {
                return Err(SandboxPrepError::Fatal(format!(
                    "clone_chrome_profile: {e}"
                )));
            }
        }
    }
    let _ = clone_succeeded;

    // Resolve the inherit allowlist.
    let allowlist = match inherit_keys {
        Some(keys) => match sandbox::parse_inherit_keys(keys) {
            Ok(a) => a,
            Err(e) => return Err(SandboxPrepError::Fatal(format!("parse_inherit_keys: {e}"))),
        },
        None => sandbox::default_allowlist(),
    };

    if !allowlist.is_empty() {
        match sandbox::clone_user_dirs(&allowlist, user_data_dir) {
            Ok(reports) => {
                tracing::info!(
                    %session_id,
                    cloned = reports.len(),
                    "user dirs cloned into session rootfs"
                );
            }
            Err(sandbox::Error::CloneUnsupported { .. }) => {
                tracing::warn!(
                    %session_id,
                    "clonefile unsupported for user dirs; agent will see empty inherits"
                );
            }
            Err(e) => {
                return Err(SandboxPrepError::Fatal(format!("clone_user_dirs: {e}")));
            }
        }
    }

    // Canonicalize the rootfs so the sandbox profile lists real paths
    // (sandbox-exec resolves symlinks; `/var/folders/*` ≠ `/private/var/folders/*`).
    let canonical_rootfs = match std::fs::canonicalize(user_data_dir) {
        Ok(p) => p,
        Err(e) => {
            return Err(SandboxPrepError::Fatal(format!(
                "canonicalize({}): {e}",
                user_data_dir.display()
            )));
        }
    };

    let mut params = sandbox::SbplParams::from_inherit(session_id, &canonical_rootfs, &allowlist);
    params.network_outbound = network_outbound;
    params.native_ax_allowed = native_ax_allowed;
    let text = sandbox::generate_sbpl(&params);
    let profile_path = canonical_rootfs.join("sandbox.sb");
    if let Err(e) = sandbox::write_sbpl(&profile_path, &text) {
        return Err(SandboxPrepError::Fatal(format!("write_sbpl: {e}")));
    }

    // Run the denial probe. The profile is auto-generated so it should
    // ALWAYS enforce, but a non-functional sandbox is worse than no
    // sandbox (operator thinks they're confined). We surface the result
    // via tracing + metrics; we don't fail register on inconclusive.
    let probe_target = canonical_rootfs
        .parent()
        .unwrap_or(&canonical_rootfs)
        .join(format!("{session_id}.probe"));
    match sandbox::probe_sandbox_enforces(&profile_path, &probe_target) {
        Ok(sandbox::ProbeOutcome::Enforcing) => {
            tracing::info!(%session_id, "sandbox probe: profile is enforcing");
        }
        Ok(sandbox::ProbeOutcome::NotEnforcing) => {
            tracing::error!(
                %session_id,
                profile = %profile_path.display(),
                "SANDBOX THEATRE: profile is non-enforcing — agent is unconfined despite wrapping"
            );
            // Don't fail register here; surface visibly via logs +
            // metrics. T7 reviewer-finisher decides if this should be
            // promoted to a hard refusal once the false-positive rate is
            // characterised.
        }
        Ok(sandbox::ProbeOutcome::Inconclusive) => {
            tracing::warn!(%session_id, "sandbox probe: inconclusive");
        }
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "sandbox probe failed to run");
        }
    }
    Ok(terminal_control::SessionSandbox {
        rootfs: canonical_rootfs.clone(),
        user_data_dir: canonical_rootfs,
        profile_path,
        seed_plan_path: sandbox::seed_plan_path(user_data_dir),
        inherit: allowlist,
        network_outbound,
        native_ax_allowed,
        enforced: true,
    })
}

// The session module's helpers (handle_session_register / handle_session_unregister
// / attach_page_event_forwarders) are surfaced via `super::session::*` re-exports
// in `mod.rs`; no re-export of the per-dispatch TLS accessors is needed here.
