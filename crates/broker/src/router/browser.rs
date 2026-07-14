//! `browser.context.*` handlers. SPEC §7.

use std::sync::Arc;

use serde_json::{json, Value};

use browser_engine::Browser;

use crate::protocol::ErrorCode;
use crate::registry::SessionEntry;

use super::session::attach_vision_pipeline;
use super::{current_session, current_state, RouterError, ToolResult};

pub(super) async fn browser_context_create(
    _browser: &Browser,
    session: &SessionEntry,
    params: Value,
) -> ToolResult {
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let persist = params
        .get("persist")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let _stealth = params
        .get("stealth")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let trace_param = params
        .get("trace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // SPEC §11 V4 — opt-in continuous vision pipeline. `vision` is one of
    // "off" | "on_demand" | "continuous". `fps` (1..=60) configures the
    // active-state cap; `idle_fps` (default 5) is used when no action is
    // in flight. We persist this on the SessionEntry so `tab.open` can
    // attach the pipeline as new tabs arrive.
    if let Some(mode_str) = params.get("vision").and_then(Value::as_str) {
        let mode = match mode_str {
            "off" => vision::VisionMode::Off,
            "on_demand" => vision::VisionMode::OnDemand,
            "continuous" => vision::VisionMode::Continuous,
            other => {
                return Err(RouterError {
                    code: ErrorCode::InvalidParams,
                    message: format!("unknown vision mode {other:?}"),
                    data: None,
                });
            }
        };
        let fps = params
            .get("fps")
            .and_then(Value::as_u64)
            .map(|v| v.clamp(1, 60) as u32)
            .unwrap_or(30);
        let idle_fps = params
            .get("idle_fps")
            .and_then(Value::as_u64)
            .map(|v| v.clamp(1, fps as u64) as u32)
            .unwrap_or_else(|| 5.min(fps));
        {
            let mut cfg = session.vision_config.write();
            cfg.mode = mode;
            cfg.max_fps = fps;
            cfg.idle_fps = idle_fps;
            cfg.format = Some(vision::FrameFormat::Jpeg);
        }
        // For continuous mode, eagerly attach pipelines to every existing
        // tab. New tabs are picked up by `tab.open` via
        // `attach_page_event_forwarders`.
        if matches!(mode, vision::VisionMode::Continuous) {
            // dispatch() sets the per-call session TLS, so this is the
            // exact same Arc the router is using for this request.
            if let Some(session_arc) = current_session() {
                let browser = session_arc.browser.load_full();
                let pages = browser.default_context().list_tabs();
                for page in pages {
                    let tab_id = page.tab_id().0.clone();
                    attach_vision_pipeline(&session_arc, &tab_id, page).await;
                }
            }
        }
    }
    // SPEC D2: context_id == session_id in v1. The stealth/trace toggles
    // currently apply session-wide via the default context's stealth/trace
    // flags initialized at session.register; future shared-Chromium mode
    // (D2 footnote) will let these vary per call. Opt-in here is honored
    // additively — calling browser.context.create {trace:true} on a session
    // that registered untraced enables M10 from this point on.
    if trace_param
        && !session
            .trace_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    {
        // Best-effort: try to attach now. Errors are logged but don't fail
        // context creation, since the rest of the v1 surface still works.
        let session_id = session.session_id.clone();
        if let Some(state) = current_state() {
            if let Ok(writer) = state.traces.get_or_start(&session_id) {
                let sink: Arc<dyn observability::trace::TraceSink> = writer.clone();
                session
                    .browser
                    .load()
                    .attach_trace_sink(Some(Arc::clone(&sink)));
                session
                    .trace_enabled
                    .store(true, std::sync::atomic::Ordering::Release);
                // No SessionEntry handle here, so we can't push driver
                // handles. This call site is best-effort — drivers are
                // attached when a future tab.open fires (or already there
                // for tabs opened pre-trace; per-tab drivers can be added
                // later via a follow-up tab.open).
                tracing::info!(%session_id, "M10 trace enabled via browser.context.create");
            }
        }
    }
    if let Some(label) = label.clone() {
        *session.context_label.write() = Some(label);
    }
    session
        .persist_context
        .store(persist, std::sync::atomic::Ordering::Release);
    Ok(json!({
        "context_id": session.session_id.clone(),
        "label": session.context_label.read().clone(),
        "persist": session
            .persist_context
            .load(std::sync::atomic::Ordering::Acquire),
        "trace": session
            .trace_enabled
            .load(std::sync::atomic::Ordering::Acquire),
    }))
}

pub(super) async fn browser_context_list(session: &SessionEntry) -> ToolResult {
    let browser = session.browser.load();
    let ctx = browser.default_context();
    let tabs = ctx.list_tabs();
    Ok(json!({
        "contexts": [{
            "context_id": session.session_id.clone(),
            "label": session.context_label.read().clone(),
            "persist": session
                .persist_context
                .load(std::sync::atomic::Ordering::Acquire),
            "created_at": session.created_at_unix_ms,
            "tab_count": tabs.len(),
        }]
    }))
}

pub(super) async fn browser_context_destroy(_browser: &Browser, params: Value) -> ToolResult {
    let Some(session) = current_session() else {
        return Err(RouterError::internal(
            "missing current session for browser.context.destroy".to_owned(),
        ));
    };
    let context_id = params
        .get("context_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("missing context_id"))?;
    if context_id != session.session_id {
        return Err(RouterError {
            code: ErrorCode::ContextNotFound,
            message: format!("context not found: {context_id}"),
            data: None,
        });
    }
    Err(RouterError {
        code: ErrorCode::PermissionDenied,
        message: "browser.context.destroy is unsupported in v1 because the session owns the Chromium process; use session.unregister or disconnect to close it".to_string(),
        data: Some(json!({"context_id": context_id, "supported": false})),
    })
}
