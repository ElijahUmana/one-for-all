//! `page.*` content tools + `vision.*` per-tab observability. SPEC §7 + §11 V4.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use browser_engine::{Browser, Page};

use crate::protocol::ErrorCode;
use crate::registry::SessionEntry;

use super::{
    deterministic_seed, locate_page, required_str, resolve_ref, resolve_ref_str, RouterError,
    ToolResult,
};

fn modifier_bits(params: &Value) -> Result<i64, RouterError> {
    let Some(values) = params.get("modifiers").and_then(Value::as_array) else {
        return Ok(0);
    };
    let names: Vec<&str> = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| RouterError::invalid_params("modifiers must be strings"))
        })
        .collect::<Result<_, _>>()?;
    browser_engine::input_translation::parse_modifier_list(names)
        .map_err(RouterError::invalid_params)
}

fn scroll_easing(
    params: &Value,
) -> Result<browser_engine::input_translation::ScrollEasing, RouterError> {
    match params
        .get("easing")
        .and_then(Value::as_str)
        .unwrap_or("linear")
    {
        "linear" => Ok(browser_engine::input_translation::ScrollEasing::Linear),
        "ease_out" => Ok(browser_engine::input_translation::ScrollEasing::EaseOut),
        "ease_in_out" => Ok(browser_engine::input_translation::ScrollEasing::EaseInOut),
        other => Err(RouterError::invalid_params(format!(
            "unsupported easing {other:?}"
        ))),
    }
}

fn pointer_details(
    params: &Value,
) -> Result<browser_engine::input_translation::PointerDetails, RouterError> {
    fn ranged_f64(
        params: &Value,
        field: &'static str,
        min: f64,
        max: f64,
    ) -> Result<Option<f64>, RouterError> {
        let Some(value) = params.get(field) else {
            return Ok(None);
        };
        let parsed = value
            .as_f64()
            .ok_or_else(|| RouterError::invalid_params(format!("{field} must be a number")))?;
        if !(min..=max).contains(&parsed) {
            return Err(RouterError::invalid_params(format!(
                "{field} must be in [{min}, {max}]"
            )));
        }
        Ok(Some(parsed))
    }

    fn ranged_i64(
        params: &Value,
        field: &'static str,
        min: i64,
        max: i64,
    ) -> Result<Option<i64>, RouterError> {
        let Some(value) = params.get(field) else {
            return Ok(None);
        };
        let parsed = value
            .as_i64()
            .ok_or_else(|| RouterError::invalid_params(format!("{field} must be an integer")))?;
        if !(min..=max).contains(&parsed) {
            return Err(RouterError::invalid_params(format!(
                "{field} must be in [{min}, {max}]"
            )));
        }
        Ok(Some(parsed))
    }

    Ok(browser_engine::input_translation::PointerDetails {
        force: ranged_f64(params, "pressure", 0.0, 1.0)?,
        tangential_pressure: ranged_f64(params, "tangential_pressure", -1.0, 1.0)?,
        tilt_x: ranged_f64(params, "tilt_x", -90.0, 90.0)?,
        tilt_y: ranged_f64(params, "tilt_y", -90.0, 90.0)?,
        twist: ranged_i64(params, "twist", 0, 359)?,
    })
}

// ---------- page.* ----------

/// SPEC §10 M2 + N19 — validate the caller's `snapshot_seq` against the
/// page's current high-water mark. The snapshot a `ref` was minted from
/// MUST still be the most recent one; otherwise the element bounding box
/// could be stale (the page has navigated/mutated since) and the click
/// would land on whatever happens to be at those coordinates now.
///
/// Returns:
/// - `Ok(provided)` when `snapshot_seq` is missing (legacy callers — we
///   defensively allow it but log; future strict-mode flips this to err)
/// - `Ok(provided)` when `snapshot_seq == current`
/// - `Err(-32004 ElementStale { data: { expected, got } })` when older
/// - `Err(-32602 InvalidParams)` for unparseable values
fn validate_snapshot_seq(
    page: &browser_engine::Page,
    params: &Value,
) -> std::result::Result<Option<u64>, RouterError> {
    let raw = match params.get("snapshot_seq") {
        Some(v) => v,
        None => return Ok(None),
    };
    let provided = raw.as_u64().ok_or_else(|| RouterError {
        code: ErrorCode::InvalidParams,
        message: "snapshot_seq must be a u64".to_string(),
        data: None,
    })?;
    let current = page.current_snapshot_seq();
    if provided < current {
        return Err(RouterError {
            code: ErrorCode::ElementStale,
            message: format!(
                "ref minted from snapshot_seq={provided}; page is at snapshot_seq={current}",
            ),
            data: Some(json!({
                "expected": current,
                "got": provided,
            })),
        });
    }
    Ok(Some(provided))
}

pub(super) async fn page_snapshot(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    // SPEC §10 M2 — `since_seq` switches to the delta path. When absent, we
    // return the full M1-shaped snapshot. When present, browser-engine
    // chooses between a delta and a full-snapshot fallback (see SPEC §7
    // "Snapshot delta shape").
    let since_seq = params.get("since_seq").and_then(Value::as_u64);
    let resp = match since_seq {
        Some(n) => page
            .snapshot_delta_since(n)
            .await
            .map_err(|e| RouterError::internal(format!("snapshot_delta: {e}")))?,
        None => browser_engine::SnapshotResponse::Full(
            page.snapshot()
                .await
                .map_err(|e| RouterError::internal(format!("snapshot: {e}")))?,
        ),
    };
    Ok(serde_json::to_value(resp).unwrap_or(Value::Null))
}

pub(super) async fn page_screenshot(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let format = params
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("png")
        .to_owned();
    let quality = params
        .get("quality")
        .and_then(Value::as_u64)
        .map(|q| q as u8);
    let cbv = params
        .get("capture_beyond_viewport")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let clip = if params.get("ref").is_some() {
        validate_snapshot_seq(&page, &params)?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| RouterError::internal(format!("screenshot pre-snapshot: {e}")))?;
        let elem = resolve_ref(&snap.elements, &params)?;
        Some(elem.bbox.clone())
    } else {
        None
    };
    let data = page
        .screenshot(&format, quality, cbv, clip.as_ref())
        .await
        .map_err(|e| RouterError::internal(format!("screenshot: {e}")))?;
    Ok(json!({"format": format, "data_base64": data, "width": 0, "height": 0}))
}

pub(super) async fn page_read_text(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let elem = if params.get("ref").is_some() {
        validate_snapshot_seq(&page, &params)?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| RouterError::internal(format!("read_text pre-snapshot: {e}")))?;
        Some(resolve_ref(&snap.elements, &params)?.clone())
    } else {
        None
    };
    let text = page
        .read_text(elem.as_ref())
        .await
        .map_err(|e| RouterError::internal(format!("read_text: {e}")))?;
    Ok(json!({"text": text}))
}

pub(super) async fn page_click(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    // N19 — refuse a click whose `snapshot_seq` is older than the current
    // page state BEFORE doing any expensive work. Equal seqs proceed; the
    // pre-snapshot below is the freshest source of truth.
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("click pre-snapshot: {e}")))?;
    let elem = resolve_ref(&snap.elements, &params)?;
    let button = match params
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
    {
        "right" => browser_engine::input_translation::MouseButton::Right,
        "middle" => browser_engine::input_translation::MouseButton::Middle,
        _ => browser_engine::input_translation::MouseButton::Left,
    };
    let click_count = params
        .get("click_count")
        .and_then(Value::as_u64)
        .unwrap_or(1) as u32;
    let realistic = params
        .get("realistic")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| browser.default_realistic());
    let nav = page
        .click(
            elem,
            button,
            click_count,
            realistic,
            deterministic_seed(&snap),
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("zero area") {
                RouterError::not_actionable()
            } else {
                RouterError::internal(format!("click: {e}"))
            }
        })?;
    let nav_val = nav.map(|n| json!({"frame_id": n.frame_id, "url": n.url}));
    Ok(json!({"ok": true, "navigation": nav_val}))
}

pub(super) async fn page_type(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("type pre-snapshot: {e}")))?;
    let elem = resolve_ref(&snap.elements, &params)?;
    let text = required_str(&params, "text")?;
    let delay_ms = params.get("delay_ms").and_then(Value::as_u64);
    let clear_first = params
        .get("clear_first")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    page.type_text(elem, text, delay_ms, clear_first, deterministic_seed(&snap))
        .await
        .map_err(|e| RouterError::internal(format!("type: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_keypress(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let key = required_str(&params, "key")?;
    let modifiers = modifier_bits(&params)?;
    page.keypress_with_modifiers(key, modifiers)
        .await
        .map_err(|e| RouterError::internal(format!("keypress: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_scroll(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    let elem = if params.get("ref").is_some() {
        validate_snapshot_seq(&page, &params)?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| RouterError::internal(format!("scroll pre-snapshot: {e}")))?;
        Some(resolve_ref(&snap.elements, &params)?.clone())
    } else {
        None
    };
    page.scroll(elem.as_ref(), dx, dy)
        .await
        .map_err(|e| RouterError::internal(format!("scroll: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_hover(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("hover pre-snapshot: {e}")))?;
    let elem = resolve_ref(&snap.elements, &params)?;
    page.hover(elem).await.map_err(|e| {
        if e.to_string().contains("zero area") {
            RouterError::not_actionable()
        } else {
            RouterError::internal(format!("hover: {e}"))
        }
    })?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_drag(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("drag pre-snapshot: {e}")))?;
    let from_ref = params
        .get("from_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("from_ref"))?;
    let to_ref = params
        .get("to_ref")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("to_ref"))?;
    let from = resolve_ref_str(&snap.elements, from_ref)?;
    let to = resolve_ref_str(&snap.elements, to_ref)?;
    page.drag(from, to, deterministic_seed(&snap))
        .await
        .map_err(|e| RouterError::internal(format!("drag: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_touch_tap(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("x must be a number"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("y must be a number"))?;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(50);
    let tap_count = params.get("tap_count").and_then(Value::as_u64).unwrap_or(1) as u32;
    page.touch_tap(x, y, duration_ms, tap_count)
        .await
        .map_err(|e| RouterError::internal(format!("touch.tap: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_touch_swipe(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let start_x = params
        .get("start_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("start_x must be a number"))?;
    let start_y = params
        .get("start_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("start_y must be a number"))?;
    let end_x = params
        .get("end_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("end_x must be a number"))?;
    let end_y = params
        .get("end_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("end_y must be a number"))?;
    let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180);
    page.touch_swipe((start_x, start_y), (end_x, end_y), steps, duration_ms)
        .await
        .map_err(|e| RouterError::internal(format!("touch.swipe: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_touch_pinch(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let center_x = params
        .get("center_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_x must be a number"))?;
    let center_y = params
        .get("center_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_y must be a number"))?;
    let start_radius = params
        .get("start_radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("start_radius must be a number"))?;
    let end_radius = params
        .get("end_radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("end_radius must be a number"))?;
    let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180);
    page.touch_pinch(
        center_x,
        center_y,
        start_radius,
        end_radius,
        steps,
        duration_ms,
    )
    .await
    .map_err(|e| RouterError::internal(format!("touch.pinch: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_touch_rotate(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let center_x = params
        .get("center_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_x must be a number"))?;
    let center_y = params
        .get("center_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_y must be a number"))?;
    let radius = params
        .get("radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("radius must be a number"))?;
    let angle_deg = params
        .get("angle_deg")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("angle_deg must be a number"))?;
    let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180);
    page.touch_rotate(center_x, center_y, radius, angle_deg, steps, duration_ms)
        .await
        .map_err(|e| RouterError::internal(format!("touch.rotate: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_pointer_press(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("x must be a number"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("y must be a number"))?;
    let button = match params
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
    {
        "right" => browser_engine::input_translation::MouseButton::Right,
        "middle" => browser_engine::input_translation::MouseButton::Middle,
        _ => browser_engine::input_translation::MouseButton::Left,
    };
    let click_count = params
        .get("click_count")
        .and_then(Value::as_u64)
        .unwrap_or(1) as u32;
    let pointer = pointer_details(&params)?;
    page.pointer_press(x, y, button, click_count, pointer)
        .await
        .map_err(|e| RouterError::internal(format!("pointer.press: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_pointer_move(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("x must be a number"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("y must be a number"))?;
    let buttons = params.get("buttons").and_then(Value::as_i64).unwrap_or(1);
    let pointer = pointer_details(&params)?;
    page.pointer_move(x, y, buttons, pointer)
        .await
        .map_err(|e| RouterError::internal(format!("pointer.move: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_pointer_release(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("x must be a number"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("y must be a number"))?;
    let button = match params
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
    {
        "right" => browser_engine::input_translation::MouseButton::Right,
        "middle" => browser_engine::input_translation::MouseButton::Middle,
        _ => browser_engine::input_translation::MouseButton::Left,
    };
    let click_count = params
        .get("click_count")
        .and_then(Value::as_u64)
        .unwrap_or(1) as u32;
    let pointer = pointer_details(&params)?;
    page.pointer_release(x, y, button, click_count, pointer)
        .await
        .map_err(|e| RouterError::internal(format!("pointer.release: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_gesture_pinch(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let center_x = params
        .get("center_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_x must be a number"))?;
    let center_y = params
        .get("center_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_y must be a number"))?;
    let start_radius = params
        .get("start_radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("start_radius must be a number"))?;
    let scale_factor = params
        .get("scale_factor")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("scale_factor must be a number"))?;
    let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180);
    page.gesture_pinch(
        center_x,
        center_y,
        start_radius,
        scale_factor,
        steps,
        duration_ms,
    )
    .await
    .map_err(|e| RouterError::internal(format!("gesture.pinch: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_gesture_rotate(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let center_x = params
        .get("center_x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_x must be a number"))?;
    let center_y = params
        .get("center_y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("center_y must be a number"))?;
    let radius = params
        .get("radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("radius must be a number"))?;
    let angle_deg = params
        .get("angle_deg")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("angle_deg must be a number"))?;
    let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(10) as u32;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180);
    page.gesture_rotate(center_x, center_y, radius, angle_deg, steps, duration_ms)
        .await
        .map_err(|e| RouterError::internal(format!("gesture.rotate: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_gesture_longpress(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("x must be a number"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RouterError::invalid_params("y must be a number"))?;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(500);
    page.gesture_longpress(x, y, duration_ms)
        .await
        .map_err(|e| RouterError::internal(format!("gesture.longpress: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_drag_file_drop(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("file_drop pre-snapshot: {e}")))?;
    let elem = resolve_ref_str(&snap.elements, required_str(&params, "target_ref")?)?;
    let (x, y) = (
        elem.bbox.x + elem.bbox.w / 2.0,
        elem.bbox.y + elem.bbox.h / 2.0,
    );
    let file_paths: Vec<String> = params
        .get("file_paths")
        .and_then(Value::as_array)
        .ok_or_else(|| RouterError::invalid_params("file_paths must be an array"))?
        .iter()
        .map(|value| {
            let path = value
                .as_str()
                .ok_or_else(|| RouterError::invalid_params("file_paths must contain strings"))?;
            if !Path::new(path).exists() {
                return Err(RouterError::invalid_params(format!(
                    "file path does not exist: {path}"
                )));
            }
            Ok(path.to_owned())
        })
        .collect::<Result<_, _>>()?;
    page.file_drop(x, y, &file_paths)
        .await
        .map_err(|e| RouterError::internal(format!("drag.file_drop: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_keyboard_shortcut(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let accel = required_str(&params, "accel")?;
    page.keyboard_shortcut(accel)
        .await
        .map_err(|e| RouterError::internal(format!("keyboard.shortcut: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_keyboard_ime(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let composition = required_str(&params, "composition_string")?;
    let commit = params.get("commit").and_then(Value::as_str).unwrap_or("");
    page.keyboard_ime(composition, commit)
        .await
        .map_err(|e| RouterError::internal(format!("keyboard.ime: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_dead_key(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let accent = required_str(&params, "accent")?;
    let base_str = required_str(&params, "base")?;
    let mut chars = base_str.chars();
    let base = chars
        .next()
        .ok_or_else(|| RouterError::invalid_params("base must be one character"))?;
    if chars.next().is_some() {
        return Err(RouterError::invalid_params("base must be one character"));
    }
    let composed = page
        .dead_key(accent, base)
        .await
        .map_err(|e| RouterError::internal(format!("dead_key: {e}")))?;
    Ok(json!({"ok": true, "text": composed}))
}

pub(super) async fn page_scroll_precise(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    let elem = if params.get("ref").is_some() {
        validate_snapshot_seq(&page, &params)?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| RouterError::internal(format!("scroll.precise pre-snapshot: {e}")))?;
        Some(resolve_ref(&snap.elements, &params)?.clone())
    } else {
        None
    };
    let momentum = params
        .get("momentum")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let easing = scroll_easing(&params)?;
    page.precise_scroll(elem.as_ref(), dx, dy, momentum, easing)
        .await
        .map_err(|e| RouterError::internal(format!("scroll.precise: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_tab_traversal(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let direction = match params
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("forward")
    {
        "forward" => "forward",
        "backward" => "backward",
        other => {
            return Err(RouterError::invalid_params(format!(
                "unsupported direction {other:?}"
            )))
        }
    };
    let count = params.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
    page.tab_traversal(direction, count)
        .await
        .map_err(|e| RouterError::internal(format!("tab_traversal: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_right_click_menu_navigate(
    browser: &Browser,
    _session: &SessionEntry,
    params: Value,
) -> ToolResult {
    let page = locate_page(browser, &params)?;
    validate_snapshot_seq(&page, &params)?;
    let snap = page
        .snapshot()
        .await
        .map_err(|e| RouterError::internal(format!("context-menu pre-snapshot: {e}")))?;
    let elem = resolve_ref(&snap.elements, &params)?;
    let item_path: Vec<String> = params
        .get("item_path")
        .and_then(Value::as_array)
        .ok_or_else(|| RouterError::invalid_params("item_path must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| RouterError::invalid_params("item_path must contain strings"))
        })
        .collect::<Result<_, _>>()?;
    page.right_click_menu_navigate(elem, &item_path)
        .await
        .map_err(|e| RouterError::internal(format!("right_click_menu_navigate: {e}")))?;
    Ok(json!({"ok": true}))
}

/// SPEC §12 R1 / N26 — hard cap on JavaScript expression length sent to
/// `Runtime.evaluate`. 64 KiB is well above expected one-shot expressions
/// (real code lives in injected scripts via `Page.addScriptToEvaluateOnNewDocument`,
/// not in the agent surface). Anything longer is almost certainly a
/// prompt-injection payload trying to ship a giant blob through the
/// JSON-RPC transport.
const MAX_EVAL_EXPR_LEN: usize = 64 * 1024;

/// SPEC §12 R1 / N26 — default wall-clock timeout for `page.eval`. Agent
/// expressions that don't return inside this budget are aborted from the
/// broker side; the underlying `Runtime.evaluate` continues in Chromium
/// (the agent gets control back so it can decide whether to navigate away
/// or kill the tab). Per-call override via `params.timeout_ms`, clamped to
/// `[100, 60_000]`.
const DEFAULT_EVAL_TIMEOUT_MS: u64 = 30_000;
const MIN_EVAL_TIMEOUT_MS: u64 = 100;
const MAX_EVAL_TIMEOUT_MS: u64 = 60_000;

pub(super) async fn page_eval(browser: &Browser, params: Value) -> ToolResult {
    // N26 — capability gate: session must declare `capabilities:["eval"]`.
    // The capability is checked here (not in dispatch) because `page.eval`
    // is the only handler with this requirement; threading another arg
    // through every other handler isn't justified.
    if let Some(session) = super::current_session() {
        if !session.capabilities.read().contains("eval") {
            return Err(RouterError {
                code: ErrorCode::PermissionDenied,
                message: "session lacks 'eval' capability — pass capabilities: [\"eval\"] to session.register".to_string(),
                data: Some(json!({"capability": "eval"})),
            });
        }
    } else {
        // Defensive: dispatch sets the per-call session TLS, so this branch
        // is unreachable in practice. Refuse rather than allow.
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: "no session bound for page.eval".to_string(),
            data: Some(json!({"capability": "eval"})),
        });
    }

    let page = locate_page(browser, &params)?;
    if params.get("ref").is_some() {
        validate_snapshot_seq(&page, &params)?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| RouterError::internal(format!("eval pre-snapshot: {e}")))?;
        let _ = resolve_ref(&snap.elements, &params)?;
    }
    let expr = required_str(&params, "expression")?;
    if expr.len() > MAX_EVAL_EXPR_LEN {
        return Err(RouterError {
            code: ErrorCode::InvalidParams,
            message: format!(
                "expression exceeds {} byte cap ({} bytes)",
                MAX_EVAL_EXPR_LEN,
                expr.len()
            ),
            data: Some(json!({
                "max_bytes": MAX_EVAL_EXPR_LEN,
                "got_bytes": expr.len(),
            })),
        });
    }
    let return_by_value = params
        .get("return_by_value")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let timeout_ms = params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(MIN_EVAL_TIMEOUT_MS, MAX_EVAL_TIMEOUT_MS))
        .unwrap_or(DEFAULT_EVAL_TIMEOUT_MS);

    let eval_fut = page.eval(expr, return_by_value);
    let v = match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), eval_fut).await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(RouterError::internal(format!("eval: {e}"))),
        Err(_) => {
            return Err(RouterError {
                code: ErrorCode::Timeout,
                message: format!("page.eval timed out after {timeout_ms}ms"),
                data: Some(json!({
                    "timeout_ms": timeout_ms,
                    "expr_len": expr.len(),
                })),
            });
        }
    };
    Ok(json!({"value": v}))
}

fn require_storage_state_capability(method: &'static str) -> Result<(), RouterError> {
    if let Some(session) = super::current_session() {
        if !session.has_capability("storage_state") {
            return Err(RouterError {
                code: ErrorCode::PermissionDenied,
                message: format!(
                    "session lacks 'storage_state' capability — pass capabilities: [\"storage_state\"] to session.register for {method}"
                ),
                data: Some(json!({"capability": "storage_state", "method": method})),
            });
        }
        Ok(())
    } else {
        Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("no session bound for {method}"),
            data: Some(json!({"capability": "storage_state", "method": method})),
        })
    }
}

fn parse_http_origin(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let scheme_end = trimmed.find("://")?;
    let scheme = &trimmed[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &trimmed[scheme_end + 3..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    Some(format!("{}://{}", scheme, &rest[..host_end]))
}

fn page_origin(page: &browser_engine::Page) -> Result<String, RouterError> {
    parse_http_origin(&page.url()).ok_or_else(|| RouterError {
        code: ErrorCode::ProtocolError,
        message: format!("active page has no http/https origin: {}", page.url()),
        data: Some(json!({"url": page.url()})),
    })
}

fn strip_null_fields(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.retain(|_, v| !v.is_null());
            Value::Object(map)
        }
        other => other,
    }
}

fn permission_origin_or_current(
    current_origin: &str,
    requested: Option<&str>,
    field: &'static str,
) -> Result<String, RouterError> {
    match requested {
        Some(origin) => parse_http_origin(origin).ok_or_else(|| {
            RouterError::invalid_params(format!("{field} must be an http/https origin"))
        }),
        None => Ok(current_origin.to_string()),
    }
}

async fn run_json_eval(
    page: &browser_engine::Page,
    expression: String,
    context: &'static str,
    data: Value,
) -> ToolResult {
    let raw = page
        .eval(&expression, true)
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("{context}: {e}"),
            data: Some(data.clone()),
        })?;
    let parsed = match raw.as_str() {
        Some(s) => serde_json::from_str::<Value>(s).map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("{context}: invalid JSON result: {e}"),
            data: Some(data.clone()),
        })?,
        None => raw,
    };
    if let Some(err) = parsed.get("error").and_then(Value::as_str) {
        return Err(RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("{context}: {err}"),
            data: Some(data),
        });
    }
    Ok(parsed)
}

async fn page_storage_call(
    browser: &Browser,
    params: Value,
    scope: &'static str,
    action: &'static str,
    method: &'static str,
) -> ToolResult {
    require_storage_state_capability(method)?;
    let storage_global = match scope {
        "local" => "localStorage",
        "session" => "sessionStorage",
        other => {
            return Err(RouterError::invalid_params(format!(
                "unknown storage scope: {other}"
            )));
        }
    };
    let key = params.get("key").and_then(Value::as_str);
    let expected = params.get("expected").and_then(Value::as_str);
    let page = locate_page(browser, &params)?;
    let js = match action {
        "get" => {
            if let Some(key) = key {
                let key_lit = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
                format!(
                    "(() => {{ try {{ return JSON.stringify({{value: {storage_global}.getItem({key_lit})}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
                )
            } else {
                format!(
                    "(() => {{ try {{ const o = {{}}; for (let i = 0; i < {storage_global}.length; i++) {{ const k = {storage_global}.key(i); o[k] = {storage_global}.getItem(k); }} return JSON.stringify({{value: o}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
                )
            }
        }
        "set" => {
            let key = required_str(&params, "key")?;
            let value = params
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| RouterError::invalid_params("value (string) required for set"))?;
            let key_lit = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
            let value_lit = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
            format!(
                "(() => {{ try {{ const previous = {storage_global}.getItem({key_lit}); {storage_global}.setItem({key_lit}, {value_lit}); return JSON.stringify({{ok: true, previous}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
            )
        }
        "delete" => {
            let key = required_str(&params, "key")?;
            let key_lit = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
            format!(
                "(() => {{ try {{ const previous = {storage_global}.getItem({key_lit}); {storage_global}.removeItem({key_lit}); return JSON.stringify({{ok: true, previous}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
            )
        }
        "clear" => {
            format!(
                "(() => {{ try {{ const size = {storage_global}.length; {storage_global}.clear(); return JSON.stringify({{ok: true, cleared_count: size}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
            )
        }
        "cas" => {
            let key = required_str(&params, "key")?;
            let value = params
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| RouterError::invalid_params("value (string) required for cas"))?;
            let key_lit = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
            let value_lit = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into());
            let expected_lit = match expected {
                Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "null".into()),
                None => "null".to_string(),
            };
            format!(
                "(() => {{ try {{ const current = {storage_global}.getItem({key_lit}); const expected = {expected_lit}; if (current !== expected) return JSON.stringify({{ok: false, matched: false, current}}); {storage_global}.setItem({key_lit}, {value_lit}); return JSON.stringify({{ok: true, matched: true, previous: current}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()",
            )
        }
        other => {
            return Err(RouterError::invalid_params(format!(
                "unknown action: {other:?} (want get|set|delete|clear|cas)"
            )));
        }
    };
    run_json_eval(
        &page,
        js,
        method,
        json!({"scope": scope, "action": action, "key": key}),
    )
    .await
}

pub(super) async fn page_cookies(browser: &Browser, params: Value) -> ToolResult {
    use browser_engine::cookies::{from_cdp_list, Cookie};
    let page = locate_page(browser, &params)?;
    let action = required_str(&params, "action")?;
    match action {
        "get" | "list" => {
            let res = page
                .cdp_call("Network.getCookies", None)
                .await
                .map_err(|e| RouterError::internal(format!("getCookies: {e}")))?;
            let arr = res
                .get("cookies")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let cookies = from_cdp_list(&arr);
            Ok(json!({"cookies": cookies}))
        }
        "set" => {
            let raw = params
                .get("cookies")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let cookies: Vec<Cookie> = raw
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
            let cdp_params: Vec<Value> = cookies.iter().map(Cookie::to_cdp_param).collect();
            page.cdp_call("Network.setCookies", Some(json!({"cookies": cdp_params})))
                .await
                .map_err(|e| RouterError::internal(format!("setCookies: {e}")))?;
            Ok(json!({"ok": true, "set_count": cookies.len()}))
        }
        "clear" => {
            let domain = params.get("domain").and_then(Value::as_str);
            let name = params.get("name").and_then(Value::as_str);
            // If domain or name was supplied we use Network.deleteCookies on
            // each matching cookie; otherwise a wholesale clear via
            // Network.clearBrowserCookies.
            if domain.is_some() || name.is_some() {
                let res = page
                    .cdp_call("Network.getCookies", None)
                    .await
                    .map_err(|e| RouterError::internal(format!("getCookies: {e}")))?;
                let arr = res
                    .get("cookies")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut count = 0u64;
                for v in arr {
                    let c = Cookie::from_cdp(&v);
                    if let Some(d) = domain {
                        if c.domain != d {
                            continue;
                        }
                    }
                    if let Some(n) = name {
                        if c.name != n {
                            continue;
                        }
                    }
                    let _ = page
                        .cdp_call(
                            "Network.deleteCookies",
                            Some(json!({
                                "name": c.name,
                                "domain": c.domain,
                                "path": c.path,
                            })),
                        )
                        .await;
                    count += 1;
                }
                Ok(json!({"ok": true, "cleared_count": count}))
            } else {
                page.cdp_call("Network.clearBrowserCookies", None)
                    .await
                    .map_err(|e| RouterError::internal(format!("clearBrowserCookies: {e}")))?;
                Ok(json!({"ok": true, "cleared_count": -1}))
            }
        }
        other => Err(RouterError::invalid_params(format!(
            "unknown action: {other}"
        ))),
    }
}

pub(super) async fn page_cookies_deep_set(browser: &Browser, params: Value) -> ToolResult {
    use browser_engine::cookies::DeepSetCookie;

    require_storage_state_capability("page.cookies.deep_set")?;
    let page = locate_page(browser, &params)?;
    let cookie_value = params
        .get("cookie")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("missing cookie"))?;
    let cookie: DeepSetCookie = serde_json::from_value(cookie_value)
        .map_err(|e| RouterError::invalid_params(format!("invalid cookie payload: {e}")))?;
    let res = page
        .cdp_call("Network.setCookie", Some(cookie.to_cdp_set_cookie_param()))
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.cookies.deep_set: {e}"),
            data: Some(json!({"name": cookie.name})),
        })?;
    if !res.get("success").and_then(Value::as_bool).unwrap_or(false) {
        return Err(RouterError {
            code: ErrorCode::ProtocolError,
            message: "page.cookies.deep_set: Network.setCookie returned success=false".into(),
            data: Some(json!({"name": cookie.name})),
        });
    }
    Ok(json!({"ok": true}))
}

pub(super) async fn page_localstorage_get(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "local", "get", "page.localstorage.get").await
}

pub(super) async fn page_localstorage_set(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "local", "set", "page.localstorage.set").await
}

pub(super) async fn page_localstorage_delete(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(
        browser,
        params,
        "local",
        "delete",
        "page.localstorage.delete",
    )
    .await
}

pub(super) async fn page_localstorage_clear(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "local", "clear", "page.localstorage.clear").await
}

pub(super) async fn page_localstorage_cas(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "local", "cas", "page.localstorage.cas").await
}

pub(super) async fn page_sessionstorage_get(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "session", "get", "page.sessionstorage.get").await
}

pub(super) async fn page_sessionstorage_set(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "session", "set", "page.sessionstorage.set").await
}

pub(super) async fn page_sessionstorage_delete(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(
        browser,
        params,
        "session",
        "delete",
        "page.sessionstorage.delete",
    )
    .await
}

pub(super) async fn page_sessionstorage_clear(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(
        browser,
        params,
        "session",
        "clear",
        "page.sessionstorage.clear",
    )
    .await
}

pub(super) async fn page_sessionstorage_cas(browser: &Browser, params: Value) -> ToolResult {
    page_storage_call(browser, params, "session", "cas", "page.sessionstorage.cas").await
}

/// Legacy umbrella wrapper kept for MCP compatibility. Canonical callers should
/// use `page.localstorage.*`, `page.sessionstorage.*`, and `page.indexeddb.*`.
pub(super) async fn page_storage(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.storage")?;
    if params.get("scope").is_some() {
        return Err(RouterError::invalid_params(
            "legacy page.storage {scope,...} shape is no longer accepted; use {kind, action, args} or page.{local,session}storage.*",
        ));
    }
    let kind = required_str(&params, "kind")?;
    let action = required_str(&params, "action")?;
    let mut delegated = match params.get("args").cloned().unwrap_or_else(|| json!({})) {
        Value::Object(map) => map,
        _ => {
            return Err(RouterError::invalid_params(
                "page.storage args must be an object when present",
            ));
        }
    };
    delegated.insert(
        "tab_id".into(),
        params
            .get("tab_id")
            .cloned()
            .ok_or_else(|| RouterError::invalid_params("missing tab_id"))?,
    );
    let delegated = Value::Object(delegated);
    match (kind, action) {
        ("local", "get") => page_localstorage_get(browser, delegated).await,
        ("local", "set") => page_localstorage_set(browser, delegated).await,
        ("local", "delete") => page_localstorage_delete(browser, delegated).await,
        ("local", "clear") => page_localstorage_clear(browser, delegated).await,
        ("local", "cas") => page_localstorage_cas(browser, delegated).await,
        ("session", "get") => page_sessionstorage_get(browser, delegated).await,
        ("session", "set") => page_sessionstorage_set(browser, delegated).await,
        ("session", "delete") => page_sessionstorage_delete(browser, delegated).await,
        ("session", "clear") => page_sessionstorage_clear(browser, delegated).await,
        ("session", "cas") => page_sessionstorage_cas(browser, delegated).await,
        ("indexeddb", "list_databases") => page_indexeddb_list_databases(browser, delegated).await,
        ("indexeddb", "list_stores") => page_indexeddb_list_stores(browser, delegated).await,
        ("indexeddb", "query") => page_indexeddb_query(browser, delegated).await,
        ("indexeddb", "put") => page_indexeddb_put(browser, delegated).await,
        ("indexeddb", "delete") => page_indexeddb_delete(browser, delegated).await,
        ("indexeddb", "delete_database") => {
            page_indexeddb_delete_database(browser, delegated).await
        }
        _ => Err(RouterError::invalid_params(format!(
            "unsupported page.storage combination: kind={kind:?} action={action:?}"
        ))),
    }
}

pub(super) async fn page_storage_quota(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.storage.quota")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let mut result = page
        .cdp_call(
            "Storage.getUsageAndQuota",
            Some(json!({"origin": origin.clone()})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.storage.quota: {e}"),
            data: Some(json!({"origin": origin})),
        })?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("origin".into(), json!(origin));
        Ok(result)
    } else {
        Ok(json!({"origin": origin, "result": result}))
    }
}

pub(super) async fn page_permissions_query(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.permissions.query")?;
    let page = locate_page(browser, &params)?;
    let current_origin = page_origin(&page)?;
    let origin = permission_origin_or_current(
        &current_origin,
        params.get("origin").and_then(Value::as_str),
        "origin",
    )?;
    let embedded_origin = match params.get("embedded_origin").and_then(Value::as_str) {
        Some(v) => Some(permission_origin_or_current(
            &origin,
            Some(v),
            "embedded_origin",
        )?),
        None => None,
    };
    if origin != current_origin
        || embedded_origin.as_deref().unwrap_or(&current_origin) != current_origin
    {
        return Err(RouterError::invalid_params(
            "page.permissions.query only supports the active document origin",
        ));
    }
    let permission = params
        .get("permission")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("missing permission descriptor"))?;
    if !permission.is_object() {
        return Err(RouterError::invalid_params(
            "permission descriptor must be an object",
        ));
    }
    let descriptor_json = serde_json::to_string(&permission).map_err(|e| {
        RouterError::invalid_params(format!("serialize permission descriptor: {e}"))
    })?;
    let expr = format!(
        "(async () => {{ try {{ const status = await navigator.permissions.query({descriptor_json}); return JSON.stringify({{state: status.state}}); }} catch (e) {{ return JSON.stringify({{error: String(e)}}); }} }})()"
    );
    run_json_eval(
        &page,
        expr,
        "page.permissions.query",
        json!({"origin": current_origin, "permission": permission}),
    )
    .await
}

pub(super) async fn page_permissions_grant(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.permissions.grant")?;
    let page = locate_page(browser, &params)?;
    let current_origin = page_origin(&page)?;
    let origin = permission_origin_or_current(
        &current_origin,
        params.get("origin").and_then(Value::as_str),
        "origin",
    )?;
    let embedded_origin = match params.get("embedded_origin").and_then(Value::as_str) {
        Some(v) => Some(permission_origin_or_current(
            &origin,
            Some(v),
            "embedded_origin",
        )?),
        None => None,
    };
    let permission = params
        .get("permission")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("missing permission descriptor"))?;
    if !permission.is_object() {
        return Err(RouterError::invalid_params(
            "permission descriptor must be an object",
        ));
    }
    browser
        .cdp()
        .root_session()
        .send_raw(
            "Browser.setPermission",
            strip_null_fields(json!({
                "permission": permission,
                "setting": "granted",
                "origin": origin,
                "embeddedOrigin": embedded_origin,
            })),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.permissions.grant: {e}"),
            data: Some(json!({"origin": current_origin})),
        })?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_permissions_revoke(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.permissions.revoke")?;
    let page = locate_page(browser, &params)?;
    let current_origin = page_origin(&page)?;
    let permission = params.get("permission").cloned();
    if let Some(permission) = permission {
        if !permission.is_object() {
            return Err(RouterError::invalid_params(
                "permission descriptor must be an object",
            ));
        }
        let origin = permission_origin_or_current(
            &current_origin,
            params.get("origin").and_then(Value::as_str),
            "origin",
        )?;
        let embedded_origin = match params.get("embedded_origin").and_then(Value::as_str) {
            Some(v) => Some(permission_origin_or_current(
                &origin,
                Some(v),
                "embedded_origin",
            )?),
            None => None,
        };
        browser
            .cdp()
            .root_session()
            .send_raw(
                "Browser.setPermission",
                strip_null_fields(json!({
                    "permission": permission,
                    "setting": "prompt",
                    "origin": origin,
                    "embeddedOrigin": embedded_origin,
                })),
            )
            .await
            .map_err(|e| RouterError {
                code: ErrorCode::ProtocolError,
                message: format!("page.permissions.revoke: {e}"),
                data: Some(json!({"origin": current_origin})),
            })?;
    } else {
        browser
            .cdp()
            .root_session()
            .send_raw("Browser.resetPermissions", Value::Null)
            .await
            .map_err(|e| RouterError {
                code: ErrorCode::ProtocolError,
                message: format!("page.permissions.revoke: {e}"),
                data: Some(json!({"origin": current_origin})),
            })?;
    }
    Ok(json!({"ok": true}))
}

pub(super) async fn page_indexeddb_list_databases(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.list_databases")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let result = page
        .cdp_call(
            "IndexedDB.requestDatabaseNames",
            Some(json!({"securityOrigin": origin.clone()})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.indexeddb.list_databases: {e}"),
            data: Some(json!({"origin": origin})),
        })?;
    Ok(
        json!({"origin": origin, "database_names": result.get("databaseNames").cloned().unwrap_or(Value::Array(Vec::new()))}),
    )
}

pub(super) async fn page_indexeddb_list_stores(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.list_stores")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let database_name = required_str(&params, "database_name")?;
    let result = page
        .cdp_call(
            "IndexedDB.requestDatabase",
            Some(json!({"securityOrigin": origin.clone(), "databaseName": database_name})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.indexeddb.list_stores: {e}"),
            data: Some(json!({"origin": origin, "database_name": database_name})),
        })?;
    Ok(result)
}

pub(super) async fn page_indexeddb_query(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.query")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let database_name = required_str(&params, "database_name")?;
    let object_store_name = required_str(&params, "object_store_name")?;
    let result = page
        .cdp_call(
            "IndexedDB.requestData",
            Some(strip_null_fields(json!({
                "securityOrigin": origin.clone(),
                "databaseName": database_name,
                "objectStoreName": object_store_name,
                "indexName": params.get("index_name").cloned(),
                "skipCount": params.get("skip_count").and_then(Value::as_u64).unwrap_or(0),
                "pageSize": params.get("page_size").and_then(Value::as_u64).unwrap_or(100),
                "keyRange": params.get("key_range").cloned(),
            }))),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.indexeddb.query: {e}"),
            data: Some(json!({
                "origin": origin,
                "database_name": database_name,
                "object_store_name": object_store_name,
            })),
        })?;
    Ok(json!({
        "entries": result.get("objectStoreDataEntries").cloned().unwrap_or(Value::Array(Vec::new())),
        "has_more": result.get("hasMore").cloned().unwrap_or(Value::Bool(false)),
    }))
}

pub(super) async fn page_indexeddb_put(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.put")?;
    let page = locate_page(browser, &params)?;
    let database_name = required_str(&params, "database_name")?;
    let object_store_name = required_str(&params, "object_store_name")?;
    let key_json = serde_json::to_string(
        params
            .get("key")
            .ok_or_else(|| RouterError::invalid_params("missing key"))?,
    )
    .map_err(|e| RouterError::invalid_params(format!("serialize key: {e}")))?;
    let value_json = serde_json::to_string(
        params
            .get("value")
            .ok_or_else(|| RouterError::invalid_params("missing value"))?,
    )
    .map_err(|e| RouterError::invalid_params(format!("serialize value: {e}")))?;
    let version_json = match params.get("database_version").and_then(Value::as_u64) {
        Some(v) => v.to_string(),
        None => "undefined".to_string(),
    };
    let db_name = serde_json::to_string(database_name).unwrap_or_else(|_| "\"\"".into());
    let store_name = serde_json::to_string(object_store_name).unwrap_or_else(|_| "\"\"".into());
    let expr = format!(
        r#"(async () => {{
  try {{
    const dbName = {db_name};
    const storeName = {store_name};
    const key = {key_json};
    const value = {value_json};
    const version = {version_json};
    await new Promise((resolve, reject) => {{
      const req = version === undefined ? indexedDB.open(dbName) : indexedDB.open(dbName, version);
      req.onupgradeneeded = () => {{
        const db = req.result;
        if (!db.objectStoreNames.contains(storeName)) db.createObjectStore(storeName);
      }};
      req.onerror = () => reject(req.error || new Error('indexedDB.open failed'));
      req.onsuccess = () => {{
        const db = req.result;
        let tx;
        try {{
          tx = db.transaction([storeName], 'readwrite');
        }} catch (e) {{
          db.close();
          reject(e);
          return;
        }}
        tx.onerror = () => reject(tx.error || new Error('indexedDB transaction failed'));
        tx.oncomplete = () => {{ db.close(); resolve(undefined); }};
        tx.objectStore(storeName).put(value, key);
      }};
    }});
    return JSON.stringify({{ok: true}});
  }} catch (e) {{
    return JSON.stringify({{error: String(e)}});
  }}
}})()"#
    );
    run_json_eval(
        &page,
        expr,
        "page.indexeddb.put",
        json!({"database_name": database_name, "object_store_name": object_store_name}),
    )
    .await
}

pub(super) async fn page_indexeddb_delete(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.delete")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let database_name = required_str(&params, "database_name")?;
    let object_store_name = required_str(&params, "object_store_name")?;
    let key_range = params
        .get("key_range")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("missing key_range"))?;
    page.cdp_call(
        "IndexedDB.deleteObjectStoreEntries",
        Some(json!({
            "securityOrigin": origin.clone(),
            "databaseName": database_name,
            "objectStoreName": object_store_name,
            "keyRange": key_range,
        })),
    )
    .await
    .map_err(|e| RouterError {
        code: ErrorCode::ProtocolError,
        message: format!("page.indexeddb.delete: {e}"),
        data: Some(json!({"origin": origin, "database_name": database_name, "object_store_name": object_store_name})),
    })?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_indexeddb_delete_database(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.indexeddb.delete_database")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let database_name = required_str(&params, "database_name")?;
    page.cdp_call(
        "IndexedDB.deleteDatabase",
        Some(json!({"securityOrigin": origin.clone(), "databaseName": database_name})),
    )
    .await
    .map_err(|e| RouterError {
        code: ErrorCode::ProtocolError,
        message: format!("page.indexeddb.delete_database: {e}"),
        data: Some(json!({"origin": origin, "database_name": database_name})),
    })?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_cache_api_list(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.cache_api.list")?;
    let page = locate_page(browser, &params)?;
    let origin = page_origin(&page)?;
    let result = page
        .cdp_call(
            "CacheStorage.requestCacheNames",
            Some(json!({"securityOrigin": origin.clone()})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.cache_api.list: {e}"),
            data: Some(json!({"origin": origin})),
        })?;
    Ok(
        json!({"origin": origin, "caches": result.get("caches").cloned().unwrap_or(Value::Array(Vec::new()))}),
    )
}

pub(super) async fn page_cache_api_inspect(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.cache_api.inspect")?;
    let page = locate_page(browser, &params)?;
    let cache_id = required_str(&params, "cache_id")?;
    if let Some(request_url) = params.get("request_url").and_then(Value::as_str) {
        let request_headers = params
            .get("request_headers")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let result = page
            .cdp_call(
                "CacheStorage.requestCachedResponse",
                Some(json!({
                    "cacheId": cache_id,
                    "requestURL": request_url,
                    "requestHeaders": request_headers,
                })),
            )
            .await
            .map_err(|e| RouterError {
                code: ErrorCode::ProtocolError,
                message: format!("page.cache_api.inspect: {e}"),
                data: Some(json!({"cache_id": cache_id, "request_url": request_url})),
            })?;
        Ok(result)
    } else {
        let result = page
            .cdp_call(
                "CacheStorage.requestEntries",
                Some(strip_null_fields(json!({
                    "cacheId": cache_id,
                    "skipCount": params.get("skip_count").and_then(Value::as_u64),
                    "pageSize": params.get("page_size").and_then(Value::as_u64),
                    "pathFilter": params.get("path_filter").and_then(Value::as_str),
                }))),
            )
            .await
            .map_err(|e| RouterError {
                code: ErrorCode::ProtocolError,
                message: format!("page.cache_api.inspect: {e}"),
                data: Some(json!({"cache_id": cache_id})),
            })?;
        Ok(json!({
            "entries": result.get("cacheDataEntries").cloned().unwrap_or(Value::Array(Vec::new())),
            "return_count": result.get("returnCount").cloned().unwrap_or(Value::Null),
        }))
    }
}

pub(super) async fn page_cache_api_delete(browser: &Browser, params: Value) -> ToolResult {
    require_storage_state_capability("page.cache_api.delete")?;
    let page = locate_page(browser, &params)?;
    let cache_id = required_str(&params, "cache_id")?;
    if let Some(request_url) = params.get("request_url").and_then(Value::as_str) {
        page.cdp_call(
            "CacheStorage.deleteEntry",
            Some(json!({"cacheId": cache_id, "request": request_url})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.cache_api.delete: {e}"),
            data: Some(json!({"cache_id": cache_id, "request_url": request_url})),
        })?;
    } else {
        page.cdp_call(
            "CacheStorage.deleteCache",
            Some(json!({"cacheId": cache_id})),
        )
        .await
        .map_err(|e| RouterError {
            code: ErrorCode::ProtocolError,
            message: format!("page.cache_api.delete: {e}"),
            data: Some(json!({"cache_id": cache_id})),
        })?;
    }
    Ok(json!({"ok": true}))
}

pub(super) async fn page_viewport(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let width = params.get("width").and_then(Value::as_u64).unwrap_or(1280) as u32;
    let height = params.get("height").and_then(Value::as_u64).unwrap_or(800) as u32;
    let dsf = params
        .get("device_scale_factor")
        .and_then(Value::as_f64)
        .unwrap_or(2.0);
    let mobile = params
        .get("mobile")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    page.set_viewport(width, height, dsf, mobile)
        .await
        .map_err(|e| RouterError::internal(format!("viewport: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_user_agent(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let ua = required_str(&params, "user_agent")?;
    let al = params.get("accept_language").and_then(Value::as_str);
    let plat = params.get("platform").and_then(Value::as_str);
    page.set_user_agent(ua, al, plat)
        .await
        .map_err(|e| RouterError::internal(format!("user_agent: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_geo(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let lat = params
        .get("latitude")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let lon = params
        .get("longitude")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let acc = params
        .get("accuracy")
        .and_then(Value::as_f64)
        .unwrap_or(50.0);
    page.set_geo(lat, lon, acc)
        .await
        .map_err(|e| RouterError::internal(format!("geo: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_dark_mode(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    page.set_dark_mode(enabled)
        .await
        .map_err(|e| RouterError::internal(format!("dark_mode: {e}")))?;
    Ok(json!({"ok": true}))
}

/// SPEC §10 M7 — wraps `Network.emulateNetworkConditions`.
pub(super) async fn page_network_conditions(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let conditions = browser_engine::emulate::parse_network_conditions(&params);
    page.set_network_conditions(&conditions)
        .await
        .map_err(|e| RouterError::internal(format!("network_conditions: {e}")))?;
    Ok(json!({"ok": true}))
}

/// SPEC §10 M8 — wraps `Emulation.setLocaleOverride / setTimezoneOverride / setCPUThrottlingRate`.
pub(super) async fn page_emulate(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let opts = browser_engine::emulate::parse_emulate_options(&params);
    page.emulate(&opts)
        .await
        .map_err(|e| RouterError::internal(format!("emulate: {e}")))?;
    Ok(json!({"ok": true}))
}

// ---------- SPEC §12 U4 — perf + introspection ----------

/// Compute the per-session "perf artifacts" dir under the broker's
/// `user_data_root`, falling back to the OS temp dir if the broker
/// state isn't reachable from this thread.
fn session_perf_dir(session: &SessionEntry) -> std::path::PathBuf {
    if let Some(state) = super::current_state() {
        return state.user_data_root.join(&session.session_id).join("perf");
    }
    std::env::temp_dir()
        .join("one-for-all")
        .join(&session.session_id)
        .join("perf")
}

pub(super) async fn page_performance_timeline_start(
    browser: &Browser,
    params: Value,
) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let cats = params.get("categories").and_then(Value::as_str);
    browser_engine::perf::performance_timeline_start(&page, cats)
        .await
        .map_err(|e| RouterError::internal(format!("performance_timeline_start: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_performance_timeline_stop(
    session: &SessionEntry,
    browser: &Browser,
    params: Value,
) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let dir = session_perf_dir(session);
    let res = browser_engine::perf::performance_timeline_stop(&page, &dir)
        .await
        .map_err(|e| RouterError::internal(format!("performance_timeline_stop: {e}")))?;
    Ok(serde_json::to_value(res).unwrap_or(Value::Null))
}

pub(super) async fn page_performance_metrics(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    browser_engine::perf::performance_metrics(&page)
        .await
        .map_err(|e| RouterError::internal(format!("performance_metrics: {e}")))
}

pub(super) async fn page_coverage_js_start(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let call_count = params.get("call_count").and_then(Value::as_bool);
    let detailed = params.get("detailed").and_then(Value::as_bool);
    browser_engine::perf::coverage_js_start(&page, call_count, detailed)
        .await
        .map_err(|e| RouterError::internal(format!("coverage_js_start: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_coverage_js_take(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    browser_engine::perf::coverage_js_take(&page)
        .await
        .map_err(|e| RouterError::internal(format!("coverage_js_take: {e}")))
}

pub(super) async fn page_coverage_css_start(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    browser_engine::perf::coverage_css_start(&page)
        .await
        .map_err(|e| RouterError::internal(format!("coverage_css_start: {e}")))?;
    Ok(json!({"ok": true}))
}

pub(super) async fn page_coverage_css_take(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    browser_engine::perf::coverage_css_take(&page)
        .await
        .map_err(|e| RouterError::internal(format!("coverage_css_take: {e}")))
}

pub(super) async fn page_heap_snapshot(
    session: &SessionEntry,
    browser: &Browser,
    params: Value,
) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let dir = session_perf_dir(session);
    let res = browser_engine::perf::heap_snapshot(&page, &dir)
        .await
        .map_err(|e| RouterError::internal(format!("heap_snapshot: {e}")))?;
    Ok(serde_json::to_value(res).unwrap_or(Value::Null))
}

pub(super) async fn page_heap_sample_alloc(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("missing duration_ms"))?;
    let interval = params
        .get("sampling_interval_bytes")
        .and_then(Value::as_u64);
    browser_engine::perf::heap_sample_alloc(&page, duration_ms, interval)
        .await
        .map_err(|e| RouterError::internal(format!("heap_sample_alloc: {e}")))
}

pub(super) async fn page_cpu_profile(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("missing duration_ms"))?;
    browser_engine::perf::cpu_profile(&page, duration_ms)
        .await
        .map_err(|e| RouterError::internal(format!("cpu_profile: {e}")))
}

pub(super) async fn page_layout_metrics(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    browser_engine::perf::layout_metrics(&page)
        .await
        .map_err(|e| RouterError::internal(format!("layout_metrics: {e}")))
}

pub(super) async fn page_paint_flash(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let enable = params
        .get("enable")
        .and_then(Value::as_bool)
        .ok_or_else(|| RouterError::invalid_params("missing enable"))?;
    browser_engine::perf::paint_flash(&page, enable)
        .await
        .map_err(|e| RouterError::internal(format!("paint_flash: {e}")))?;
    Ok(json!({"ok": true}))
}

// ---------- SPEC §12 U5 — print + PDF ----------

pub(super) async fn page_pdf(
    session: &SessionEntry,
    browser: &Browser,
    params: Value,
) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let dir = session_perf_dir(session);
    let mut opts_value = params.clone();
    if let Some(obj) = opts_value.as_object_mut() {
        obj.remove("tab_id");
    }
    let options: browser_engine::pdf::PdfOptions = serde_json::from_value(opts_value)
        .map_err(|e| RouterError::invalid_params(format!("pdf options: {e}")))?;
    let res = browser_engine::pdf::pdf(&page, options, &dir)
        .await
        .map_err(|e| RouterError::internal(format!("pdf: {e}")))?;
    Ok(serde_json::to_value(res).unwrap_or(Value::Null))
}

pub(super) async fn page_print_preview(browser: &Browser, params: Value) -> ToolResult {
    let page = locate_page(browser, &params)?;
    let format = params
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("png")
        .to_owned();
    let cbv = params
        .get("capture_beyond_viewport")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    browser_engine::pdf::print_preview(&page, &format, cbv)
        .await
        .map_err(|e| RouterError::internal(format!("print_preview: {e}")))
}

// ---------- vision.* (SPEC §11 V4) ----------

pub(super) async fn vision_read_text(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"));
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let regions = pipeline
        .read_text(region)
        .await
        .map_err(|e| RouterError::internal(format!("vision.read_text: {e}")))?;
    Ok(json!({ "regions": regions }))
}

pub(super) async fn vision_find_text(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("query required"))?
        .to_owned();
    let is_regex = params
        .get("is_regex")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let region = parse_region(params.get("region"));
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let matches = pipeline
        .find_text(vision::TextQuery {
            query,
            is_regex,
            region,
        })
        .await
        .map_err(|e| RouterError::internal(format!("vision.find_text: {e}")))?;
    Ok(json!({ "matches": matches }))
}

pub(super) async fn vision_compare(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let ref_path = params
        .get("ref_image_path")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("ref_image_path required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let score = pipeline
        .compare(std::path::Path::new(ref_path))
        .await
        .map_err(|e| RouterError::internal(format!("vision.compare: {e}")))?;
    Ok(json!({ "similarity": score }))
}

pub(super) async fn vision_fps(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let fps = params
        .get("fps")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, 60) as u32)
        .ok_or_else(|| RouterError::invalid_params("fps required (1..=60)"))?;
    let idle_fps = params
        .get("idle_fps")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, fps as u64) as u32)
        .unwrap_or_else(|| 5.min(fps));
    if !matches!(
        session.vision_config.read().mode,
        vision::VisionMode::Continuous
    ) {
        return Err(RouterError::invalid_params(
            "vision.fps requires vision=continuous for this session",
        ));
    }
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    pipeline
        .set_fps(fps, idle_fps)
        .map_err(|e| RouterError::invalid_params(format!("vision.fps: {e}")))?;
    Ok(json!({ "ok": true, "fps": fps, "idle_fps": idle_fps }))
}

// ---------- SPEC §11 V4 deeper hooks ----------

pub(super) async fn vision_stability(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    Ok(json!(pipeline.stability_now()))
}

pub(super) async fn vision_changed_since(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let since_us = params
        .get("since_us")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("since_us (u64) required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    Ok(json!({ "changed_tiles": pipeline.changed_since(since_us) }))
}

pub(super) async fn vision_verify_action(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let action_val = params
        .get("action")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("action required"))?;
    let action: vision::ActionContext = serde_json::from_value(action_val)
        .map_err(|e| RouterError::invalid_params(format!("action: {e}")))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let verdict = pipeline
        .pre_action_verify(action)
        .await
        .map_err(|e| RouterError::internal(format!("vision.verify_action: {e}")))?;
    Ok(json!(verdict))
}

// ---------- SPEC §12 U10 sub-granularity surface ----------

pub(super) async fn vision_pixel(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let x = params
        .get("x")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("x required"))? as u32;
    let y = params
        .get("y")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("y required"))? as u32;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let px = vision::pixel::pixel_at(&pipeline, x, y)
        .map_err(|e| RouterError::internal(format!("vision.pixel: {e}")))?;
    Ok(json!(px))
}

pub(super) async fn vision_region_classify(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"))
        .ok_or_else(|| RouterError::invalid_params("region required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, Some(region)).await?;
    let r = vision::region_classify::classify(&pipeline, region)
        .await
        .map_err(|e| RouterError::internal(format!("vision.region.classify: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_color_palette(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"));
    let k = params
        .get("k")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("k required (1..=16)"))? as u32;
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let r = vision::palette::palette(&pipeline, region, k)
        .map_err(|e| RouterError::invalid_params(format!("vision.color.palette: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_text_style(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"))
        .ok_or_else(|| RouterError::invalid_params("region required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, Some(region)).await?;
    let r = vision::text_style::text_style(&pipeline, region)
        .await
        .map_err(|e| RouterError::internal(format!("vision.text.style: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_layout_segments(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let r = vision::layout::segments(&pipeline)
        .map_err(|e| RouterError::internal(format!("vision.layout.segments: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_icon_recognize(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"))
        .ok_or_else(|| RouterError::invalid_params("region required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, Some(region)).await?;
    let r = vision::icon::recognize(&pipeline, region)
        .map_err(|e| RouterError::internal(format!("vision.icon.recognize: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_qr_barcode(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"));
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let r = vision::barcode::scan(&pipeline, region)
        .map_err(|e| RouterError::internal(format!("vision.qr_barcode: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_scrollbar_position(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let region = parse_region(params.get("region"));
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let r = vision::scrollbar::scrollbar_position(&pipeline, region)
        .map_err(|e| RouterError::internal(format!("vision.scrollbar.position: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_loading_detect(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    Ok(json!(vision::loading::detect(&pipeline)))
}

pub(super) async fn vision_tooltip_detect(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let r = vision::overlay::tooltip(&pipeline)
        .map_err(|e| RouterError::internal(format!("vision.tooltip.detect: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_modal_detect(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let r = vision::overlay::modal(&pipeline)
        .map_err(|e| RouterError::internal(format!("vision.modal.detect: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_diff_semantic(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let prev_seq = params
        .get("prev")
        .and_then(Value::as_object)
        .and_then(|prev| prev.get("seq"))
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("prev.seq required"))?;
    let next_seq = params
        .get("next")
        .and_then(Value::as_object)
        .and_then(|next| next.get("seq"))
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("next.seq required"))?;
    let action_val = params
        .get("action_context")
        .cloned()
        .ok_or_else(|| RouterError::invalid_params("action_context required"))?;
    let action: vision::ActionContext = serde_json::from_value(action_val)
        .map_err(|e| RouterError::invalid_params(format!("action_context: {e}")))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let prev_frame = pipeline
        .decoded_frame_by_seq(prev_seq)
        .ok_or_else(|| RouterError {
            code: ErrorCode::ElementStale,
            message: format!(
                "vision.diff.semantic: prev seq {prev_seq} is no longer retained for tab {tab_id}"
            ),
            data: Some(json!({ "seq": prev_seq, "which": "prev" })),
        })?;
    let next_frame = pipeline
        .decoded_frame_by_seq(next_seq)
        .ok_or_else(|| RouterError {
            code: ErrorCode::ElementStale,
            message: format!(
                "vision.diff.semantic: next seq {next_seq} is no longer retained for tab {tab_id}"
            ),
            data: Some(json!({ "seq": next_seq, "which": "next" })),
        })?;
    let r = vision::semantic_diff::semantic_diff(
        &pipeline, prev_seq, prev_frame, next_seq, next_frame, action,
    )
    .await
    .map_err(|e| RouterError::internal(format!("vision.diff.semantic: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_animation_frames(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    let duration_ms = params
        .get("duration_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("duration_ms required"))?
        as u32;
    let pipeline = require_or_lazy_pipeline(session, tab_id, None).await?;
    let r = vision::animation::animation_frames(&pipeline, duration_ms)
        .map_err(|e| RouterError::invalid_params(format!("vision.animation.frames: {e}")))?;
    Ok(json!(r))
}

pub(super) async fn vision_face_blur(session: &SessionEntry, params: Value) -> ToolResult {
    let tab_id = params
        .get("tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("tab_id required"))?;
    if !session.has_capability("face_detect") {
        return Err(RouterError {
            code: ErrorCode::InvalidParams,
            message: "session lacks 'face_detect' capability — pass capabilities: [\"face_detect\"] to session.register".into(),
            data: None,
        });
    }
    let region = parse_region(params.get("region"));
    let output = params
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| RouterError::invalid_params("output (path) required"))?;
    let pipeline = require_or_lazy_pipeline(session, tab_id, region).await?;
    let r = vision::face_blur::face_blur(&pipeline, region, std::path::Path::new(output))
        .map_err(|e| RouterError::internal(format!("vision.face_blur: {e}")))?;
    Ok(json!(r))
}

fn parse_region(v: Option<&Value>) -> Option<vision::Bbox> {
    let v = v?;
    let x = v.get("x").and_then(Value::as_u64)? as u32;
    let y = v.get("y").and_then(Value::as_u64)? as u32;
    let w = v.get("w").and_then(Value::as_u64)? as u32;
    let h = v.get("h").and_then(Value::as_u64)? as u32;
    Some(vision::Bbox { x, y, w, h })
}

fn vision_session_trace_parts(
    session: &SessionEntry,
    target_id: &str,
) -> (
    Option<Arc<dyn observability::trace::TraceSink>>,
    Option<String>,
    Option<String>,
) {
    let trace_sink = session.browser.load().trace_sink();
    let trace_target = trace_sink.as_ref().map(|_| target_id.to_owned());
    let trace_session = trace_sink.as_ref().map(|_| session.session_id.clone());
    (trace_sink, trace_target, trace_session)
}

fn build_raw_frame(
    encoded: Arc<Vec<u8>>,
    format: vision::FrameFormat,
    width: u32,
    height: u32,
    page_scale_factor: f64,
) -> vision::ScreencastFrame {
    let captured_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    vision::ScreencastFrame {
        bytes: encoded,
        format,
        metadata: vision::ScreencastFrameMetadata {
            offset_top: 0.0,
            page_scale_factor,
            device_width: width as f64,
            device_height: height as f64,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
            timestamp: 0.0,
        },
        session_id: String::new(),
        captured_us,
    }
}

async fn prime_browser_pipeline(
    session: &SessionEntry,
    tab_id: &str,
    page: &Arc<Page>,
    pipeline: &Arc<vision::VisionPipeline>,
    region: Option<vision::Bbox>,
) -> Result<(), RouterError> {
    let clip = region.map(|bbox| browser_engine::snapshot::BBox {
        x: bbox.x as f64,
        y: bbox.y as f64,
        w: bbox.w as f64,
        h: bbox.h as f64,
    });
    let png_b64 = page
        .screenshot("png", None, false, clip.as_ref())
        .await
        .map_err(|e| RouterError::internal(format!("vision prime screenshot: {e}")))?;
    let encoded = base64::engine::general_purpose::STANDARD
        .decode(png_b64.as_bytes())
        .map_err(|e| RouterError::internal(format!("vision prime decode: {e}")))?;
    let encoded = Arc::new(encoded);
    let (width, height, page_scale_factor) = match clip.as_ref() {
        Some(clip) => (clip.w.round() as u32, clip.h.round() as u32, 1.0),
        None => {
            let metrics = browser_engine::perf::layout_metrics(page)
                .await
                .map_err(|e| RouterError::internal(format!("vision prime layout_metrics: {e}")))?;
            let viewport = metrics
                .get("css_visual_viewport")
                .or_else(|| metrics.get("visual_viewport"))
                .ok_or_else(|| {
                    RouterError::internal(
                        "vision prime layout_metrics missing visual viewport".to_string(),
                    )
                })?;
            let width = viewport
                .get("clientWidth")
                .or_else(|| viewport.get("client_width"))
                .or_else(|| viewport.get("width"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .round() as u32;
            let height = viewport
                .get("clientHeight")
                .or_else(|| viewport.get("client_height"))
                .or_else(|| viewport.get("height"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .round() as u32;
            let scale = viewport
                .get("scale")
                .and_then(Value::as_f64)
                .filter(|s| *s > 0.0)
                .unwrap_or(1.0);
            (width, height, scale)
        }
    };
    let raw = build_raw_frame(
        encoded,
        vision::FrameFormat::Png,
        width,
        height,
        page_scale_factor,
    );
    let _ = vision_session_trace_parts(session, tab_id);
    pipeline
        .ingest_raw_frame(raw)
        .await
        .map_err(|e| RouterError::internal(format!("vision prime ingest: {e}")))?;
    Ok(())
}

async fn ensure_browser_pipeline(
    session: &SessionEntry,
    tab_id: &str,
    region: Option<vision::Bbox>,
) -> Result<Arc<vision::VisionPipeline>, RouterError> {
    let mode = session.vision_config.read().mode;
    if matches!(mode, vision::VisionMode::Off) {
        return Err(RouterError {
            code: ErrorCode::InvalidParams,
            message: "vision is off for this session — set vision=on_demand or continuous in browser.context.create".into(),
            data: None,
        });
    }
    let page = session
        .browser
        .load()
        .default_context()
        .get(&browser_engine::TabId(tab_id.to_owned()))
        .ok_or_else(RouterError::tab_not_found)?;
    let pipeline = if let Some(p) = session.vision_pipelines.get(tab_id) {
        Arc::clone(p.value())
    } else {
        let cfg_vlm = session.vision_config.read().vlm.clone();
        let p = vision::VisionPipeline::new(
            session.session_id.clone(),
            tab_id.to_owned(),
            session.vision_metrics.clone(),
            cfg_vlm,
        )
        .map_err(|e| RouterError::internal(format!("vision pipeline init: {e}")))?;
        let arc = Arc::new(p);
        session
            .vision_pipelines
            .insert(tab_id.to_owned(), Arc::clone(&arc));
        arc
    };
    if pipeline.last_decoded().is_none() {
        prime_browser_pipeline(session, tab_id, &page, &pipeline, region).await?;
    }
    Ok(pipeline)
}

async fn require_or_lazy_pipeline(
    session: &SessionEntry,
    tab_id: &str,
    region: Option<vision::Bbox>,
) -> Result<Arc<vision::VisionPipeline>, RouterError> {
    ensure_browser_pipeline(session, tab_id, region).await
}
