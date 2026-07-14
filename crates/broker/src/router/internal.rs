//! `_internal.*` broker introspection + `app.*` native macOS app control. SPEC §7 + §11 V2.

use std::sync::Arc;

use observability::trace::TraceSink;
use serde_json::{json, Value};

use crate::protocol::{ErrorCode, JsonRpcResponse};
use crate::registry::SessionEntry;
use crate::State;

use super::{required_str, RouterError, ToolResult};

// ---------- _internal.* introspection ----------

/// SPEC §7 — `_internal.status`.
pub(super) fn handle_internal_status(state: Arc<State>, id: Value) -> JsonRpcResponse {
    let sessions: Vec<Value> = state
        .registry
        .iter()
        .map(|(sid, entry)| {
            json!({
                "session_id": sid,
                "label": entry.context_label.read().clone(),
                "context_id": sid,
                "tab_count": entry.browser.load().default_context().list_tabs().len(),
                "terminal_count": entry.terminal_controller.session_count(),
                "last_activity_ms": entry.last_activity_age_ms(),
                "created_at_ms": entry.created_at_unix_ms,
            })
        })
        .collect();
    let totals_sessions = sessions.len();
    let totals_tabs: u64 = sessions
        .iter()
        .filter_map(|s| s.get("tab_count").and_then(Value::as_u64))
        .sum();
    let totals_terminals: u64 = sessions
        .iter()
        .filter_map(|s| s.get("terminal_count").and_then(Value::as_u64))
        .sum();
    let chromium: Vec<Value> = state
        .registry
        .iter()
        .map(|(sid, entry)| {
            let browser = entry.browser.load_full();
            let pid = browser.pid().unwrap_or(0);
            let rss_bytes = if pid > 0 {
                system_control::process::info(pid as i32)
                    .map(|info| info.rss_bytes)
                    .unwrap_or(0)
            } else {
                0
            };
            json!({
                "session_id": sid,
                "pid": pid,
                "rss_bytes": rss_bytes,
            })
        })
        .collect();
    JsonRpcResponse::ok(
        id,
        json!({
            "broker_version": env!("CARGO_PKG_VERSION"),
            "uptime_ms": state.started_at.elapsed().as_millis() as u64,
            "sessions": sessions,
            "chromium": chromium,
            "totals": {
                "sessions": totals_sessions,
                "contexts": totals_sessions,
                "tabs": totals_tabs,
                "terminals": totals_terminals,
            }
        }),
    )
}

/// SPEC §7 — `_internal.metrics`.
pub(super) fn handle_internal_metrics(state: Arc<State>, id: Value) -> JsonRpcResponse {
    use std::sync::atomic::Ordering;

    // Per-session CDP method histograms. Each `SessionEntry` holds a
    // `Browser` that owns its own `CdpMetricsSink`; we collect a snapshot
    // per session so operators can spot which session is generating retries
    // or transport errors. Sessions with zero CDP traffic are omitted to
    // keep the JSON small for the common idle case.
    let mut cdp_methods = serde_json::Map::new();
    let mut vision = serde_json::Map::new();
    let mut trace = serde_json::Map::new();
    for (sid, entry) in state.registry.iter() {
        let browser = entry.browser.load_full();
        let snap = browser.cdp_metrics().snapshot();
        if !snap.methods.is_empty() {
            if let Ok(v) = serde_json::to_value(&snap) {
                cdp_methods.insert(sid.clone(), v);
            }
        }

        let vision_snap = entry.vision_metrics.snapshot_all();
        if !vision_snap.is_empty() {
            if let Ok(v) = serde_json::to_value(&vision_snap) {
                vision.insert(sid.clone(), v);
            }
        }

        if let Some(writer) = state.traces.get(&sid) {
            let trace_snap = json!({
                "write_latency": writer.write_latency_snapshot(),
                "dropped": writer.dropped_count(),
                "drop_alarm_active": writer.drop_alarm_active(),
            });
            trace.insert(sid.clone(), trace_snap);
        }
    }

    JsonRpcResponse::ok(
        id,
        json!({
            "counters": {
                "requests": state.request_counter.load(Ordering::Relaxed),
                "errors": state.error_counter.load(Ordering::Relaxed),
                "session_register_rejected_cap":
                    state.session_register_rejected_cap.load(Ordering::Relaxed),
            },
            "sessions": state.metrics.snapshot(),
            "cdp_methods": cdp_methods,
            "fetch": observability::metrics::fetch_metrics().snapshot(),
            "perf": observability::metrics::perf_metrics().snapshot(),
            "mutation": observability::metrics::mutation_metrics().snapshot(),
            "vision": vision,
            "trace": trace,
            "max_sessions": state.max_sessions,
        }),
    )
}

// ---------- SPEC §11 V2 — native macOS app control ----------

/// Capability + permission gate that every `app.*` handler runs first.
/// Returns `Err` with the appropriate JSON-RPC error code:
///   - `-32009 PermissionDenied` with `data.settings_url` if AX trust is missing
///   - `-32009 PermissionDenied` if the session lacks `"native"` capability
fn require_native(session: &SessionEntry) -> Result<(), RouterError> {
    if !session.has_native_capability() {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: "session lacks 'native' capability — pass capabilities: [\"native\"] to session.register".to_string(),
            data: Some(json!({"capability": "native"})),
        });
    }
    if !native_control::permission::is_trusted() {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: "Accessibility permission missing — grant access to one-for-all-broker in System Settings".to_string(),
            data: Some(json!({
                "settings_url": native_control::permission::settings_deeplink(),
            })),
        });
    }
    Ok(())
}

fn map_native_err(r: &str, e: native_control::NativeControlError) -> RouterError {
    use native_control::NativeControlError as NE;
    match e {
        NE::PermissionMissing { settings_url } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: "Accessibility permission missing".to_string(),
            data: Some(json!({"settings_url": settings_url})),
        },
        NE::AppNotFound { bundle_id } => RouterError {
            code: ErrorCode::TabNotFound,
            message: format!("app not found: {bundle_id}"),
            data: None,
        },
        NE::RefStale { .. } => RouterError {
            code: ErrorCode::ElementStale,
            message: format!("ref {r:?} not found in current snapshot"),
            data: None,
        },
        NE::NotActionable { reason, .. } => RouterError {
            code: ErrorCode::ElementNotActionable,
            message: format!("element not actionable: {reason}"),
            data: None,
        },
        NE::ActivateForbidden { reason } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("AppleScript activate forbidden: {reason}"),
            data: None,
        },
        NE::ScreenRecordingMissing { settings_url } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: "Screen Recording permission missing".to_string(),
            data: Some(json!({"settings_url": settings_url})),
        },
        NE::Blocked { bundle_id } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("app {bundle_id:?} is blocked by session policy"),
            data: Some(json!({"bundle_id": bundle_id})),
        },
        NE::FocusStealForbidden { what } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("focus-stealing action forbidden: {what}"),
            data: Some(json!({"capability": "focus_steal"})),
        },
        NE::PrivateApiUnavailable { what } => RouterError {
            code: ErrorCode::InternalError,
            message: format!("private API unavailable: {what}"),
            data: None,
        },
        NE::Cgs(c) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("CGS error: {c}"),
            data: None,
        },
        NE::Tis(c) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("TIS error: {c}"),
            data: None,
        },
        NE::Timeout(what) => RouterError {
            code: ErrorCode::Timeout,
            message: format!("native timeout: {what}"),
            data: None,
        },
        NE::AppleScript { msg } => RouterError {
            code: ErrorCode::InternalError,
            message: format!("AppleScript: {msg}"),
            data: None,
        },
        NE::AxError(c) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("AX call failed (AXError={c})"),
            data: None,
        },
        NE::Io(s) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("io: {s}"),
            data: None,
        },
        NE::Internal(s) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("internal: {s}"),
            data: None,
        },
        NE::UnsupportedPlatform => RouterError {
            code: ErrorCode::InternalError,
            message: "native-control unsupported on this platform".to_string(),
            data: None,
        },
    }
}

pub(super) async fn app_list(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    let apps = native_control::list_apps()
        .await
        .map_err(|e| map_native_err("", e))?;
    let arr: Vec<Value> = apps
        .into_iter()
        .filter(|app| !session.app_controller.is_blocked(&app.bundle_id))
        .map(|a| {
            json!({
                "bundle_id": a.bundle_id,
                "pid": a.pid,
                "name": a.name,
                "has_focus": a.has_focus,
            })
        })
        .collect();
    Ok(json!({"apps": arr}))
}

pub(super) async fn app_snapshot(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let snap = session
        .app_controller
        .snapshot(bundle_id)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(serde_json::to_value(&*snap).unwrap_or(Value::Null))
}

pub(super) async fn app_click(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let r = required_str(&params, "ref")?;
    session
        .app_controller
        .click(bundle_id, r)
        .await
        .map_err(|e| map_native_err(r, e))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn app_type(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let r = required_str(&params, "ref")?;
    let text = required_str(&params, "text")?;
    let clear_first = params
        .get("clear_first")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    session
        .app_controller
        .type_text(bundle_id, r, text, clear_first)
        .await
        .map_err(|e| map_native_err(r, e))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn app_scroll(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let r = params.get("ref").and_then(Value::as_str);
    let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    session
        .app_controller
        .scroll(bundle_id, r, dx, dy)
        .await
        .map_err(|e| map_native_err(r.unwrap_or(""), e))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn app_eval(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let script = required_str(&params, "applescript")?;
    let v = session
        .app_controller
        .eval(bundle_id, script)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}
