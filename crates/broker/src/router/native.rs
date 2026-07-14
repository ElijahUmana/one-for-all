//! SPEC §12 U6/U7 + AX events — native macOS deep-input router handlers.
//!
//! Every method in this file follows the same pattern as the base V2 handlers
//! in [`super::internal`]: capability + AX trust gate, then dispatch to the
//! corresponding `native_control::*` function, then map the typed error onto
//! a JSON-RPC error code via [`map_native_err`].
//!
//! Focus discipline:
//! - Verbs that steal focus (window.raise, dock.click, spotlight.open,
//!   notification_center.open) require `"focus_steal"` capability AND
//!   `confirm: true` in the call params. Both checks happen here in the
//!   broker so a hostile session that lied to native-control can't bypass.

use serde_json::{json, Value};

use crate::protocol::ErrorCode;
use crate::registry::SessionEntry;

use super::{required_str, RouterError, ToolResult};

// ---------------------------------------------------------------------------
// Shared gates + error mapping (kept close to the new methods so the file is
// self-contained — internal.rs already has its own copies for the base V2
// surface, and we mirror them here to avoid pub(crate) leaks across submodules)
// ---------------------------------------------------------------------------

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
            message: "Accessibility permission missing — grant access in System Settings"
                .to_string(),
            data: Some(json!({
                "settings_url": native_control::permission::settings_deeplink(),
            })),
        });
    }
    Ok(())
}

fn ensure_indirect_app_targeting_allowed(
    blocklist_active: bool,
    method: &'static str,
) -> Result<(), RouterError> {
    if blocklist_active {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("{method} is denied when app_blocklist is active because the target app cannot be constrained safely"),
            data: Some(json!({"app_blocklist_active": true})),
        });
    }
    Ok(())
}

fn require_indirect_app_targeting_allowed(
    session: &SessionEntry,
    method: &'static str,
) -> Result<(), RouterError> {
    ensure_indirect_app_targeting_allowed(session.app_controller.privacy().has_blocklist(), method)
}

fn redact_event_text(
    session: &SessionEntry,
    mut ev: native_control::AxEvent,
) -> native_control::AxEvent {
    let engine = session.app_controller.privacy();
    if let Some(name) = ev.name.as_mut() {
        if let Some(redacted) = engine.redact_text(name) {
            *name = redacted;
        }
    }
    if let Some(value) = ev.value.as_mut() {
        if let Some(redacted) = engine.redact_text(value) {
            *value = redacted;
        }
    }
    ev
}

fn require_focus_steal(session: &SessionEntry) -> Result<(), RouterError> {
    if !session.has_capability("focus_steal") {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: "operation requires capabilities:[\"focus_steal\"] in session.register"
                .to_string(),
            data: Some(json!({"capability": "focus_steal"})),
        });
    }
    Ok(())
}

fn confirm_or_deny(params: &Value, what: &'static str) -> Result<(), RouterError> {
    if !params
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("{what} requires confirm:true to acknowledge focus shift"),
            data: Some(json!({"confirm_required": true})),
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
        NE::ScreenRecordingMissing { settings_url } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: "Screen Recording permission missing".to_string(),
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
        NE::Blocked { bundle_id } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("app {bundle_id:?} is blocked by session policy"),
            data: Some(json!({"bundle_id": bundle_id})),
        },
        NE::FocusStealForbidden { what } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("focus-stealing action forbidden: {what}"),
            data: Some(json!({"capability": "focus_steal", "confirm_required": true})),
        },
        NE::PrivateApiUnavailable { what } => RouterError {
            code: ErrorCode::InternalError,
            message: format!("private API unavailable: {what}"),
            data: Some(json!({"reason": "private_api_unavailable"})),
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
        NE::Timeout(what) => RouterError {
            code: ErrorCode::Timeout,
            message: format!("native: timeout in {what}"),
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

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Menu bar
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_menu_list(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let items = session
        .app_controller
        .menu_list(bundle_id)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"items": items}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_menu_click(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let path: Vec<String> = params
        .get("path")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| RouterError::invalid_params("missing path: array"))?;
    session
        .app_controller
        .menu_click(bundle_id, path)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_statusmenu_click(_session: &SessionEntry, params: Value) -> ToolResult {
    require_native(_session)?;
    require_indirect_app_targeting_allowed(_session, "app.statusmenu.click")?;
    let label = required_str(&params, "label")?;
    native_control::statusmenu::click(label)
        .await
        .map_err(|e| map_native_err(label, e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Notification Center
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_notif_open(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.notification_center.open")?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.notification_center.open")?;
    native_control::notification_center::open(true, true)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_notif_list(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.notification_center.list")?;
    let v = native_control::notification_center::list()
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"items": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_notif_click(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.notification_center.click")?;
    let title = required_str(&params, "title")?;
    native_control::notification_center::click(title)
        .await
        .map_err(|e| map_native_err(title, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_notif_dismiss(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.notification_center.dismiss")?;
    let title = required_str(&params, "title")?;
    native_control::notification_center::dismiss(title)
        .await
        .map_err(|e| map_native_err(title, e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Spotlight
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_spotlight_open(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.spotlight.open")?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.spotlight.open")?;
    native_control::spotlight::open(true, true)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_spotlight_query(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.spotlight.query")?;
    let text = required_str(&params, "query")?;
    native_control::spotlight::query(text)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_spotlight_select(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.spotlight.select")?;
    let index = params.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
    native_control::spotlight::select(index)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Spaces
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_spaces_list(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.spaces.list")?;
    let v = native_control::spaces::list()
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"spaces": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_spaces_switch(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.spaces.switch_to")?;
    let delta = params.get("delta").and_then(Value::as_i64).unwrap_or(0) as i32;
    native_control::spaces::switch_relative(delta)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_spaces_move_window(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let delta = params.get("delta").and_then(Value::as_i64).unwrap_or(0) as i32;
    session
        .app_controller
        .spaces_move_window(bundle_id, delta)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Dock
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_dock_list(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.dock.list")?;
    let items = native_control::dock::list()
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"items": items}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_dock_click(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.dock.click")?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.dock.click")?;
    let label = required_str(&params, "label")?;
    let has_focus_steal = session.has_capability("focus_steal");
    native_control::dock::click(label, true, has_focus_steal)
        .await
        .map_err(|e| map_native_err(label, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_dock_reveal_app(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.dock.reveal_app")?;
    let label = required_str(&params, "label")?;
    native_control::dock::reveal_app(label)
        .await
        .map_err(|e| map_native_err(label, e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — Window
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_window_list(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let v = session
        .app_controller
        .window_list(bundle_id)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"windows": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_window_raise(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.window.raise")?;
    let bundle_id = required_str(&params, "app_id")?;
    let window_id = required_str(&params, "window_id")?;
    let has_focus_steal = session.has_capability("focus_steal");
    session
        .app_controller
        .window_raise(bundle_id, window_id, true, has_focus_steal)
        .await
        .map_err(|e| map_native_err(window_id, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_window_minimize(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let window_id = required_str(&params, "window_id")?;
    let value = params
        .get("minimized")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    session
        .app_controller
        .window_set_minimized(bundle_id, window_id, value)
        .await
        .map_err(|e| map_native_err(window_id, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_window_fullscreen(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let window_id = required_str(&params, "window_id")?;
    let value = params
        .get("fullscreen")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    session
        .app_controller
        .window_set_fullscreen(bundle_id, window_id, value)
        .await
        .map_err(|e| map_native_err(window_id, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_window_move(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let window_id = required_str(&params, "window_id")?;
    let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    session
        .app_controller
        .window_move_to(bundle_id, window_id, x, y)
        .await
        .map_err(|e| map_native_err(window_id, e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_window_resize(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let bundle_id = required_str(&params, "app_id")?;
    let window_id = required_str(&params, "window_id")?;
    let w = params.get("w").and_then(Value::as_f64).unwrap_or(0.0);
    let h = params.get("h").and_then(Value::as_f64).unwrap_or(0.0);
    session
        .app_controller
        .window_resize(bundle_id, window_id, w, h)
        .await
        .map_err(|e| map_native_err(window_id, e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U6 — TouchBar / Gesture / IME / Scripting
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_touchbar_tap(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.touchbar.tap")?;
    let identifier = required_str(&params, "identifier")?;
    native_control::touchbar::tap(identifier)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_gesture_swipe(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.gesture.three_finger_swipe")?;
    let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    native_control::gesture::three_finger_swipe(dx, dy)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_force_touch(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.force_touch")?;
    let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    let pressure = params
        .get("pressure")
        .and_then(Value::as_f64)
        .unwrap_or(0.5);
    native_control::gesture::force_touch(x, y, pressure)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_ime_list(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.ime.list")?;
    let v = native_control::ime::list()
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"input_sources": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_ime_switch(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.ime.switch")?;
    let id = required_str(&params, "input_id")?;
    native_control::ime::switch(id)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_ime_set(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.ime.set_input_source")?;
    let id = required_str(&params, "input_id")?;
    native_control::ime::set_input_source(id)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_shortcut_run(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.shortcut.run")?;
    let name = required_str(&params, "name")?;
    let input = params.get("input").and_then(Value::as_str);
    let v = native_control::scripting::shortcut_run(name, input)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_automator_run(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.automator.run")?;
    let path = required_str(&params, "workflow_path")?;
    let v = native_control::scripting::automator_run(path)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_applescript(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.applescript")?;
    let src = required_str(&params, "source")?;
    let v = native_control::scripting::applescript(src)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_jxa(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.javascript_for_automation")?;
    let src = required_str(&params, "source")?;
    let v = native_control::scripting::jxa(src)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_terminal_spawn(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.terminal.spawn_session")?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.terminal.spawn_session")?;
    let cmd = required_str(&params, "command")?;
    let v = native_control::scripting::terminal_spawn_session(
        cmd,
        true,
        session.has_capability("focus_steal"),
    )
    .await
    .map_err(|e| map_native_err("", e))?;
    Ok(json!({"value": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_quicklook_preview(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.quicklook.preview")?;
    require_focus_steal(session)?;
    confirm_or_deny(&params, "app.quicklook.preview")?;
    let path = required_str(&params, "path")?;
    native_control::quicklook::preview(path)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_quicklook_close(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "app.quicklook.close")?;
    native_control::quicklook::close()
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U7 — Clipboard
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_read_string(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    let cache = session.app_controller.clipboard();
    let engine = session.app_controller.privacy();
    let v = native_control::clipboard::read_string(cache, engine)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"text": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_write_string(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let text = required_str(&params, "text")?;
    let cache = session.app_controller.clipboard();
    native_control::clipboard::write_string(cache, text)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_read_files(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    let cache = session.app_controller.clipboard();
    let engine = session.app_controller.privacy();
    let v = native_control::clipboard::read_files(cache, engine)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"files": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_write_files(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let cache = session.app_controller.clipboard();
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| RouterError::invalid_params("missing paths: array"))?;
    native_control::clipboard::write_files(cache, paths)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_read_image(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    use base64::Engine as _;
    let cache = session.app_controller.clipboard();
    let bytes = native_control::clipboard::read_image(cache)
        .await
        .map_err(|e| map_native_err("", e))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(json!({"png_base64": b64}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_write_image(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    use base64::Engine as _;
    let b64 = required_str(&params, "png_base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| RouterError::invalid_params(format!("png_base64: {e}")))?;
    let cache = session.app_controller.clipboard();
    native_control::clipboard::write_image(cache, bytes)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_types(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    let cache = session.app_controller.clipboard();
    let v = native_control::clipboard::types(cache)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"types": v}))
}

#[cfg(target_os = "macos")]
pub(super) async fn clipboard_history(session: &SessionEntry) -> ToolResult {
    require_native(session)?;
    let cache = session.app_controller.clipboard();
    let engine = session.app_controller.privacy();
    let v = native_control::clipboard::history(cache, engine)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"items": v}))
}

// ---------------------------------------------------------------------------
// SPEC §12 U7 — Drag
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn drag_from_finder(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "drag.from_finder")?;
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| RouterError::invalid_params("missing paths: array"))?;
    let from_x = params.get("from_x").and_then(Value::as_f64).unwrap_or(0.0);
    let from_y = params.get("from_y").and_then(Value::as_f64).unwrap_or(0.0);
    let to_x = params.get("to_x").and_then(Value::as_f64).unwrap_or(0.0);
    let to_y = params.get("to_y").and_then(Value::as_f64).unwrap_or(0.0);
    native_control::drag::from_finder(paths, from_x, from_y, to_x, to_y)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

#[cfg(target_os = "macos")]
pub(super) async fn drag_between_apps(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    require_indirect_app_targeting_allowed(session, "drag.between_apps")?;
    let from_x = params.get("from_x").and_then(Value::as_f64).unwrap_or(0.0);
    let from_y = params.get("from_y").and_then(Value::as_f64).unwrap_or(0.0);
    let to_x = params.get("to_x").and_then(Value::as_f64).unwrap_or(0.0);
    let to_y = params.get("to_y").and_then(Value::as_f64).unwrap_or(0.0);
    native_control::drag::between_apps(from_x, from_y, to_x, to_y)
        .await
        .map_err(|e| map_native_err("", e))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// SPEC §12 — `app.subscribe` / `app.unsubscribe`
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(super) async fn app_subscribe(
    state: &std::sync::Arc<crate::State>,
    session: &std::sync::Arc<SessionEntry>,
    params: Value,
) -> ToolResult {
    require_native(session)?;
    use native_control::AxEventTopic;
    let bundle_id = required_str(&params, "app_id")?;
    let topics: Vec<AxEventTopic> = params
        .get("topics")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().and_then(AxEventTopic::parse))
                .collect()
        })
        .ok_or_else(|| RouterError::invalid_params("missing topics: array"))?;
    let (info, mut rx) = session
        .app_controller
        .subscribe(bundle_id, &topics)
        .map_err(|e| map_native_err("", e))?;

    // Spawn a forwarder that drains the receiver into the session's
    // event/notify channel and — when tracing is enabled for this session —
    // doubles every event into a synthetic `tool="app.event"` Action record
    // so `ofa-trace` / replay tooling can correlate AX events with their
    // dispatch context (mirrors the cdp_event trace pattern m10-implementer
    // wired into browser-engine's event pump).
    let entry = std::sync::Arc::clone(session);
    let state = std::sync::Arc::clone(state);
    let session_id = session.session_id.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            use crate::ServerEvent;
            let redacted = redact_event_text(&entry, ev);
            let payload = serde_json::to_value(&redacted).unwrap_or(Value::Null);
            // Trace doubling — only when this session has trace enabled.
            // Cheap atomic-load hot path; cold path (Some writer) clones the
            // payload once.
            if entry
                .trace_enabled
                .load(std::sync::atomic::Ordering::Acquire)
            {
                if let Some(writer) = state.traces.get(&session_id) {
                    use observability::trace::{TraceEvent, TraceSink};
                    writer.record(TraceEvent::Action {
                        ts_ms: writer.now_ms(),
                        session_id: session_id.clone(),
                        // AX events aren't tab-scoped — bundle_id stands in
                        // for the per-event grouping key the ofa-trace tools
                        // expect in `tab_id`.
                        tab_id: redacted.bundle_id.clone(),
                        tool: "app.event".into(),
                        args: serde_json::json!({
                            "topic": redacted.topic,
                            "bundle_id": redacted.bundle_id,
                        }),
                        result: payload.clone(),
                    });
                }
            }
            let server_event = ServerEvent {
                jsonrpc: "2.0".into(),
                method: "event/notify".into(),
                params: json!({
                    "topic": "app.event",
                    "session_id": session_id,
                    "payload": payload,
                }),
            };
            let _ = entry.try_push(server_event);
        }
    });

    Ok(json!({"subscription": info}))
}

#[cfg(target_os = "macos")]
pub(super) async fn app_unsubscribe(session: &SessionEntry, params: Value) -> ToolResult {
    require_native(session)?;
    let id = required_str(&params, "subscription_id")?;
    let removed = session.app_controller.unsubscribe(id);
    Ok(json!({"ok": removed}))
}

// ---------------------------------------------------------------------------
// Non-macOS stubs — return UnsupportedPlatform for every method so the broker
// can compile end-to-end on Linux CI without a sea of cfg gates at the call
// site.
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "macos"))]
fn unsupported() -> RouterError {
    RouterError {
        code: ErrorCode::InternalError,
        message: "native-control unsupported on this platform".to_string(),
        data: None,
    }
}

#[cfg(not(target_os = "macos"))]
macro_rules! unsupported_handlers {
    ($($name:ident),* $(,)?) => {
        $(
            pub(super) async fn $name(_session: &SessionEntry, _params: Value) -> ToolResult {
                Err(unsupported())
            }
        )*
    };
}

#[cfg(not(target_os = "macos"))]
unsupported_handlers! {
    app_menu_list, app_menu_click, app_statusmenu_click,
    app_notif_open, app_notif_click, app_notif_dismiss,
    app_spotlight_open, app_spotlight_query, app_spotlight_select,
    app_spaces_switch, app_spaces_move_window,
    app_dock_click, app_dock_reveal_app,
    app_window_list, app_window_raise, app_window_minimize,
    app_window_fullscreen, app_window_move, app_window_resize,
    app_touchbar_tap, app_gesture_swipe, app_force_touch,
    app_ime_switch, app_ime_set,
    app_shortcut_run, app_automator_run, app_applescript, app_jxa,
    app_terminal_spawn, app_quicklook_preview,
    clipboard_write_string, clipboard_write_files, clipboard_write_image,
    drag_from_finder, drag_between_apps,
    app_unsubscribe,
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn app_notif_list(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn app_spaces_list(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn app_dock_list(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn app_ime_list(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn app_quicklook_close(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn clipboard_read_string(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn clipboard_read_files(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn clipboard_read_image(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn clipboard_types(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn clipboard_history(_session: &SessionEntry) -> ToolResult {
    Err(unsupported())
}
#[cfg(not(target_os = "macos"))]
pub(super) async fn app_subscribe(
    _state: &std::sync::Arc<crate::State>,
    _session: &std::sync::Arc<SessionEntry>,
    _params: Value,
) -> ToolResult {
    Err(unsupported())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indirect_app_targeting_guard_allows_when_blocklist_inactive() {
        ensure_indirect_app_targeting_allowed(false, "app.notification_center.list").unwrap();
    }

    #[test]
    fn indirect_app_targeting_guard_denies_when_blocklist_active() {
        let err = ensure_indirect_app_targeting_allowed(true, "app.notification_center.list")
            .expect_err("active blocklist must deny indirect app targeting");
        assert!(matches!(err.code, ErrorCode::PermissionDenied));
        assert!(
            err.message
                .contains("app.notification_center.list is denied when app_blocklist is active"),
            "unexpected error message: {}",
            err.message
        );
        assert_eq!(
            err.data,
            Some(json!({"app_blocklist_active": true})),
            "guard must surface machine-readable policy denial data"
        );
    }
}
