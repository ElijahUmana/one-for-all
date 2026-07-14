//! `tab.*` handlers. SPEC §7.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use browser_engine::{validate_navigable_url, Browser, WaitUntil};

use super::{
    current_session, current_state, parse_wait_until, required_str, RouterError, ToolResult,
};

pub(super) async fn tab_open(browser: &Browser, params: Value) -> ToolResult {
    let url = required_str(&params, "url")?;
    validate_navigable_url(url).map_err(|e| RouterError::nav(format!("tab.open: {e}")))?;
    let wait_until = parse_wait_until(params.get("wait_until"))?;
    let ctx = browser.default_context();
    let page = ctx
        .open_tab(url, wait_until)
        .await
        .map_err(|e| RouterError::nav(format!("tab.open: {e}")))?;
    page.refresh_target_info().await.ok();
    // SPEC §10 M5 (N13) — attach the console + exception forwarders so the
    // freshly-opened tab's events reach the bound MCP client. Without this,
    // every `tab.open` since session.register would silently drop console /
    // exception / dialog / download / network notifications.
    if let Some(entry) = current_session() {
        super::attach_page_event_forwarders(&entry, Arc::clone(&page));
    }
    // SPEC §10 M10 — if this session has trace recording enabled, attach
    // the 500 ms DOM-snapshot driver to the freshly opened tab.
    if let (Some(state), Some(entry)) = (current_state(), current_session()) {
        if entry
            .trace_enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some(writer) = state.traces.get(&entry.session_id) {
                let sink: Arc<dyn observability::trace::TraceSink> = writer.clone();
                crate::trace_drivers::attach_trace_driver_for_page(&entry, sink, Arc::clone(&page));
            }
        }
    }
    Ok(json!({
        "tab_id": page.tab_id().0,
        "target_id": page.target_id(),
        "frame_id": "",
        "url": page.url(),
        "title": page.title(),
    }))
}

pub(super) async fn tab_list(browser: &Browser) -> ToolResult {
    let ctx = browser.default_context();
    let tabs: Vec<Value> = ctx
        .list_tabs()
        .into_iter()
        .map(|p| {
            json!({
                "tab_id": p.tab_id().0,
                "url": p.url(),
                "title": p.title(),
                "active": false,
            })
        })
        .collect();
    Ok(json!({ "tabs": tabs }))
}

pub(super) async fn tab_close(browser: &Browser, params: Value) -> ToolResult {
    let tab_id = required_str(&params, "tab_id")?;
    let ctx = browser.default_context();
    ctx.close_tab(&browser_engine::TabId(tab_id.into()))
        .await
        .map_err(|_| RouterError::tab_not_found())?;
    Ok(json!({"ok": true}))
}

pub(super) async fn tab_focus(browser: &Browser, params: Value) -> ToolResult {
    let tab_id = required_str(&params, "tab_id")?;
    let ctx = browser.default_context();
    let page = ctx
        .get(&browser_engine::TabId(tab_id.into()))
        .ok_or_else(RouterError::tab_not_found)?;
    page.bring_to_front()
        .await
        .map_err(|e| RouterError::internal(format!("Page.bringToFront: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn tab_navigate(browser: &Browser, params: Value) -> ToolResult {
    let tab_id = required_str(&params, "tab_id")?;
    let url = required_str(&params, "url")?;
    validate_navigable_url(url).map_err(|e| RouterError::nav(format!("tab.navigate: {e}")))?;
    let wait_until = parse_wait_until(params.get("wait_until"))?;
    let ctx = browser.default_context();
    let page = ctx
        .get(&browser_engine::TabId(tab_id.into()))
        .ok_or_else(RouterError::tab_not_found)?;
    page.navigate(url, wait_until)
        .await
        .map_err(|e| RouterError::nav(format!("tab.navigate: {e}")))?;
    Ok(json!({"frame_id": "", "url": page.url(), "title": page.title()}))
}

pub(super) async fn tab_wait(browser: &Browser, params: Value) -> ToolResult {
    let tab_id = required_str(&params, "tab_id")?;
    let timeout_ms = params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let deadline = Duration::from_millis(timeout_ms);
    let ctx = browser.default_context();
    let page = ctx
        .get(&browser_engine::TabId(tab_id.into()))
        .ok_or_else(RouterError::tab_not_found)?;

    let start = std::time::Instant::now();
    let predicate = params
        .get("predicate")
        .ok_or_else(|| RouterError::invalid_params("missing predicate"))?;
    if let Some(s) = predicate.as_str() {
        match s {
            "load" => page.wait_for_lifecycle(WaitUntil::Load, deadline).await,
            "networkidle" => {
                page.wait_for_network_idle(Duration::from_millis(500), deadline)
                    .await
            }
            other => {
                return Err(RouterError::invalid_params(format!(
                    "unknown predicate: {other}"
                )));
            }
        }
        .map_err(|e| RouterError::timeout(e.to_string()))?;
    } else if let Some(sel) = predicate.get("selector").and_then(Value::as_str) {
        page.wait_for_selector(sel, deadline)
            .await
            .map_err(|e| RouterError::timeout(e.to_string()))?;
    } else if let Some(re) = predicate.get("url_regex").and_then(Value::as_str) {
        page.wait_for_url(re, deadline)
            .await
            .map_err(|e| RouterError::timeout(e.to_string()))?;
    } else {
        return Err(RouterError::invalid_params("malformed predicate"));
    }

    Ok(json!({"ok": true, "elapsed_ms": start.elapsed().as_millis() as u64}))
}
