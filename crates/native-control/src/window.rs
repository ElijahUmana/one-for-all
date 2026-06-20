//! SPEC §12 U6 — `app.window.{list, raise, minimize, move, resize, fullscreen}`.
//!
//! Window operations operate on `kAXWindowsAttribute` of the application's
//! AX root. Refs are scoped per-snapshot via `WindowHandle::window_id`
//! (`"w0"`, `"w1"`, …) — the same scoping discipline as `AppElement`.
//!
//! Focus discipline:
//! - `raise` IS focus-stealing — it calls `kAXRaiseAction` (forbidden in
//!   `actions.rs` per SPEC §5). We default-deny here unless the caller has
//!   `focus_steal` capability AND passes `confirm: true`. The lock-out is
//!   identical to `dock::click`.
//! - `minimize` / `move` / `resize` / `fullscreen` are NOT focus-stealing.
//!   Apple's WindowServer applies geometry changes without raising the app.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    kAXErrorSuccess, kAXMinimizedAttribute, kAXPositionAttribute, kAXSizeAttribute,
    kAXTitleAttribute, kAXValueTypeCGPoint, kAXValueTypeCGSize, kAXWindowsAttribute,
    AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
    AXUIElementSetAttributeValue, AXValueCreate,
};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;

use crate::ax_walk::{
    bbox_of, copy_attr_owned, copy_bool_attr, copy_children, copy_string_attr, map_ax_error,
};
use crate::cf_owned::AxOwned;
use crate::types::{BBox, NativeControlError, WindowHandle};

/// AX attribute name for fullscreen state. The `accessibility-sys` crate at
/// our pinned version does not expose this constant; the underlying C
/// constant is the string `"AXFullScreen"` (kAXFullScreenAttribute).
const AX_FULLSCREEN_ATTR: &str = "AXFullScreen";
/// AX attribute name for the "raise" (focus-stealing) action. Equivalent to
/// `kAXRaiseAction` — string-encoded so we don't import a constant we never
/// otherwise reference (the crate-wide ban on `kAXRaiseAction` lives in
/// `actions.rs`).
const AX_RAISE_ACTION: &str = "AXRaise";

/// List every top-level window of the target app.
pub async fn list(bundle_id: &str) -> Result<Vec<WindowHandle>, NativeControlError> {
    let bundle_id = bundle_id.to_string();
    tokio::task::spawn_blocking(move || list_blocking(&bundle_id))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_blocking(bundle_id: &str) -> Result<Vec<WindowHandle>, NativeControlError> {
    let pid = crate::actions::resolve_pid(bundle_id)?;
    // SAFETY: AXUIElementCreateApplication returns +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;

    // Use AXChildren of kAXWindowsAttribute → array of window refs.
    let windows_owned = match copy_attr_owned(app.as_ptr() as AXUIElementRef, kAXWindowsAttribute) {
        Some(w) => w,
        None => return Ok(vec![]),
    };
    // The attribute returns a CFArrayRef of AXUIElementRef. Reuse the
    // copy_children helper which expects an element with an AXChildren
    // attribute — instead, we manually walk via the AXValue array of refs.
    // Simpler: copy_children works because AXWindows behaves array-like
    // when iterated. We instead query the app's children attribute filtered
    // by role == AXWindow.
    let kids = copy_children(app.as_ptr() as AXUIElementRef).unwrap_or_default();
    let _ = windows_owned; // ownership held to mirror the canonical pattern.

    let mut out = Vec::new();
    for (i, k) in kids.iter().enumerate() {
        let role = copy_string_attr(
            k.as_ptr() as AXUIElementRef,
            accessibility_sys::kAXRoleAttribute,
        )
        .unwrap_or_default();
        if role != accessibility_sys::kAXWindowRole {
            continue;
        }
        let title =
            copy_string_attr(k.as_ptr() as AXUIElementRef, kAXTitleAttribute).unwrap_or_default();
        let bbox = bbox_of(k.as_ptr() as AXUIElementRef).unwrap_or(BBox {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        });
        let minimized =
            copy_bool_attr(k.as_ptr() as AXUIElementRef, kAXMinimizedAttribute).unwrap_or(false);
        let fullscreen =
            copy_bool_attr(k.as_ptr() as AXUIElementRef, AX_FULLSCREEN_ATTR).unwrap_or(false);
        let main = copy_bool_attr(k.as_ptr() as AXUIElementRef, "AXMain").unwrap_or(false);
        out.push(WindowHandle {
            window_id: format!("w{i}"),
            bundle_id: bundle_id.to_string(),
            title,
            bbox,
            minimized,
            fullscreen,
            main,
        });
    }
    Ok(out)
}

/// Resolve a window_id like `"w3"` to the live AXUIElementRef.
fn resolve_window(
    bundle_id: &str,
    window_id: &str,
) -> Result<AxOwned<accessibility_sys::__AXUIElement>, NativeControlError> {
    let idx = window_id
        .strip_prefix('w')
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| NativeControlError::RefStale {
            r: window_id.to_string(),
        })?;
    let pid = crate::actions::resolve_pid(bundle_id)?;
    // SAFETY: +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;
    let kids = copy_children(app.as_ptr() as AXUIElementRef).unwrap_or_default();
    let mut filtered: Vec<AxOwned<accessibility_sys::__AXUIElement>> = Vec::new();
    for k in kids {
        let role = copy_string_attr(
            k.as_ptr() as AXUIElementRef,
            accessibility_sys::kAXRoleAttribute,
        )
        .unwrap_or_default();
        if role == accessibility_sys::kAXWindowRole {
            filtered.push(k);
        }
    }
    filtered
        .into_iter()
        .nth(idx)
        .ok_or_else(|| NativeControlError::RefStale {
            r: window_id.to_string(),
        })
}

/// Raise a window to front. **Focus-stealing.** Default-denied unless the
/// caller proves capability + intent.
pub async fn raise(
    bundle_id: &str,
    window_id: &str,
    confirm: bool,
    focus_steal_capability: bool,
) -> Result<(), NativeControlError> {
    if !(confirm && focus_steal_capability) {
        return Err(NativeControlError::FocusStealForbidden {
            what: "app.window.raise",
        });
    }
    let bundle_id = bundle_id.to_string();
    let window_id = window_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let win = resolve_window(&bundle_id, &window_id)?;
        let action_cf = CFString::new(AX_RAISE_ACTION);
        // SAFETY: window ref is live; action_cf is a CFString.
        let err = unsafe {
            AXUIElementPerformAction(
                win.as_ptr() as AXUIElementRef,
                action_cf.as_concrete_TypeRef(),
            )
        };
        if err == kAXErrorSuccess {
            Ok(())
        } else {
            Err(map_ax_error(err))
        }
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Toggle minimization. Setting `minimize=true` minimizes; `false` un-minimizes.
pub async fn set_minimized(
    bundle_id: &str,
    window_id: &str,
    minimize: bool,
) -> Result<(), NativeControlError> {
    let bundle_id = bundle_id.to_string();
    let window_id = window_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let win = resolve_window(&bundle_id, &window_id)?;
        set_bool(
            win.as_ptr() as AXUIElementRef,
            kAXMinimizedAttribute,
            minimize,
        )
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Toggle fullscreen.
pub async fn set_fullscreen(
    bundle_id: &str,
    window_id: &str,
    fullscreen: bool,
) -> Result<(), NativeControlError> {
    let bundle_id = bundle_id.to_string();
    let window_id = window_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let win = resolve_window(&bundle_id, &window_id)?;
        set_bool(
            win.as_ptr() as AXUIElementRef,
            AX_FULLSCREEN_ATTR,
            fullscreen,
        )
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Move window top-left to (x, y) screen coords.
pub async fn move_to(
    bundle_id: &str,
    window_id: &str,
    x: f64,
    y: f64,
) -> Result<(), NativeControlError> {
    let bundle_id = bundle_id.to_string();
    let window_id = window_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let win = resolve_window(&bundle_id, &window_id)?;
        set_cgpoint(win.as_ptr() as AXUIElementRef, kAXPositionAttribute, x, y)
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Resize window to (w, h).
pub async fn resize(
    bundle_id: &str,
    window_id: &str,
    w: f64,
    h: f64,
) -> Result<(), NativeControlError> {
    let bundle_id = bundle_id.to_string();
    let window_id = window_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let win = resolve_window(&bundle_id, &window_id)?;
        set_cgsize(win.as_ptr() as AXUIElementRef, kAXSizeAttribute, w, h)
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

// ---------- low-level setters --------------------------------------------

fn set_bool(elem: AXUIElementRef, attr: &str, v: bool) -> Result<(), NativeControlError> {
    let attr_cf = CFString::new(attr);
    let val = if v {
        CFBoolean::true_value()
    } else {
        CFBoolean::false_value()
    };
    // SAFETY: elem live; attr_cf and val are valid CF refs.
    let err = unsafe {
        AXUIElementSetAttributeValue(
            elem,
            attr_cf.as_concrete_TypeRef(),
            val.as_concrete_TypeRef() as CFTypeRef,
        )
    };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        Err(map_ax_error(err))
    }
}

fn set_cgpoint(elem: AXUIElementRef, attr: &str, x: f64, y: f64) -> Result<(), NativeControlError> {
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let p = CGPoint { x, y };
    // SAFETY: AXValueCreate copies the CGPoint into the AXValue. We own +1.
    let val = unsafe { AXValueCreate(kAXValueTypeCGPoint, &p as *const _ as *const _) };
    if val.is_null() {
        return Err(NativeControlError::Internal(
            "AXValueCreate(CGPoint) returned NULL".into(),
        ));
    }
    let attr_cf = CFString::new(attr);
    // SAFETY: elem live; val is a +1 AXValue.
    let err = unsafe {
        AXUIElementSetAttributeValue(elem, attr_cf.as_concrete_TypeRef(), val as CFTypeRef)
    };
    // SAFETY: drop +1.
    unsafe { core_foundation_sys::base::CFRelease(val as CFTypeRef) };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        Err(map_ax_error(err))
    }
}

fn set_cgsize(elem: AXUIElementRef, attr: &str, w: f64, h: f64) -> Result<(), NativeControlError> {
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let s = CGSize { w, h };
    // SAFETY: AXValueCreate copies the CGSize into the AXValue. We own +1.
    let val = unsafe { AXValueCreate(kAXValueTypeCGSize, &s as *const _ as *const _) };
    if val.is_null() {
        return Err(NativeControlError::Internal(
            "AXValueCreate(CGSize) returned NULL".into(),
        ));
    }
    let attr_cf = CFString::new(attr);
    // SAFETY: elem live; val is a +1 AXValue.
    let err = unsafe {
        AXUIElementSetAttributeValue(elem, attr_cf.as_concrete_TypeRef(), val as CFTypeRef)
    };
    // SAFETY: drop +1.
    unsafe { core_foundation_sys::base::CFRelease(val as CFTypeRef) };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        Err(map_ax_error(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_default_deny() {
        let bg = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = bg.block_on(raise("com.apple.calculator", "w0", false, true));
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.window.raise");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
        let r = bg.block_on(raise("com.apple.calculator", "w0", true, false));
        match r {
            Err(NativeControlError::FocusStealForbidden { .. }) => {}
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
    }

    #[test]
    fn bad_window_id_format_is_ref_stale() {
        let r = resolve_window("com.apple.calculator", "not-a-window-id");
        match r {
            Err(NativeControlError::RefStale { .. }) => {}
            // App might not be running in CI — that's also acceptable.
            Err(NativeControlError::AppNotFound { .. }) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
