//! [`Page`] — one browser tab. Owns a CDP session, a frame tree, and a
//! lifecycle event channel.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use cdp_client::generated::domains::{fetch as cdp_fetch, page as cdp_page, target as cdp_target};
use cdp_client::generated::CdpEvent;
use cdp_client::{CdpSession, Command, SessionId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tracing::{debug, trace, warn};

use crate::browser::{validate_navigable_url, Browser};
use crate::network::{
    dispatch_fetch_event, EsMessage, FetchDispatch, PageNetworkState, WsFrame, WsFrameKind,
};
use crate::WaitUntil;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TabId(pub String);

impl TabId {
    pub fn from_target(target_id: &str) -> Self {
        Self(format!("t_{}", &target_id[..target_id.len().min(8)]))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub frame_id: String,
    pub name: String,
    pub timestamp: f64,
}

/// SPEC §10 M5 — `Runtime.consoleAPICalled` payload, normalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleMessage {
    pub level: String,
    pub text: String,
    pub source: String,
    pub ts_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// SPEC §10 M5 — `Runtime.exceptionThrown` payload, normalized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageException {
    pub text: String,
    pub stack: String,
    pub ts_ms: f64,
}

/// SPEC §11 V4 — `Page.screencastFrame` event, normalized for vision
/// pipeline consumers. The base64 body sits inside an `Arc<String>` so
/// broadcast subscribers don't clone the (potentially 256 KiB+) string
/// per fan-out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreencastFramePayload {
    /// Base64-encoded image bytes, per CDP `Page.screencastFrame`.
    pub data_b64: Arc<String>,
    /// Frame metadata (`offsetTop`, `scrollOffset`, `pageScaleFactor`, ...).
    pub metadata: serde_json::Value,
    /// CDP per-frame session id, required for `Page.screencastFrameAck`.
    pub cdp_frame_session_id: i64,
}

/// SPEC §10 M2 — anchor that links the per-tab `snapshot_seq` to the
/// in-page MutationObserver log high-water mark.
///
/// Each `Page::snapshot()` call records the seq it issued plus the
/// largest mutation seq drained at the time the snapshot was taken
/// (`0` when the log was empty). The next
/// `Page::snapshot_delta_since(N)` call only returns mutations whose
/// seq is strictly greater than `mutation_high_water`. The anchor is
/// cleared on top-frame `Page.frameNavigated` because Chromium creates
/// a fresh document and the observer JS state (counter + log) resets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeltaAnchor {
    pub snapshot_seq: u64,
    pub mutation_high_water: u64,
}

pub struct Page {
    browser: Browser,
    tab_id: TabId,
    target_id: String,
    /// Typed CDP session for this page's attached target. Cheap to clone
    /// (Arc internally) — the pump task and command-issuing methods both
    /// share the same handle.
    cdp_session: CdpSession,
    state: Mutex<PageState>,
    /// Broadcast of every page lifecycle event.
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
    /// `Page.frameNavigated` URL changes.
    nav_tx: broadcast::Sender<String>,
    /// SPEC §10 M5 — console API events.
    console_tx: broadcast::Sender<ConsoleMessage>,
    /// SPEC §10 M5 — uncaught page exceptions.
    exception_tx: broadcast::Sender<PageException>,
    /// SPEC §11 V4 — `Page.screencastFrame` events, surfaced to the
    /// vision pipeline. Bounded broadcaster (32 slots per D16); slow
    /// consumers see `Lagged`, never block the pump.
    screencast_tx: broadcast::Sender<ScreencastFramePayload>,
    /// In-flight network request ids; used by `wait::network_idle`. Shared
    /// with the event-pump task via Arc so both observe the same set.
    in_flight: Arc<Mutex<HashSet<String>>>,
    /// SPEC §10 M1 — counters drained by `page.snapshot`.
    completed_counter: Arc<std::sync::atomic::AtomicU64>,
    failed_counter: Arc<std::sync::atomic::AtomicU64>,
    /// Last 50 console messages and exceptions — drained on snapshot.
    recent_console: Arc<Mutex<std::collections::VecDeque<ConsoleMessage>>>,
    recent_exceptions: Arc<Mutex<std::collections::VecDeque<PageException>>>,
    snapshot_seq: Mutex<u64>,
    /// SPEC §10 M2 — delta anchor, populated by `snapshot()` /
    /// `snapshot_delta_since()` and cleared on top-frame navigation.
    /// Shared with the event pump (`Arc`) so the pump can clear it on
    /// `Page.frameNavigated` without holding a `&Page`.
    delta_anchor: Arc<Mutex<Option<DeltaAnchor>>>,
    /// SPEC §10 M10 — monotonic id used to correlate `cdp_request`/
    /// `cdp_response` trace records when the underlying typed `Command`
    /// transport hides the wire id from us.
    trace_call_seq: std::sync::atomic::AtomicU64,
    /// SPEC §12 U3 — per-page network state (handler registry, HAR ring
    /// buffer, WS / EventSource / observe broadcasters). Shared with
    /// the event pump via `Arc` so the pump can dispatch
    /// `Fetch.requestPaused` events without re-locking the page.
    network: Arc<PageNetworkState>,
}

#[derive(Debug, Default)]
struct PageState {
    url: String,
    title: String,
}

impl Page {
    /// Wire up a `Page` from an attached CDP session: subscribe to events,
    /// enable the relevant domains, and return.
    pub(crate) async fn bootstrap(
        browser: Browser,
        target_id: String,
        cdp_session_id: SessionId,
    ) -> Result<Self> {
        let tab_id = TabId::from_target(&target_id);
        let cdp_session = browser.cdp().session_for(&cdp_session_id);

        let (lifecycle_tx, _) =
            broadcast::channel::<LifecycleEvent>(crate::PAGE_LIFECYCLE_CAPACITY);
        let (nav_tx, _) =
            broadcast::channel::<String>(observability::caps::PAGE_LIFECYCLE_CAPACITY);
        // SPEC §1 D16 + §10 / N2 — console + exception mailbox capacities live
        // in `observability::caps`; exception bumped from 64 → 512 per N21.
        let (console_tx, _) =
            broadcast::channel::<ConsoleMessage>(observability::caps::CONSOLE_CAP);
        let (exception_tx, _) =
            broadcast::channel::<PageException>(observability::caps::EXCEPTION_CAP);
        // SPEC §11 V4 — bounded screencast broadcaster (D16). Slow
        // consumers see `Lagged`; the pump task never blocks.
        let (screencast_tx, _) = broadcast::channel::<ScreencastFramePayload>(32);
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let completed_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let failed_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let recent_console = Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(50)));
        let recent_exceptions = Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(50)));
        // SPEC §12 U3 — per-page network state. Owned by the page; the
        // pump shares an `Arc` for dispatch.
        let network = PageNetworkState::new();

        let page = Page {
            browser: browser.clone(),
            tab_id,
            target_id,
            cdp_session: cdp_session.clone(),
            state: Mutex::new(PageState::default()),
            lifecycle_tx: lifecycle_tx.clone(),
            nav_tx: nav_tx.clone(),
            console_tx: console_tx.clone(),
            exception_tx: exception_tx.clone(),
            screencast_tx: screencast_tx.clone(),
            in_flight: Arc::clone(&in_flight),
            completed_counter: Arc::clone(&completed_counter),
            failed_counter: Arc::clone(&failed_counter),
            recent_console: Arc::clone(&recent_console),
            recent_exceptions: Arc::clone(&recent_exceptions),
            snapshot_seq: Mutex::new(0),
            delta_anchor: Arc::new(Mutex::new(None)),
            trace_call_seq: std::sync::atomic::AtomicU64::new(0),
            network: Arc::clone(&network),
        };

        // Subscribe to events BEFORE issuing the domain-enable calls so we
        // don't lose any lifecycle events that fire during enable.
        let event_rx = cdp_session.events();

        // Enable the domains we need on this page. Each typed call gives
        // compile-time guarantees on method names (zero-typo by construction).
        use cdp_client::generated::domains::{
            accessibility as cdp_a11y, dom as cdp_dom, network as cdp_network,
            runtime as cdp_runtime,
        };
        page.cdp_send(cdp_page::EnableParams::default())
            .await
            .context("Page.enable")?;
        page.cdp_send(cdp_network::EnableParams::default())
            .await
            .context("Network.enable")?;
        page.cdp_send(cdp_runtime::EnableParams::default())
            .await
            .context("Runtime.enable")?;
        page.cdp_send(cdp_dom::EnableParams::default())
            .await
            .context("DOM.enable")?;
        page.cdp_send(cdp_a11y::EnableParams::default())
            .await
            .context("Accessibility.enable")?;

        // Lifecycle events ride on Page.lifecycleEvent.
        let _ = page
            .cdp_send(cdp_page::SetLifecycleEventsEnabledParams { enabled: true })
            .await;

        if browser
            .proxy_config()
            .as_ref()
            .and_then(|cfg| cfg.auth.as_ref())
            .is_some()
        {
            page.cdp_send(cdp_fetch::EnableParams {
                patterns: Some(Value::Array(Vec::new())),
                handle_auth_requests: Some(true),
            })
            .await
            .context("Fetch.enable (proxy auth)")?;
        }

        // SPEC §10 M2 — install the MutationObserver bootstrap. Idempotent:
        // re-running on the same document is a no-op (the JS guards on
        // `window.__claudeBridgeMutationInstalled`). On every subsequent
        // top-frame navigation Chromium re-runs the registered bootstrap
        // automatically against the new document. Must run AFTER
        // `Runtime.enable` because the helper uses `Runtime.evaluate` to
        // arm the current document.
        ax_engine::install_mutation_observer(&page.cdp_session)
            .await
            .context("install mutation observer (SPEC §10 M2)")?;

        if let Some(seed_script) = browser.session_storage_seed_script() {
            page.cdp_send(cdp_page::AddScriptToEvaluateOnNewDocumentParams {
                source: seed_script.clone(),
                ..Default::default()
            })
            .await
            .context("Page.addScriptToEvaluateOnNewDocument (V-R1 sessionStorage seed)")?;
            let _ = page.eval(&seed_script, false).await;
        }

        // Spawn the event pump task before we return.
        tokio::spawn(event_pump(EventPumpHandles {
            rx: event_rx,
            in_flight,
            lifecycle_tx,
            nav_tx,
            console_tx,
            exception_tx,
            screencast_tx,
            completed_counter,
            failed_counter,
            recent_console,
            recent_exceptions,
            delta_anchor: Arc::clone(&page.delta_anchor),
            browser: browser.clone(),
            cdp_session_id: page.cdp_session.id().as_str().to_owned(),
            target_id: page.target_id.clone(),
            cdp_session: cdp_session.clone(),
            network: Arc::clone(&network),
        }));

        Ok(page)
    }

    pub fn tab_id(&self) -> &TabId {
        &self.tab_id
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn cdp_session_id(&self) -> &SessionId {
        self.cdp_session.id()
    }

    pub(crate) fn cdp_session(&self) -> &CdpSession {
        &self.cdp_session
    }

    /// SPEC §12 U3 — per-page network state used by the U3 surface
    /// (`net.intercept.*`, HAR, WebSocket / EventSource observers).
    pub(crate) fn network_state(&self) -> &Arc<PageNetworkState> {
        &self.network
    }

    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    pub fn url(&self) -> String {
        self.state.lock().url.clone()
    }

    pub fn title(&self) -> String {
        self.state.lock().title.clone()
    }

    pub(crate) fn next_snapshot_seq(&self) -> u64 {
        let mut g = self.snapshot_seq.lock();
        *g += 1;
        *g
    }

    /// Read the current per-tab snapshot seq without bumping it. SPEC §10 M2
    /// uses this in tests; production callers go through `next_snapshot_seq`.
    pub fn current_snapshot_seq(&self) -> u64 {
        *self.snapshot_seq.lock()
    }

    /// SPEC §10 M2 — current delta anchor (the seq + mutation-log
    /// high-water recorded by the most recent snapshot). `None` when no
    /// snapshot has been taken yet, or after a top-frame navigation
    /// cleared it.
    pub(crate) fn delta_anchor(&self) -> Option<DeltaAnchor> {
        *self.delta_anchor.lock()
    }

    /// SPEC §10 M2 — install a fresh anchor. Used by `snapshot()` and
    /// `snapshot_delta_since()` to record the seq they issued and the
    /// largest mutation seq drained at that moment.
    pub(crate) fn set_delta_anchor(&self, anchor: DeltaAnchor) {
        *self.delta_anchor.lock() = Some(anchor);
    }

    /// SPEC §10 M2 — drop the anchor. M4 (browser swap / crash recovery)
    /// calls this when restoring a page so the next snapshot falls back
    /// to a full one rather than chasing a stale in-page seq.
    pub fn clear_delta_anchor(&self) {
        *self.delta_anchor.lock() = None;
    }

    /// Issue a typed CDP command on this page's session. The compiler refuses
    /// any `params` that doesn't implement [`Command`] — method-name typos
    /// are surfaced at compile time, not at runtime.
    pub async fn cdp_send<C>(&self, params: C) -> Result<C::Returns>
    where
        C: Command,
    {
        self.cdp_session
            .send(params)
            .await
            .map_err(anyhow::Error::new)
    }

    /// Untyped CDP escape hatch. Forwards to [`CdpSession::send_raw`] with
    /// `params.unwrap_or(Value::Null)`.
    ///
    /// This shim exists so call sites that haven't yet been migrated to
    /// typed [`Self::cdp_send`] still compile. The wire effect is identical
    /// to the typed path; the only difference is response type erasure
    /// (callers must `serde_json::from_value` themselves).
    ///
    /// SPEC §10 M10 — when the parent [`Browser`] has a trace sink attached,
    /// every call emits a `cdp_request` record before dispatch and a
    /// `cdp_response` record after the reply (or error) lands. The CDP wire
    /// id from `cdp-client` is hidden behind the typed transport so we
    /// generate a per-page monotonic correlation id instead.
    pub async fn cdp_call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let sink = self.browser.trace_sink();
        if let Some(sink) = sink.as_ref() {
            let record_id = self.next_trace_call_id();
            let p = params.unwrap_or(Value::Null);
            sink.record(observability::trace::TraceEvent::CdpRequest {
                ts_ms: sink.now_ms(),
                session_id: self.cdp_session.id().as_str().to_owned(),
                target_id: Some(self.target_id.clone()),
                id: record_id,
                method: method.to_owned(),
                params: p.clone(),
            });
            let outcome = self
                .cdp_session
                .send_raw(method, p)
                .await
                .map_err(anyhow::Error::new);
            match &outcome {
                Ok(v) => sink.record(observability::trace::TraceEvent::CdpResponse {
                    ts_ms: sink.now_ms(),
                    session_id: self.cdp_session.id().as_str().to_owned(),
                    target_id: Some(self.target_id.clone()),
                    id: record_id,
                    result: Some(v.clone()),
                    error: None,
                }),
                Err(e) => sink.record(observability::trace::TraceEvent::CdpResponse {
                    ts_ms: sink.now_ms(),
                    session_id: self.cdp_session.id().as_str().to_owned(),
                    target_id: Some(self.target_id.clone()),
                    id: record_id,
                    result: None,
                    error: Some(serde_json::json!({"message": e.to_string()})),
                }),
            }
            outcome
        } else {
            self.cdp_session
                .send_raw(method, params.unwrap_or(Value::Null))
                .await
                .map_err(anyhow::Error::new)
        }
    }

    /// SPEC §10 M10 — per-page monotonic id used to correlate `cdp_request`
    /// and `cdp_response` trace records when the typed `Command` transport
    /// hides the wire id.
    fn next_trace_call_id(&self) -> i64 {
        use std::sync::atomic::Ordering;
        self.trace_call_seq.fetch_add(1, Ordering::Relaxed) as i64
    }

    /// Implements `tab.navigate` (SPEC §7).
    pub async fn navigate(&self, url: &str, wait_until: WaitUntil) -> Result<()> {
        validate_navigable_url(url)?;
        let _ = self
            .cdp_send(cdp_page::NavigateParams {
                url: url.to_owned(),
                ..Default::default()
            })
            .await
            .context("Page.navigate")?;
        if wait_until != WaitUntil::None {
            self.wait_for_lifecycle(wait_until, Duration::from_secs(30))
                .await?;
        }
        // Refresh state from Target info.
        self.refresh_target_info().await.ok();
        Ok(())
    }

    /// Implements `tab.focus` via `Page.bringToFront` only (SPEC §5 Layer D
    /// forbids `Target.activateTarget`).
    pub async fn bring_to_front(&self) -> Result<()> {
        self.cdp_send(cdp_page::BringToFrontParams::default())
            .await
            .context("Page.bringToFront")?;
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.browser
            .cdp()
            .root_session()
            .send(cdp_target::CloseTargetParams {
                target_id: Value::String(self.target_id.clone()),
            })
            .await
            .context("Target.closeTarget")?;
        Ok(())
    }

    pub async fn refresh_target_info(&self) -> Result<()> {
        let res = self
            .browser
            .cdp()
            .root_session()
            .send_with_retry(cdp_target::GetTargetInfoParams {
                target_id: Some(Value::String(self.target_id.clone())),
            })
            .await
            .context("Target.getTargetInfo")?;
        // `targetInfo` is modelled as `serde_json::Value` (it's a $ref to a
        // protocol type the codegen renders as Value). Parse the two fields
        // we care about.
        if let Some(u) = res.target_info.get("url").and_then(Value::as_str) {
            self.state.lock().url = u.to_owned();
        }
        if let Some(t) = res.target_info.get("title").and_then(Value::as_str) {
            self.state.lock().title = t.to_owned();
        }
        Ok(())
    }

    pub fn lifecycle_subscribe(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle_tx.subscribe()
    }

    pub fn nav_subscribe(&self) -> broadcast::Receiver<String> {
        self.nav_tx.subscribe()
    }

    /// SPEC §10 M5 — subscribe to console messages.
    pub fn console_subscribe(&self) -> broadcast::Receiver<ConsoleMessage> {
        self.console_tx.subscribe()
    }

    /// SPEC §10 M5 — subscribe to uncaught page exceptions.
    pub fn exception_subscribe(&self) -> broadcast::Receiver<PageException> {
        self.exception_tx.subscribe()
    }

    /// SPEC §11 V4 — subscribe to `Page.screencastFrame` events. Bounded
    /// broadcaster (32) so slow consumers see `Lagged` rather than blocking
    /// the pump.
    pub fn screencast_subscribe(&self) -> broadcast::Receiver<ScreencastFramePayload> {
        self.screencast_tx.subscribe()
    }

    /// SPEC §11 V4 — start the CDP screencast. `every_nth` gates effective
    /// FPS (CDP samples its compositor at ~60Hz, so `every_nth = 2` ≈ 30
    /// fps and `every_nth = 12` ≈ 5 fps).
    pub async fn start_screencast(
        &self,
        format: &str,
        quality: u8,
        every_nth: u32,
        max_width: Option<u32>,
        max_height: Option<u32>,
    ) -> Result<()> {
        self.cdp_send(cdp_page::StartScreencastParams {
            format: Some(format.to_owned()),
            quality: Some(quality.into()),
            max_width: max_width.map(|w| w as i64),
            max_height: max_height.map(|h| h as i64),
            every_nth_frame: Some(every_nth.max(1) as i64),
        })
        .await
        .context("Page.startScreencast")?;
        Ok(())
    }

    /// SPEC §11 V4 — stop the CDP screencast.
    pub async fn stop_screencast(&self) -> Result<()> {
        self.cdp_send(cdp_page::StopScreencastParams::default())
            .await
            .context("Page.stopScreencast")?;
        Ok(())
    }

    /// SPEC §11 V4 — ack a screencast frame so the next one flows. CDP
    /// requires this; without it the producer pauses.
    pub async fn screencast_frame_ack(&self, session_id: i64) -> Result<()> {
        self.cdp_send(cdp_page::ScreencastFrameAckParams { session_id })
            .await
            .context("Page.screencastFrameAck")?;
        Ok(())
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.lock().len()
    }

    /// Drain the per-snapshot counters and recent-message buffers. Used by
    /// `page.snapshot` to assemble the M1 augmented result. Returns:
    /// `(console, exceptions, completed_since_last, failed_since_last)`.
    pub(crate) fn drain_snapshot_extras(
        &self,
    ) -> (Vec<ConsoleMessage>, Vec<PageException>, u64, u64) {
        use std::sync::atomic::Ordering;
        let console: Vec<ConsoleMessage> = self.recent_console.lock().drain(..).collect();
        let exceptions: Vec<PageException> = self.recent_exceptions.lock().drain(..).collect();
        let completed = self.completed_counter.swap(0, Ordering::AcqRel);
        let failed = self.failed_counter.swap(0, Ordering::AcqRel);
        (console, exceptions, completed, failed)
    }
}

/// Background event pump. Demuxes typed CDP events into the page's broadcast
/// channels and updates the in-flight network counter.
struct EventPumpHandles {
    rx: broadcast::Receiver<CdpEvent>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
    nav_tx: broadcast::Sender<String>,
    console_tx: broadcast::Sender<ConsoleMessage>,
    exception_tx: broadcast::Sender<PageException>,
    /// SPEC §11 V4 — `Page.screencastFrame` events.
    screencast_tx: broadcast::Sender<ScreencastFramePayload>,
    completed_counter: Arc<std::sync::atomic::AtomicU64>,
    failed_counter: Arc<std::sync::atomic::AtomicU64>,
    recent_console: Arc<Mutex<std::collections::VecDeque<ConsoleMessage>>>,
    recent_exceptions: Arc<Mutex<std::collections::VecDeque<PageException>>>,
    /// SPEC §10 M2 — cleared by the pump on top-frame `Page.frameNavigated`.
    /// The MutationObserver bootstrap re-arms automatically on the new
    /// document, but the in-page seq counter resets to zero, so any anchor
    /// the engine was holding is now meaningless.
    delta_anchor: Arc<Mutex<Option<DeltaAnchor>>>,
    /// SPEC §10 M10 — owning Browser handle so the pump can read the
    /// current trace sink (Some when traced, None otherwise) on each event.
    /// Holding the Browser keeps the sink reachable for the pump's lifetime
    /// without snapshotting (so attaching a sink mid-session takes effect
    /// immediately on the next event).
    browser: Browser,
    /// SPEC §10 M10 — CDP session id of the page this pump serves.
    cdp_session_id: String,
    /// SPEC §10 M10 — `targetId` of the page this pump serves.
    target_id: String,
    /// SPEC §12 U3 + N22 — issuing CDP session for `Fetch.continueRequest` /
    /// `Fetch.fulfillRequest` / `Fetch.failRequest` dispatch in response
    /// to `Fetch.requestPaused`. Cheap to clone.
    cdp_session: CdpSession,
    /// SPEC §12 U3 + N22 — per-page network state for handler lookup,
    /// HAR ring, WS / EventSource / observe broadcasters.
    network: Arc<PageNetworkState>,
}

async fn event_pump(mut h: EventPumpHandles) {
    use std::sync::atomic::Ordering;
    use tokio::sync::broadcast::error::RecvError;

    loop {
        let ev = match h.rx.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(n)) => {
                debug!(skipped = n, "event pump lagged; continuing");
                continue;
            }
        };

        // SPEC §10 M10 — record EVERY inbound CDP event when a trace
        // sink is attached. We do this before the match so even events
        // we don't otherwise route (e.g. domain-specific events the page
        // doesn't subscribe to) are persisted in the audit trail.
        if let Some(sink) = h.browser.trace_sink() {
            let method = ev.method().to_owned();
            let params = serde_json::to_value(&ev).unwrap_or(Value::Null);
            sink.record(observability::trace::TraceEvent::CdpEvent {
                ts_ms: sink.now_ms(),
                session_id: h.cdp_session_id.clone(),
                target_id: Some(h.target_id.clone()),
                method,
                params,
            });
        }

        match ev {
            CdpEvent::PageLifecycleEvent(e) => {
                let frame_id = e.frame_id.as_str().map(str::to_owned).unwrap_or_default();
                let timestamp = e.timestamp.as_f64().unwrap_or(0.0);
                let _ = h.lifecycle_tx.send(LifecycleEvent {
                    frame_id,
                    name: e.name,
                    timestamp,
                });
            }
            CdpEvent::PageFrameNavigated(e) => {
                // SPEC §10 M2 — clear the delta anchor on every
                // `Page.frameNavigated`. Strictly speaking only top-frame
                // navigations reset the in-page MutationObserver state, but
                // detecting "top frame" reliably across CDP versions is
                // brittle and the only cost of over-clearing is a single
                // full-snapshot fallback. Conservative is correct here.
                *h.delta_anchor.lock() = None;
                if let Some(url) = e.frame.get("url").and_then(Value::as_str) {
                    let _ = h.nav_tx.send(url.to_owned());
                }
            }
            CdpEvent::NetworkRequestWillBeSent(e) => {
                if let Some(rid) = e.request_id.as_str() {
                    h.in_flight.lock().insert(rid.to_owned());
                }
                // SPEC §12 U3 — feed HAR ring + net.observe broadcaster.
                h.network.record_request(&e);
            }
            CdpEvent::NetworkResponseReceived(e) => {
                // SPEC §12 U3 — finalise the response side of the HAR
                // record + emit a synthetic observe record.
                h.network.record_response(&e);
            }
            CdpEvent::NetworkLoadingFinished(e) => {
                if let Some(rid) = e.request_id.as_str() {
                    h.in_flight.lock().remove(rid);
                    h.completed_counter.fetch_add(1, Ordering::AcqRel);
                }
                h.network.record_finished(&e);
            }
            CdpEvent::NetworkLoadingFailed(e) => {
                if let Some(rid) = e.request_id.as_str() {
                    h.in_flight.lock().remove(rid);
                    h.failed_counter.fetch_add(1, Ordering::AcqRel);
                }
                h.network.record_failed(&e);
            }
            // SPEC §12 U3 + N22 — Fetch.requestPaused dispatch. Without
            // this arm, every matching request hangs forever (#59).
            CdpEvent::FetchRequestPaused(e) => {
                // CR-4: only synthesize the request event when Fetch
                // has no underlying Network.requestId to pair with. When
                // `network_id` is present, the regular
                // `Network.requestWillBeSent` path will emit the request.
                if e.network_id.is_none() {
                    h.network.record_synthetic_request(&e);
                }
                let dispatch = dispatch_fetch_event(&h.network, &e);
                if let Err(err) = run_fetch_dispatch(&h.cdp_session, dispatch).await {
                    // Never silently swallow — but never break the page
                    // either. Surface at warn! and let the request hang
                    // only if Chromium itself rejected our recovery
                    // attempt; the dispatch fn already preferred a
                    // Continue when we had no plan.
                    warn!(error = %err, "Fetch.requestPaused dispatch failed");
                }
            }
            CdpEvent::FetchAuthRequired(e) => {
                let response = if let Some(auth) = h
                    .browser
                    .proxy_config()
                    .as_ref()
                    .and_then(|cfg| cfg.auth.as_ref())
                {
                    if e.auth_challenge.get("source").and_then(Value::as_str) == Some("Proxy") {
                        json!({
                            "response": "ProvideCredentials",
                            "username": auth.user,
                            "password": auth.pass,
                        })
                    } else {
                        json!({"response": "Default"})
                    }
                } else {
                    json!({"response": "Default"})
                };
                if let Err(err) = h
                    .cdp_session
                    .send(cdp_fetch::ContinueWithAuthParams {
                        request_id: e.request_id,
                        auth_challenge_response: response,
                    })
                    .await
                    .map_err(anyhow::Error::new)
                    .context("Fetch.continueWithAuth")
                {
                    warn!(error = %err, "Fetch.authRequired dispatch failed");
                }
            }
            // SPEC §12 U3 — net.websocket.observe.
            CdpEvent::NetworkWebSocketCreated(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::Created,
                    ts_ms: 0.0,
                    payload_base64: None,
                    url: Some(e.url),
                    error: None,
                });
            }
            CdpEvent::NetworkWebSocketWillSendHandshakeRequest(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::HandshakeRequest,
                    ts_ms,
                    payload_base64: None,
                    url: None,
                    error: None,
                });
            }
            CdpEvent::NetworkWebSocketHandshakeResponseReceived(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::HandshakeResponse,
                    ts_ms,
                    payload_base64: None,
                    url: None,
                    error: None,
                });
            }
            CdpEvent::NetworkWebSocketFrameSent(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                let payload = ws_payload_b64(&e.response);
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::FrameSent,
                    ts_ms,
                    payload_base64: payload,
                    url: None,
                    error: None,
                });
            }
            CdpEvent::NetworkWebSocketFrameReceived(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                let payload = ws_payload_b64(&e.response);
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::FrameReceived,
                    ts_ms,
                    payload_base64: payload,
                    url: None,
                    error: None,
                });
            }
            CdpEvent::NetworkWebSocketFrameError(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::FrameError,
                    ts_ms,
                    payload_base64: None,
                    url: None,
                    error: Some(e.error_message),
                });
            }
            CdpEvent::NetworkWebSocketClosed(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                h.network.record_ws(WsFrame {
                    request_id,
                    kind: WsFrameKind::Closed,
                    ts_ms,
                    payload_base64: None,
                    url: None,
                    error: None,
                });
            }
            // SPEC §12 U3 — net.eventsource.observe.
            CdpEvent::NetworkEventSourceMessageReceived(e) => {
                let request_id = e.request_id.as_str().map(str::to_owned).unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0) * 1000.0;
                h.network.record_es(EsMessage {
                    request_id,
                    event_name: e.event_name,
                    event_id: e.event_id,
                    data: e.data,
                    ts_ms,
                });
            }
            CdpEvent::RuntimeConsoleApiCalled(e) => {
                let level = e.r#type;
                let text = e
                    .args
                    .as_array()
                    .map(|args| {
                        args.iter()
                            .map(|a| {
                                a.get("value")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| a.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let source = e
                    .stack_trace
                    .as_ref()
                    .and_then(|st| st.get("callFrames"))
                    .and_then(|cf| cf.get(0))
                    .and_then(|f| f.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0);
                let stack = e.stack_trace.as_ref().map(|s| s.to_string());
                let msg = ConsoleMessage {
                    level,
                    text,
                    source,
                    ts_ms,
                    stack,
                };
                {
                    let mut q = h.recent_console.lock();
                    if q.len() == 50 {
                        q.pop_front();
                    }
                    q.push_back(msg.clone());
                }
                let _ = h.console_tx.send(msg);
            }
            CdpEvent::RuntimeExceptionThrown(e) => {
                let text = e
                    .exception_details
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let stack = e
                    .exception_details
                    .get("stackTrace")
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let ts_ms = e.timestamp.as_f64().unwrap_or(0.0);
                let exc = PageException { text, stack, ts_ms };
                {
                    let mut q = h.recent_exceptions.lock();
                    if q.len() == 50 {
                        q.pop_front();
                    }
                    q.push_back(exc.clone());
                }
                let _ = h.exception_tx.send(exc);
            }
            CdpEvent::PageScreencastFrame(e) => {
                // SPEC §11 V4 — broadcast to vision pipeline. The
                // base64 body is wrapped in `Arc<String>` so per-subscriber
                // fan-out doesn't clone the payload.
                let payload = ScreencastFramePayload {
                    data_b64: Arc::new(e.data),
                    metadata: e.metadata,
                    cdp_frame_session_id: e.session_id,
                };
                let _ = h.screencast_tx.send(payload);
            }
            other => {
                trace!(method = other.method(), "ignored cdp event");
            }
        }
    }
    debug!("page event pump exiting");
}

/// SPEC §12 U3 + N22 — issue the dispatch chosen by
/// [`crate::network::dispatch_fetch_event`]. Lives at module scope so
/// the typed CDP calls can be shared with future call sites.
async fn run_fetch_dispatch(session: &CdpSession, dispatch: FetchDispatch) -> Result<()> {
    use cdp_client::generated::domains::fetch as cdp_fetch;
    match dispatch {
        FetchDispatch::Continue {
            request_id,
            url,
            method,
            post_data_b64,
            headers,
        } => {
            session
                .send(cdp_fetch::ContinueRequestParams {
                    request_id,
                    url,
                    method,
                    post_data: post_data_b64,
                    headers,
                    intercept_response: None,
                })
                .await
                .map_err(anyhow::Error::new)
                .context("Fetch.continueRequest")?;
        }
        FetchDispatch::Fulfill {
            request_id,
            status,
            headers,
            body_b64,
        } => {
            session
                .send(cdp_fetch::FulfillRequestParams {
                    request_id,
                    response_code: status as i64,
                    response_headers: headers,
                    binary_response_headers: None,
                    body: if body_b64.is_empty() {
                        None
                    } else {
                        Some(body_b64)
                    },
                    response_phrase: None,
                })
                .await
                .map_err(anyhow::Error::new)
                .context("Fetch.fulfillRequest")?;
        }
        FetchDispatch::Fail {
            request_id,
            error_reason,
        } => {
            session
                .send(cdp_fetch::FailRequestParams {
                    request_id,
                    error_reason: Value::String(error_reason),
                })
                .await
                .map_err(anyhow::Error::new)
                .context("Fetch.failRequest")?;
        }
    }
    Ok(())
}

/// CDP `Network.WebSocketFrame` shape: `{opcode, mask, payloadData}`.
/// Body is utf-8 string (text frames) or already-base64 (binary). We
/// pass it through as base64 either way: text frames get re-encoded so
/// callers see one shape regardless of opcode.
fn ws_payload_b64(response: &Value) -> Option<String> {
    use base64::Engine as _;
    let payload = response.get("payloadData").and_then(Value::as_str)?;
    let opcode = response.get("opcode").and_then(Value::as_i64).unwrap_or(1);
    // Opcode 2 = binary (already base64); opcodes 1/9/10 carry text or
    // ping/pong utf-8 — base64-encode for uniformity.
    if opcode == 2 {
        Some(payload.to_owned())
    } else {
        Some(base64::engine::general_purpose::STANDARD.encode(payload.as_bytes()))
    }
}
