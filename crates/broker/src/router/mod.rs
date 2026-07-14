//! JSON-RPC method router. Dispatches `tab.*`, `page.*`, `net.*`, and
//! `browser.context.*` calls to the per-session [`Browser`].
//!
//! Per SPEC §2 every call after `session.register` carries an implicit
//! session: the connection has been bound to a session_id by the handshake,
//! and the router uses that to look up the right `Browser`.
//!
//! ## Layout (SPEC §11 R13 — single-file 1500-line cap)
//!
//! The dispatcher used to live in a single 2068-line `router.rs`. It is now
//! split by tool family. Wire format (SPEC §2/§7) is byte-exact — only file
//! boundaries moved.
//!
//! - [`mod@browser`]   — `browser.context.*`
//! - [`mod@tab`]       — `tab.*`
//! - [`mod@page`]      — `page.*` + `vision.*` (per-page content tools)
//! - [`mod@net`]       — `net.*`
//! - [`mod@session`]   — `session.register`/`unregister` + sandbox prep + event forwarders
//! - [`mod@internal`]  — `_internal.*` introspection + `app.*` native macOS control

use std::sync::Arc;

use serde_json::{json, Value};
use tracing::warn;

use browser_engine::{Browser, Element, WaitUntil};

use crate::protocol::{ErrorCode, JsonRpcRequest, JsonRpcResponse};
use crate::registry::SessionEntry;
use crate::State;

mod browser;
mod internal;
mod native;
mod net;
mod page;
pub(crate) mod session;
mod system;
mod tab;
mod term;

// ---------- public surface (callers in server.rs and recovery.rs) ----------

pub(crate) use net::replay_network_observe_subscriptions;
pub use session::attach_page_event_forwarders;

/// Resolve a single JSON-RPC request to a response.
pub async fn dispatch(
    state: Arc<State>,
    session: Option<Arc<SessionEntry>>,
    req: JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let id = match req.id.clone() {
        Some(v) => v,
        None => {
            // Notifications get no response per JSON-RPC 2.0.
            return None;
        }
    };

    // session.register and session.unregister are special — they don't need
    // an existing session bound (and register is the one that creates it).
    match req.method.as_str() {
        "session.register" => {
            return Some(session::handle_session_register(state, id, req.params).await);
        }
        "session.unregister" => {
            return Some(session::handle_session_unregister(state, session, id).await);
        }
        // SPEC §7 internal RPCs — broker introspection. No session bind required.
        "_internal.ping" => {
            return Some(JsonRpcResponse::ok(
                id,
                json!({
                    "ok": true,
                    "broker_version": env!("CARGO_PKG_VERSION"),
                    "uptime_ms": state.started_at.elapsed().as_millis() as u64,
                }),
            ));
        }
        "_internal.status" => {
            return Some(internal::handle_internal_status(state, id));
        }
        "_internal.metrics" => {
            return Some(internal::handle_internal_metrics(state, id));
        }
        _ => {}
    }

    // Every other call requires a bound session.
    let Some(session) = session else {
        return Some(JsonRpcResponse::err(
            id,
            ErrorCode::SessionNotFound,
            "no session bound to this connection",
            None,
        ));
    };
    session.touch();
    // SPEC §10 M10 — set thread-locals for the duration of this dispatch so
    // the action-record helper and `browser.context.create` can reach the
    // trace registry without threading an extra arg through every tool fn.
    CURRENT_STATE.with(|c| *c.borrow_mut() = Some(Arc::clone(&state)));
    CURRENT_SESSION.with(|c| *c.borrow_mut() = Some(Arc::clone(&session)));
    let _tls_guard = TlsGuard;
    // SPEC §10 M4 — `load_full()` returns an `Arc<Browser>` we can carry
    // across `.await` points without holding the ArcSwap guard, so the
    // (rare) crash-recovery `store()` is never blocked by an in-flight
    // tool call. Auto-deref turns `&Arc<Browser>` into `&Browser` for the
    // helpers below.
    let browser = session.browser.load_full();
    let params = req.params.unwrap_or(Value::Null);
    // SPEC §10 M10 — keep a copy so the action record after dispatch can
    // reference the original args without contending with the per-handler
    // moves below.
    let trace_args = if session
        .trace_enabled
        .load(std::sync::atomic::Ordering::Acquire)
    {
        Some(params.clone())
    } else {
        None
    };

    let result = match req.method.as_str() {
        "browser.context.create" => {
            browser::browser_context_create(&browser, &session, params).await
        }
        "browser.context.list" => browser::browser_context_list(&session).await,
        "browser.context.destroy" => browser::browser_context_destroy(&browser, params).await,

        "tab.open" => tab::tab_open(&browser, params).await,
        "tab.list" => tab::tab_list(&browser).await,
        "tab.close" => tab::tab_close(&browser, params).await,
        "tab.focus" => tab::tab_focus(&browser, params).await,
        "tab.navigate" => tab::tab_navigate(&browser, params).await,
        "tab.wait" => tab::tab_wait(&browser, params).await,

        "page.snapshot" => page::page_snapshot(&browser, params).await,
        "page.screenshot" => page::page_screenshot(&browser, params).await,
        "page.read_text" => page::page_read_text(&browser, params).await,
        "page.click" => page::page_click(&browser, params).await,
        "page.type" => page::page_type(&browser, params).await,
        "page.keypress" => page::page_keypress(&browser, params).await,
        "page.scroll" => page::page_scroll(&browser, params).await,
        "page.hover" => page::page_hover(&browser, params).await,
        "page.drag" => page::page_drag(&browser, params).await,
        "page.touch.tap" => page::page_touch_tap(&browser, params).await,
        "page.touch.swipe" => page::page_touch_swipe(&browser, params).await,
        "page.touch.pinch" => page::page_touch_pinch(&browser, params).await,
        "page.touch.rotate" => page::page_touch_rotate(&browser, params).await,
        "page.pointer.press" => page::page_pointer_press(&browser, params).await,
        "page.pointer.move" => page::page_pointer_move(&browser, params).await,
        "page.pointer.release" => page::page_pointer_release(&browser, params).await,
        "page.gesture.pinch" => page::page_gesture_pinch(&browser, params).await,
        "page.gesture.rotate" => page::page_gesture_rotate(&browser, params).await,
        "page.gesture.longpress" => page::page_gesture_longpress(&browser, params).await,
        "page.drag.file_drop" => page::page_drag_file_drop(&browser, params).await,
        "page.keyboard.shortcut" => page::page_keyboard_shortcut(&browser, params).await,
        "page.keyboard.ime" => page::page_keyboard_ime(&browser, params).await,
        "page.dead_key" => page::page_dead_key(&browser, params).await,
        "page.scroll.precise" => page::page_scroll_precise(&browser, params).await,
        "page.tab_traversal" => page::page_tab_traversal(&browser, params).await,
        "page.right_click_menu_navigate" => {
            page::page_right_click_menu_navigate(&browser, &session, params).await
        }
        "page.eval" => page::page_eval(&browser, params).await,
        "page.cookies" => page::page_cookies(&browser, params).await,
        "page.cookies.deep_set" => page::page_cookies_deep_set(&browser, params).await,
        "page.storage" => page::page_storage(&browser, params).await,
        "page.localstorage.get" => page::page_localstorage_get(&browser, params).await,
        "page.localstorage.set" => page::page_localstorage_set(&browser, params).await,
        "page.localstorage.delete" => page::page_localstorage_delete(&browser, params).await,
        "page.localstorage.clear" => page::page_localstorage_clear(&browser, params).await,
        "page.localstorage.cas" => page::page_localstorage_cas(&browser, params).await,
        "page.sessionstorage.get" => page::page_sessionstorage_get(&browser, params).await,
        "page.sessionstorage.set" => page::page_sessionstorage_set(&browser, params).await,
        "page.sessionstorage.delete" => page::page_sessionstorage_delete(&browser, params).await,
        "page.sessionstorage.clear" => page::page_sessionstorage_clear(&browser, params).await,
        "page.sessionstorage.cas" => page::page_sessionstorage_cas(&browser, params).await,
        "page.indexeddb.list_databases" => {
            page::page_indexeddb_list_databases(&browser, params).await
        }
        "page.indexeddb.list_stores" => page::page_indexeddb_list_stores(&browser, params).await,
        "page.indexeddb.query" => page::page_indexeddb_query(&browser, params).await,
        "page.indexeddb.put" => page::page_indexeddb_put(&browser, params).await,
        "page.indexeddb.delete" => page::page_indexeddb_delete(&browser, params).await,
        "page.indexeddb.delete_database" => {
            page::page_indexeddb_delete_database(&browser, params).await
        }
        "page.cache_api.list" => page::page_cache_api_list(&browser, params).await,
        "page.cache_api.inspect" => page::page_cache_api_inspect(&browser, params).await,
        "page.cache_api.delete" => page::page_cache_api_delete(&browser, params).await,
        "page.permissions.query" => page::page_permissions_query(&browser, params).await,
        "page.permissions.grant" => page::page_permissions_grant(&browser, params).await,
        "page.permissions.revoke" => page::page_permissions_revoke(&browser, params).await,
        "page.storage.quota" => page::page_storage_quota(&browser, params).await,
        "page.viewport" => page::page_viewport(&browser, params).await,
        "page.user_agent" => page::page_user_agent(&browser, params).await,
        "page.geo" => page::page_geo(&browser, params).await,
        "page.dark_mode" => page::page_dark_mode(&browser, params).await,
        // SPEC §10 M7 + M8.
        "page.network_conditions" => page::page_network_conditions(&browser, params).await,
        "page.emulate" => page::page_emulate(&browser, params).await,

        // SPEC §12 U4 — perf + introspection.
        "page.performance.timeline_start" | "page.performance_timeline_start" => {
            page::page_performance_timeline_start(&browser, params).await
        }
        "page.performance.timeline_stop" | "page.performance_timeline_stop" => {
            page::page_performance_timeline_stop(&session, &browser, params).await
        }
        "page.performance.metrics" | "page.performance_metrics" => {
            page::page_performance_metrics(&browser, params).await
        }
        "page.coverage.js_start" | "page.coverage_js_start" => {
            page::page_coverage_js_start(&browser, params).await
        }
        "page.coverage.js_take" | "page.coverage_js_take" => {
            page::page_coverage_js_take(&browser, params).await
        }
        "page.coverage.css_start" | "page.coverage_css_start" => {
            page::page_coverage_css_start(&browser, params).await
        }
        "page.coverage.css_take" | "page.coverage_css_take" => {
            page::page_coverage_css_take(&browser, params).await
        }
        "page.heap.snapshot" | "page.heap_snapshot" => {
            page::page_heap_snapshot(&session, &browser, params).await
        }
        "page.heap.sample_alloc" | "page.heap_sample_alloc" => {
            page::page_heap_sample_alloc(&browser, params).await
        }
        "page.cpu.profile" | "page.cpu_profile" => page::page_cpu_profile(&browser, params).await,
        "page.layout.metrics" | "page.layout_metrics" => {
            page::page_layout_metrics(&browser, params).await
        }
        "page.paint.flash" | "page.paint_flash" => page::page_paint_flash(&browser, params).await,

        // SPEC §12 U5 — print + PDF.
        "page.pdf" => page::page_pdf(&session, &browser, params).await,
        "page.print_preview" => page::page_print_preview(&browser, params).await,

        "net.intercept"
        | "net.mock"
        | "net.observe"
        | "net.intercept.fulfill_with_body"
        | "net.intercept.modify_request"
        | "net.intercept.fail"
        | "net.replay"
        | "net.websocket.observe"
        | "net.websocket.inject_frame"
        | "net.eventsource.observe"
        | "net.har.export"
        | "net.proxy"
        | "net.mitm_cert.install" => {
            // SPEC §7 + §12 U3 — deep-network surface. Per-page state lives
            // on the Page; this dispatcher just routes typed args.
            net::net_dispatch(&browser, req.method.as_str(), params).await
        }

        // SPEC §11 V4 — vision tool surface.
        "vision.read_text" => page::vision_read_text(&session, params).await,
        "vision.find_text" => page::vision_find_text(&session, params).await,
        "vision.compare" => page::vision_compare(&session, params).await,
        "vision.fps" => page::vision_fps(&session, params).await,
        // SPEC §11 V4 deeper hooks.
        "vision.stability" => page::vision_stability(&session, params).await,
        "vision.changed_since" => page::vision_changed_since(&session, params).await,
        "vision.verify_action" => page::vision_verify_action(&session, params).await,
        // SPEC §12 U10 — universal vision sub-granularity surface.
        "vision.pixel" => page::vision_pixel(&session, params).await,
        "vision.region.classify" => page::vision_region_classify(&session, params).await,
        "vision.color.palette" => page::vision_color_palette(&session, params).await,
        "vision.text.style" => page::vision_text_style(&session, params).await,
        "vision.layout.segments" => page::vision_layout_segments(&session, params).await,
        "vision.icon.recognize" => page::vision_icon_recognize(&session, params).await,
        "vision.qr_barcode" => page::vision_qr_barcode(&session, params).await,
        "vision.scrollbar.position" => page::vision_scrollbar_position(&session, params).await,
        "vision.loading.detect" => page::vision_loading_detect(&session, params).await,
        "vision.tooltip.detect" => page::vision_tooltip_detect(&session, params).await,
        "vision.modal.detect" => page::vision_modal_detect(&session, params).await,
        "vision.diff.semantic" => page::vision_diff_semantic(&session, params).await,
        "vision.animation.frames" => page::vision_animation_frames(&session, params).await,
        "vision.face_blur" => page::vision_face_blur(&session, params).await,

        // SPEC §12 U9 — terminal / PTY surface.
        "term.spawn" => term::term_spawn(&session, params).await,
        "term.write" => term::term_write(&session, params).await,
        "term.read" => term::term_read(&session, params).await,
        "term.snapshot" => term::term_snapshot(&session, params).await,
        "term.resize" => term::term_resize(&session, params).await,
        "term.close" => term::term_close(&session, params).await,
        "term.send_signal" => term::term_send_signal(&session, params).await,
        "term.scrollback" => term::term_scrollback(&session, params).await,
        "term.alt_screen_active" => term::term_alt_screen_active(&session, params).await,
        "term.mouse_event" => term::term_mouse_event(&session, params).await,

        // SPEC §12 U8 — host system-control surface.
        "system.audio.output"
        | "system.audio.input"
        | "system.audio.select"
        | "system.audio.volume"
        | "system.audio.mute"
        | "system.audio.capture_to_file"
        | "system.mic.capture"
        | "system.camera.snapshot"
        | "system.screen.capture_region"
        | "system.screen.list_displays"
        | "system.bluetooth.scan"
        | "system.bluetooth.connect"
        | "system.bluetooth.disconnect"
        | "system.usb.devices"
        | "system.battery"
        | "system.network.interfaces"
        | "system.network.routes"
        | "system.network.connections"
        | "system.process.list"
        | "system.process.info"
        | "system.process.signal"
        | "system.fsevents.watch"
        | "system.spotlight.query"
        | "system.metadata" => system::system_dispatch(&session, req.method.as_str(), params).await,

        // SPEC §11 V2 — native macOS app control via Accessibility API.
        // Capability-gated: every arm requires the session to have been
        // registered with `capabilities: ["native"]`. The gate ALSO
        // checks `AXIsProcessTrusted()` and surfaces the System Settings
        // deeplink in `data.settings_url` when permission is missing.
        "app.list" => internal::app_list(&session).await,
        "app.snapshot" => internal::app_snapshot(&session, params).await,
        "app.click" => internal::app_click(&session, params).await,
        "app.type" => internal::app_type(&session, params).await,
        "app.scroll" => internal::app_scroll(&session, params).await,
        "app.eval" => internal::app_eval(&session, params).await,

        // SPEC §12 U6 — Menu / status menu / dock / window / spotlight /
        // spaces / notifications / quicklook / touchbar / gestures / IME /
        // scripting. Same capability + AX gate as the base V2 set.
        "app.menu.list" => native::app_menu_list(&session, params).await,
        "app.menu.click" => native::app_menu_click(&session, params).await,
        "app.statusmenu.click" => native::app_statusmenu_click(&session, params).await,
        "app.notification_center.open" => native::app_notif_open(&session, params).await,
        "app.notification_center.list" => native::app_notif_list(&session).await,
        "app.notification_center.click" => native::app_notif_click(&session, params).await,
        "app.notification_center.dismiss" => native::app_notif_dismiss(&session, params).await,
        "app.spotlight.open" => native::app_spotlight_open(&session, params).await,
        "app.spotlight.query" => native::app_spotlight_query(&session, params).await,
        "app.spotlight.select" => native::app_spotlight_select(&session, params).await,
        "app.spaces.list" => native::app_spaces_list(&session).await,
        "app.spaces.switch_to" => native::app_spaces_switch(&session, params).await,
        "app.spaces.move_window" => native::app_spaces_move_window(&session, params).await,
        "app.dock.list" => native::app_dock_list(&session).await,
        "app.dock.click" => native::app_dock_click(&session, params).await,
        "app.dock.reveal_app" => native::app_dock_reveal_app(&session, params).await,
        "app.window.list" => native::app_window_list(&session, params).await,
        "app.window.raise" => native::app_window_raise(&session, params).await,
        "app.window.minimize" => native::app_window_minimize(&session, params).await,
        "app.window.fullscreen" => native::app_window_fullscreen(&session, params).await,
        "app.window.move" => native::app_window_move(&session, params).await,
        "app.window.resize" => native::app_window_resize(&session, params).await,
        "app.touchbar.tap" => native::app_touchbar_tap(&session, params).await,
        "app.gesture.three_finger_swipe" => native::app_gesture_swipe(&session, params).await,
        "app.force_touch" => native::app_force_touch(&session, params).await,
        "app.ime.list" => native::app_ime_list(&session).await,
        "app.ime.switch" => native::app_ime_switch(&session, params).await,
        "app.ime.set_input_source" => native::app_ime_set(&session, params).await,
        "app.shortcut.run" => native::app_shortcut_run(&session, params).await,
        "app.automator.run" => native::app_automator_run(&session, params).await,
        "app.applescript" => native::app_applescript(&session, params).await,
        "app.javascript_for_automation" => native::app_jxa(&session, params).await,
        "app.terminal.spawn_session" => native::app_terminal_spawn(&session, params).await,
        "app.quicklook.preview" => native::app_quicklook_preview(&session, params).await,
        "app.quicklook.close" => native::app_quicklook_close(&session).await,

        // SPEC §12 U7 — Clipboard + drag.
        "clipboard.read_string" => native::clipboard_read_string(&session).await,
        "clipboard.write_string" => native::clipboard_write_string(&session, params).await,
        "clipboard.read_files" => native::clipboard_read_files(&session).await,
        "clipboard.write_files" => native::clipboard_write_files(&session, params).await,
        "clipboard.read_image" => native::clipboard_read_image(&session).await,
        "clipboard.write_image" => native::clipboard_write_image(&session, params).await,
        "clipboard.types" => native::clipboard_types(&session).await,
        "clipboard.history" => native::clipboard_history(&session).await,
        "drag.from_finder" => native::drag_from_finder(&session, params).await,
        "drag.between_apps" => native::drag_between_apps(&session, params).await,

        // SPEC §12 — `app.subscribe` AX event stream.
        "app.subscribe" => native::app_subscribe(&state, &session, params).await,
        "app.unsubscribe" => native::app_unsubscribe(&session, params).await,

        _ => Err(RouterError::method_not_found(req.method.clone())),
    };

    Some(match result {
        Ok(v) => {
            state
                .request_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            session.touch();
            // SPEC §10 M10 — record the action AFTER successful dispatch.
            if let Some(args) = trace_args.as_ref() {
                record_action_if_traced(&session, req.method.as_str(), args, &v);
            }
            JsonRpcResponse::ok(id, v)
        }
        Err(e) => {
            state
                .error_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn!(method = %req.method, error = %e.message, "tool call failed");
            JsonRpcResponse::err(id, e.code, e.message, e.data)
        }
    })
}

/// SPEC §10 M10 — emit a `TraceEvent::Action` record for any successful
/// tool dispatch on a traced session. No-op when the session has trace
/// off; cheap atomic-load on the hot path.
fn record_action_if_traced(session: &SessionEntry, method: &str, args: &Value, result: &Value) {
    if !session
        .trace_enabled
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let Some(state) = current_state() else { return };
    let Some(writer) = state.traces.get(&session.session_id) else {
        return;
    };
    let tab_id = args
        .get("tab_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    use observability::trace::{TraceEvent, TraceSink};
    writer.record(TraceEvent::Action {
        ts_ms: writer.now_ms(),
        session_id: session.session_id.clone(),
        tab_id,
        tool: method.to_owned(),
        args: args.clone(),
        result: result.clone(),
    });
}

/// RAII drop guard that clears the per-dispatch thread-locals when the
/// `dispatch` future returns or is cancelled.
struct TlsGuard;

impl Drop for TlsGuard {
    fn drop(&mut self) {
        CURRENT_STATE.with(|c| c.borrow_mut().take());
        CURRENT_SESSION.with(|c| c.borrow_mut().take());
    }
}

thread_local! {
    static CURRENT_STATE: std::cell::RefCell<Option<Arc<State>>> = const { std::cell::RefCell::new(None) };
    static CURRENT_SESSION: std::cell::RefCell<Option<Arc<SessionEntry>>> =
        const { std::cell::RefCell::new(None) };
}

/// Best-effort access to the current `Arc<State>`. Set by `dispatch` for
/// the duration of one tool call so deeper helpers can reach the trace
/// registry without threading an extra parameter through every signature.
pub(super) fn current_state() -> Option<Arc<State>> {
    CURRENT_STATE.with(|c| c.borrow().clone())
}

pub(super) fn current_session() -> Option<Arc<SessionEntry>> {
    CURRENT_SESSION.with(|c| c.borrow().clone())
}

// ---------- shared helpers (used across page/tab/net/internal) ----------

pub(super) type ToolResult = std::result::Result<Value, RouterError>;

#[derive(Debug)]
pub(super) struct RouterError {
    pub(super) code: ErrorCode,
    pub(super) message: String,
    pub(super) data: Option<Value>,
}

impl RouterError {
    pub(super) fn method_not_found(method: String) -> Self {
        Self {
            code: ErrorCode::MethodNotFound,
            message: format!("method not found: {method}"),
            data: None,
        }
    }
    pub(super) fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidParams,
            message: msg.into(),
            data: None,
        }
    }
    pub(super) fn nav(msg: String) -> Self {
        Self {
            code: ErrorCode::NavigationFailed,
            message: msg,
            data: None,
        }
    }
    pub(super) fn timeout(msg: String) -> Self {
        Self {
            code: ErrorCode::Timeout,
            message: msg,
            data: None,
        }
    }
    pub(super) fn tab_not_found() -> Self {
        Self {
            code: ErrorCode::TabNotFound,
            message: "tab not found".into(),
            data: None,
        }
    }
    pub(super) fn not_actionable() -> Self {
        Self {
            code: ErrorCode::ElementNotActionable,
            message: "element not actionable".into(),
            data: None,
        }
    }
    pub(super) fn internal(msg: String) -> Self {
        Self {
            code: ErrorCode::InternalError,
            message: msg,
            data: None,
        }
    }
}

pub(super) fn required_str<'a>(
    params: &'a Value,
    field: &'static str,
) -> Result<&'a str, RouterError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params(format!("missing {field}")))
}

/// Parse the optional `wait_until` JSON-RPC parameter.
///
/// SPEC §7 names exactly four valid forms (`load`, `domcontentloaded`,
/// `networkidle`, omitted/`null`). Anything else — a misspelled
/// `"loaad"`, a non-string JSON value, or a typo from the agent — used
/// to silently map to `WaitUntil::None`, which is indistinguishable
/// from "caller wanted no wait" and stranded the agent on a navigate
/// that returned immediately. Per N20, reject unknown values with
/// `-32602 InvalidParams` so the failure is loud.
///
/// Note: `Some(Value::Null)` is treated like a missing field — JSON
/// libraries vary in whether they elide nulls, and the existing
/// `required_str` helper already accepts both shapes for similar
/// optional-string fields.
pub(super) fn parse_wait_until(v: Option<&Value>) -> Result<WaitUntil, RouterError> {
    match v {
        None | Some(Value::Null) => Ok(WaitUntil::None),
        Some(Value::String(s)) => match s.as_str() {
            "load" => Ok(WaitUntil::Load),
            "domcontentloaded" => Ok(WaitUntil::DomContentLoaded),
            "networkidle" => Ok(WaitUntil::NetworkIdle),
            other => Err(RouterError::invalid_params(format!(
                "invalid wait_until {other:?}; expected one of: \
                 \"load\", \"domcontentloaded\", \"networkidle\""
            ))),
        },
        Some(other) => Err(RouterError::invalid_params(format!(
            "invalid wait_until: expected string, got {}",
            json_type_name(other)
        ))),
    }
}

/// Map a `serde_json::Value` to a short type name for error messages.
/// Avoids dumping the whole value (which can be large or contain
/// user payload) while still giving the agent enough to fix the call.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(super) fn locate_page(
    browser: &Browser,
    params: &Value,
) -> Result<std::sync::Arc<browser_engine::Page>, RouterError> {
    let tab_id = required_str(params, "tab_id")?;
    let ctx = browser.default_context();
    ctx.get(&browser_engine::TabId(tab_id.into()))
        .ok_or_else(RouterError::tab_not_found)
}

pub(super) fn resolve_ref<'a>(
    elements: &'a [Element],
    params: &Value,
) -> Result<&'a Element, RouterError> {
    let r = params
        .get("ref")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("missing ref"))?;
    resolve_ref_str(elements, r)
}

pub(super) fn resolve_ref_str<'a>(
    elements: &'a [Element],
    r: &str,
) -> Result<&'a Element, RouterError> {
    elements
        .iter()
        .find(|e| e.element_ref == r)
        .ok_or_else(|| RouterError {
            code: ErrorCode::ElementStale,
            message: format!("ref {r:?} not found in current snapshot"),
            data: None,
        })
}

pub(super) fn deterministic_seed(snap: &browser_engine::Snapshot) -> u64 {
    // Stable per-snapshot seed — replays produce the same input paths.
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    s ^= snap.snapshot_seq.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    s ^= (snap.url.len() as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    s
}

pub(super) fn supported_methods() -> Vec<String> {
    [
        "session.register",
        "session.unregister",
        "_internal.ping",
        "_internal.status",
        "_internal.metrics",
        "browser.context.create",
        "browser.context.list",
        "browser.context.destroy",
        "tab.open",
        "tab.list",
        "tab.close",
        "tab.focus",
        "tab.navigate",
        "tab.wait",
        "page.snapshot",
        "page.screenshot",
        "page.read_text",
        "page.click",
        "page.type",
        "page.keypress",
        "page.scroll",
        "page.hover",
        "page.drag",
        "page.touch.tap",
        "page.touch.swipe",
        "page.touch.pinch",
        "page.touch.rotate",
        "page.pointer.press",
        "page.pointer.move",
        "page.pointer.release",
        "page.gesture.pinch",
        "page.gesture.rotate",
        "page.gesture.longpress",
        "page.drag.file_drop",
        "page.keyboard.shortcut",
        "page.keyboard.ime",
        "page.dead_key",
        "page.scroll.precise",
        "page.tab_traversal",
        "page.right_click_menu_navigate",
        "page.eval",
        "page.cookies",
        "page.cookies.deep_set",
        "page.storage",
        "page.localstorage.get",
        "page.localstorage.set",
        "page.localstorage.delete",
        "page.localstorage.clear",
        "page.localstorage.cas",
        "page.sessionstorage.get",
        "page.sessionstorage.set",
        "page.sessionstorage.delete",
        "page.sessionstorage.clear",
        "page.sessionstorage.cas",
        "page.indexeddb.list_databases",
        "page.indexeddb.list_stores",
        "page.indexeddb.query",
        "page.indexeddb.put",
        "page.indexeddb.delete",
        "page.indexeddb.delete_database",
        "page.cache_api.list",
        "page.cache_api.inspect",
        "page.cache_api.delete",
        "page.permissions.query",
        "page.permissions.grant",
        "page.permissions.revoke",
        "page.storage.quota",
        "page.viewport",
        "page.user_agent",
        "page.geo",
        "page.dark_mode",
        "page.network_conditions",
        "page.performance.timeline_start",
        "page.performance.timeline_stop",
        "page.performance.metrics",
        "page.coverage.js_start",
        "page.coverage.js_take",
        "page.coverage.css_start",
        "page.coverage.css_take",
        "page.heap.snapshot",
        "page.heap.sample_alloc",
        "page.cpu.profile",
        "page.layout.metrics",
        "page.paint.flash",
        "page.pdf",
        "page.print_preview",
        "net.intercept",
        "net.mock",
        "net.observe",
        // SPEC §12 U3 — browser deep-network surface.
        "net.intercept.fulfill_with_body",
        "net.intercept.modify_request",
        "net.intercept.fail",
        "net.replay",
        "net.websocket.observe",
        "net.websocket.inject_frame",
        "net.eventsource.observe",
        "net.har.export",
        "net.proxy",
        "net.mitm_cert.install",
        // SPEC §11 V4 — vision tool surface.
        "vision.read_text",
        "vision.find_text",
        "vision.compare",
        "vision.fps",
        // SPEC §11 V4 deeper hooks.
        "vision.stability",
        "vision.changed_since",
        "vision.verify_action",
        // SPEC §12 U10 — universal vision sub-granularity surface.
        "vision.pixel",
        "vision.region.classify",
        "vision.color.palette",
        "vision.text.style",
        "vision.layout.segments",
        "vision.icon.recognize",
        "vision.qr_barcode",
        "vision.scrollbar.position",
        "vision.loading.detect",
        "vision.tooltip.detect",
        "vision.modal.detect",
        "vision.diff.semantic",
        "vision.animation.frames",
        "vision.face_blur",
        // SPEC §12 U9 — terminal / PTY surface.
        "term.spawn",
        "term.write",
        "term.read",
        "term.snapshot",
        "term.resize",
        "term.close",
        "term.send_signal",
        "term.scrollback",
        "term.alt_screen_active",
        "term.mouse_event",
        // SPEC §12 U8 — host system-control surface.
        "system.audio.output",
        "system.audio.input",
        "system.audio.select",
        "system.audio.volume",
        "system.audio.mute",
        "system.audio.capture_to_file",
        "system.mic.capture",
        "system.camera.snapshot",
        "system.screen.capture_region",
        "system.screen.list_displays",
        "system.bluetooth.scan",
        "system.bluetooth.connect",
        "system.bluetooth.disconnect",
        "system.usb.devices",
        "system.battery",
        "system.network.interfaces",
        "system.network.routes",
        "system.network.connections",
        "system.process.list",
        "system.process.info",
        "system.process.signal",
        "system.fsevents.watch",
        "system.spotlight.query",
        "system.metadata",
        // SPEC §11 V2 — native macOS app control. Capability-gated.
        "app.list",
        "app.snapshot",
        "app.click",
        "app.type",
        "app.scroll",
        "app.eval",
        // SPEC §12 U6 — deep-input expansion.
        "app.menu.list",
        "app.menu.click",
        "app.statusmenu.click",
        "app.notification_center.open",
        "app.notification_center.list",
        "app.notification_center.click",
        "app.notification_center.dismiss",
        "app.spotlight.open",
        "app.spotlight.query",
        "app.spotlight.select",
        "app.spaces.list",
        "app.spaces.switch_to",
        "app.spaces.move_window",
        "app.dock.list",
        "app.dock.click",
        "app.dock.reveal_app",
        "app.window.list",
        "app.window.raise",
        "app.window.minimize",
        "app.window.fullscreen",
        "app.window.move",
        "app.window.resize",
        "app.touchbar.tap",
        "app.gesture.three_finger_swipe",
        "app.force_touch",
        "app.ime.list",
        "app.ime.switch",
        "app.ime.set_input_source",
        "app.shortcut.run",
        "app.automator.run",
        "app.applescript",
        "app.javascript_for_automation",
        "app.terminal.spawn_session",
        "app.quicklook.preview",
        "app.quicklook.close",
        // SPEC §12 U7 — clipboard + drag.
        "clipboard.read_string",
        "clipboard.write_string",
        "clipboard.read_files",
        "clipboard.write_files",
        "clipboard.read_image",
        "clipboard.write_image",
        "clipboard.types",
        "clipboard.history",
        "drag.from_finder",
        "drag.between_apps",
        // SPEC §12 — AX events.
        "app.subscribe",
        "app.unsubscribe",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub(super) fn supported_events() -> Vec<String> {
    [
        "console.message",
        "page.exception",
        "network.request",
        "network.response",
        "network.websocket",
        "network.eventsource",
        "session.recovered",
        "session.recovery_failed",
        "broker.shutdown",
        // SPEC §11 V4 — continuous vision event stream.
        "vision.frame",
        // SPEC §12 U9 — terminal output / lifecycle stream.
        "term.output",
        "term.exit",
        // SPEC §12 — AX event stream.
        "app.event",
        // SPEC §12 U8 — filesystem event stream.
        "system.fsevents",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod parse_wait_until_tests {
    //! N20 — `parse_wait_until` rejects unknown values with
    //! `-32602 InvalidParams` instead of silently mapping them to
    //! `WaitUntil::None`. The wildcard arm used to swallow typos like
    //! `"loaad"` and non-string JSON values, leaving the agent to
    //! wonder why their navigate returned without waiting.

    use super::*;
    use crate::protocol::ErrorCode;
    use serde_json::json;

    #[test]
    fn missing_field_maps_to_none() {
        let r = parse_wait_until(None).unwrap();
        assert_eq!(r, WaitUntil::None);
    }

    #[test]
    fn explicit_null_maps_to_none() {
        let v = Value::Null;
        let r = parse_wait_until(Some(&v)).unwrap();
        assert_eq!(r, WaitUntil::None);
    }

    #[test]
    fn known_strings_parse_to_their_variants() {
        let load = json!("load");
        let dcl = json!("domcontentloaded");
        let ni = json!("networkidle");
        assert_eq!(parse_wait_until(Some(&load)).unwrap(), WaitUntil::Load);
        assert_eq!(
            parse_wait_until(Some(&dcl)).unwrap(),
            WaitUntil::DomContentLoaded
        );
        assert_eq!(parse_wait_until(Some(&ni)).unwrap(), WaitUntil::NetworkIdle);
    }

    #[test]
    fn unknown_string_returns_invalid_params() {
        let v = json!("loaad"); // typo — used to silently map to None
        let err = parse_wait_until(Some(&v)).unwrap_err();
        assert!(matches!(err.code, ErrorCode::InvalidParams));
        assert!(err.message.contains("loaad"), "got {}", err.message);
        assert!(
            err.message.contains("load")
                && err.message.contains("domcontentloaded")
                && err.message.contains("networkidle"),
            "error message must list valid options; got: {}",
            err.message
        );
    }

    #[test]
    fn empty_string_is_rejected() {
        // `""` is a string, but it isn't one of the four valid forms.
        // Treat it as a typo, not as "no wait".
        let v = json!("");
        let err = parse_wait_until(Some(&v)).unwrap_err();
        assert!(matches!(err.code, ErrorCode::InvalidParams));
    }

    #[test]
    fn non_string_json_is_rejected_by_type() {
        // Each non-string variant produces a distinct error message
        // mentioning the offending JSON type so the agent can fix
        // their call.
        for (val, ty) in [
            (json!(true), "bool"),
            (json!(42), "number"),
            (json!([]), "array"),
            (json!({}), "object"),
        ] {
            let err = parse_wait_until(Some(&val)).unwrap_err();
            assert!(
                matches!(err.code, ErrorCode::InvalidParams),
                "got {:?}",
                err.code
            );
            assert!(
                err.message.contains(ty),
                "error for {val:?} must mention type {ty}; got: {}",
                err.message
            );
        }
    }
}

#[cfg(test)]
mod u2_surface_tests {
    use super::*;

    #[test]
    fn broker_supported_methods_include_landed_u2_subset_but_not_worker_stubs() {
        let methods = supported_methods();
        for method in [
            "page.storage",
            "page.cookies.deep_set",
            "page.localstorage.get",
            "page.localstorage.set",
            "page.localstorage.delete",
            "page.localstorage.clear",
            "page.localstorage.cas",
            "page.sessionstorage.get",
            "page.sessionstorage.set",
            "page.sessionstorage.delete",
            "page.sessionstorage.clear",
            "page.sessionstorage.cas",
            "page.indexeddb.list_databases",
            "page.indexeddb.list_stores",
            "page.indexeddb.query",
            "page.indexeddb.put",
            "page.indexeddb.delete",
            "page.indexeddb.delete_database",
            "page.cache_api.list",
            "page.cache_api.inspect",
            "page.cache_api.delete",
            "page.permissions.query",
            "page.permissions.grant",
            "page.permissions.revoke",
            "page.storage.quota",
        ] {
            assert!(
                methods.iter().any(|m| m == method),
                "missing landed U2 method from broker supported_methods: {method}"
            );
        }

        for absent in [
            "page.workers.list",
            "page.workers.console",
            "page.workers.evaluate",
            "page.service_workers.list",
            "page.service_workers.inspect",
            "page.service_workers.unregister",
            "page.service_workers.update",
            "page.service_workers.trigger_event",
        ] {
            assert!(
                !methods.iter().any(|m| m == absent),
                "worker/service-worker stub leaked into broker supported_methods: {absent}"
            );
        }
    }
}
