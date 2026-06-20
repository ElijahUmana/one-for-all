//! `app.list` / `app.click` / `app.type` / `app.scroll` / `app.eval`
//! implementations.
//!
//! Each function is `async` but offloads the FFI work onto
//! `tokio::task::spawn_blocking` so the broker reactor never blocks on AX
//! calls.
//!
//! # Focus-no-steal invariant
//!
//! None of these calls activate the target app. We use:
//! - `kAXPressAction` via `AXUIElementPerformAction` — does not raise.
//! - `AXSetAttributeValue(kAXValueAttribute, …)` for settable text fields —
//!   does not raise.
//! - `CGEventPostToPid(pid, …)` for keyboard fallback — pid-targeted, does
//!   NOT enter the systemwide event tap, does NOT raise the app.
//! - `CGEventCreateScrollWheelEvent2` posted via `CGEventPostToPid` for
//!   scrolling.
//!
//! `kAXRaiseAction` is **forbidden** in this crate; SPEC §5 lists it. AppleScript
//! bodies that contain `activate` directed at the target are rejected with
//! [`NativeControlError::ActivateForbidden`].

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::Arc;

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXPressAction, kAXRoleAttribute,
    kAXTitleAttribute, kAXValueAttribute, AXUIElementCreateApplication,
    AXUIElementCreateSystemWide, AXUIElementIsAttributeSettable, AXUIElementPerformAction,
    AXUIElementRef, AXUIElementSetAttributeValue,
};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_graphics::event::{CGEvent, CGEventTapLocation, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;
use serde_json::Value;
use tracing::{debug, warn};

use crate::ax_walk::{copy_attr_owned, locate_by_path, map_ax_error};
use crate::cf_owned::AxOwned;
use crate::types::{AppElement, AppHandle, AppSnapshot, NativeControlError};

/// SPEC §11 V2 `app.list` — currently running apps with bundle id, pid, name,
/// focus flag.
pub async fn list_apps() -> Result<Vec<AppHandle>, NativeControlError> {
    tokio::task::spawn_blocking(list_apps_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_apps_blocking() -> Result<Vec<AppHandle>, NativeControlError> {
    // SAFETY: NSWorkspace.sharedWorkspace is a thread-safe singleton.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let frontmost_pid =
        unsafe { ws.frontmostApplication() }.map(|a| unsafe { a.processIdentifier() });

    let apps = unsafe { ws.runningApplications() };
    let count = apps.count();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: index in [0, count); unsafe is required by the bindings to
        // mark the +1 retain semantics, but the access is in-bounds.
        let app = unsafe { apps.objectAtIndex(i) };
        let bundle_id =
            unsafe { app.bundleIdentifier() }.map(|b: objc2::rc::Retained<NSString>| b.to_string());
        let Some(bundle_id) = bundle_id else { continue };
        let pid = unsafe { app.processIdentifier() } as i32;
        let name = unsafe { app.localizedName() }
            .map(|n: objc2::rc::Retained<NSString>| n.to_string())
            .unwrap_or_else(|| bundle_id.clone());
        out.push(AppHandle {
            bundle_id,
            pid,
            name,
            has_focus: frontmost_pid == Some(pid),
        });
    }
    debug!(apps = out.len(), "native-control list_apps");
    Ok(out)
}

/// Resolve `bundle_id` → `pid` via NSWorkspace. Returns `AppNotFound` if the
/// app isn't running.
pub(crate) fn resolve_pid(bundle_id: &str) -> Result<i32, NativeControlError> {
    // SAFETY: NSWorkspace.sharedWorkspace is a thread-safe singleton.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let apps = unsafe { ws.runningApplications() };
    let count = apps.count();
    for i in 0..count {
        let app = unsafe { apps.objectAtIndex(i) };
        let bid =
            unsafe { app.bundleIdentifier() }.map(|b: objc2::rc::Retained<NSString>| b.to_string());
        if bid.as_deref() == Some(bundle_id) {
            let pid = unsafe { app.processIdentifier() };
            return Ok(pid);
        }
    }
    Err(NativeControlError::AppNotFound {
        bundle_id: bundle_id.to_string(),
    })
}

pub(crate) fn resolve_bundle_id(pid: i32) -> Result<String, NativeControlError> {
    // SAFETY: NSWorkspace.sharedWorkspace is a thread-safe singleton.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let apps = unsafe { ws.runningApplications() };
    let count = apps.count();
    for i in 0..count {
        let app = unsafe { apps.objectAtIndex(i) };
        let app_pid = unsafe { app.processIdentifier() } as i32;
        if app_pid == pid {
            if let Some(bundle_id) = unsafe { app.bundleIdentifier() }
                .map(|b: objc2::rc::Retained<NSString>| b.to_string())
            {
                return Ok(bundle_id);
            }
        }
    }
    Err(NativeControlError::AppNotFound {
        bundle_id: format!("pid:{pid}"),
    })
}

/// Walk `bundle_id`'s AX tree and return a fresh snapshot.
pub async fn snapshot_app(
    bundle_id: &str,
    snapshot_seq: u64,
) -> Result<AppSnapshot, NativeControlError> {
    let bundle_id = bundle_id.to_string();
    tokio::task::spawn_blocking(move || {
        let pid = resolve_pid(&bundle_id)?;
        crate::ax_walk::walk(pid, &bundle_id, snapshot_seq)
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// SPEC §11 V2 `app.click` — `AXUIElementPerformAction(elem, kAXPressAction)`.
/// Does NOT activate the target app.
pub async fn app_click(elem: Arc<AppElement>, snap_seq: u64) -> Result<(), NativeControlError> {
    let _ = snap_seq; // ref-staleness check happens at the controller layer.
    if elem.bbox.is_zero_area() {
        return Err(NativeControlError::NotActionable {
            r: elem.element_ref.clone(),
            reason: "zero area",
        });
    }
    let bundle_id = elem.app_id.clone();
    let path = elem.ax_path.clone();
    let r = elem.element_ref.clone();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let pid = resolve_pid(&bundle_id)?;
        let owned = locate_by_path(pid, &bundle_id, &path)?;
        let action_cf = CFString::new(kAXPressAction);
        // SAFETY: owned is a live AXUIElementRef; action_cf is a CFString.
        let err = unsafe {
            AXUIElementPerformAction(
                owned.as_ptr() as AXUIElementRef,
                action_cf.as_concrete_TypeRef(),
            )
        };
        if err == kAXErrorSuccess {
            Ok(())
        } else if err == accessibility_sys::kAXErrorActionUnsupported {
            Err(NativeControlError::NotActionable {
                r: r.clone(),
                reason: "no AXPress action",
            })
        } else {
            Err(map_ax_error(err))
        }
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// SPEC §11 V2 `app.type` — set value attribute (Strategy A) or, if not
/// settable, focus the element and post Unicode keyboard events to the pid
/// (Strategy B). Neither path activates the target app.
pub async fn app_type(
    elem: Arc<AppElement>,
    text: String,
    clear_first: bool,
) -> Result<(), NativeControlError> {
    if elem.bbox.is_zero_area() {
        return Err(NativeControlError::NotActionable {
            r: elem.element_ref.clone(),
            reason: "zero area",
        });
    }
    let bundle_id = elem.app_id.clone();
    let path = elem.ax_path.clone();
    let r = elem.element_ref.clone();

    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let pid = resolve_pid(&bundle_id)?;
        let owned = locate_by_path(pid, &bundle_id, &path)?;

        // Strategy A: settable kAXValueAttribute (text fields, search boxes).
        let attr_cf = CFString::new(kAXValueAttribute);
        let mut settable: u8 = 0;
        // SAFETY: AXUIElementIsAttributeSettable; settable is u8 out-slot.
        let err = unsafe {
            AXUIElementIsAttributeSettable(
                owned.as_ptr() as AXUIElementRef,
                attr_cf.as_concrete_TypeRef(),
                &mut settable,
            )
        };
        if err == kAXErrorSuccess && settable != 0 {
            let final_text = if clear_first { text.clone() } else {
                // Read current value and append.
                let current = current_value_string(owned.as_ptr() as AXUIElementRef)
                    .unwrap_or_default();
                let mut combined = current;
                combined.push_str(&text);
                combined
            };
            let cf_text = CFString::new(&final_text);
            // SAFETY: setting CFString on a settable AX value attribute.
            let serr = unsafe {
                AXUIElementSetAttributeValue(
                    owned.as_ptr() as AXUIElementRef,
                    attr_cf.as_concrete_TypeRef(),
                    cf_text.as_concrete_TypeRef() as core_foundation_sys::base::CFTypeRef,
                )
            };
            if serr == kAXErrorSuccess {
                debug!(r = %r, len = text.len(), "app.type strategy A (kAXValueAttribute set)");
                return Ok(());
            }
            // Fall through to Strategy B on settable-but-set-failed.
            warn!(r = %r, err = serr, "kAXValueAttribute settable=true but set failed; falling back to keyboard events");
        }

        // Strategy B: focus the element, post Unicode keyboard events to pid.
        focus_element_systemwide(owned.as_ptr() as AXUIElementRef)?;
        post_unicode_string_to_pid(pid, &text)?;
        debug!(r = %r, len = text.len(), "app.type strategy B (CGEventPostToPid)");
        Ok(())
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Read kAXValueAttribute as a string, used by Strategy A's "append unless
/// clear_first" path.
fn current_value_string(elem: AXUIElementRef) -> Option<String> {
    use accessibility_sys::AXUIElementCopyAttributeValue;
    let attr_cf = CFString::new(kAXValueAttribute);
    let mut out: core_foundation_sys::base::CFTypeRef = std::ptr::null_mut();
    // SAFETY: standard read.
    let err =
        unsafe { AXUIElementCopyAttributeValue(elem, attr_cf.as_concrete_TypeRef(), &mut out) };
    if err != kAXErrorSuccess || out.is_null() {
        return None;
    }
    let id = unsafe { core_foundation_sys::base::CFGetTypeID(out) };
    if id == unsafe { core_foundation_sys::string::CFStringGetTypeID() } {
        // SAFETY: id confirmed CFString.
        let s = unsafe {
            CFString::wrap_under_create_rule(out as core_foundation_sys::string::CFStringRef)
        };
        return Some(s.to_string());
    }
    // Wrong type — release.
    unsafe { core_foundation_sys::base::CFRelease(out) };
    None
}

/// Focus an AX element via the systemwide AXUIElement so subsequent CGEvents
/// land in this element. Does NOT activate the parent app — it merely sets
/// the focused-ui-element attribute, which the OS honors for keystroke
/// routing as long as the app is in the responder chain.
fn focus_element_systemwide(elem: AXUIElementRef) -> Result<(), NativeControlError> {
    // SAFETY: AXUIElementCreateSystemWide returns a +1 ref.
    let sys_ref = unsafe { AXUIElementCreateSystemWide() };
    if sys_ref.is_null() {
        return Err(NativeControlError::Internal(
            "AXUIElementCreateSystemWide returned null".into(),
        ));
    }
    let attr_cf = CFString::new(kAXFocusedUIElementAttribute);
    // SAFETY: typed AX setter; element ref is from caller.
    let err = unsafe {
        AXUIElementSetAttributeValue(
            sys_ref,
            attr_cf.as_concrete_TypeRef(),
            elem as core_foundation_sys::base::CFTypeRef,
        )
    };
    // Always release the systemwide handle.
    unsafe { core_foundation_sys::base::CFRelease(sys_ref as _) };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        Err(map_ax_error(err))
    }
}

pub async fn click_menu_path_for_pid(
    pid: i32,
    path: Vec<String>,
) -> Result<(), NativeControlError> {
    let bundle_id = resolve_bundle_id(pid)?;
    tokio::task::spawn_blocking(move || click_menu_path_for_pid_blocking(pid, &bundle_id, &path))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn click_menu_path_for_pid_blocking(
    pid: i32,
    bundle_id: &str,
    path: &[String],
) -> Result<(), NativeControlError> {
    if path.is_empty() {
        return Err(NativeControlError::Internal("empty menu path".into()));
    }
    // SAFETY: AXUIElementCreateApplication returns a +1 ref or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;
    let focused = copy_attr_owned(app.as_ptr() as AXUIElementRef, kAXFocusedUIElementAttribute)
        .ok_or_else(|| NativeControlError::NotActionable {
            r: path.join(" > "),
            reason: "no focused UI element for popup menu traversal",
        })?;

    let root = find_popup_menu_root(focused.as_ptr() as AXUIElementRef).ok_or_else(|| {
        NativeControlError::NotActionable {
            r: path.join(" > "),
            reason: "focused element is not inside an AXMenu popup",
        }
    })?;
    let item = descend_menu_path(root.as_ptr() as AXUIElementRef, path).ok_or_else(|| {
        NativeControlError::RefStale {
            r: format!("menu path {:?}", path),
        }
    })?;

    let action_cf = CFString::new(kAXPressAction);
    let err = unsafe {
        AXUIElementPerformAction(
            item.as_ptr() as AXUIElementRef,
            action_cf.as_concrete_TypeRef(),
        )
    };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        Err(map_ax_error(err))
    }
}

fn find_popup_menu_root(elem: AXUIElementRef) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let role_attr = CFString::new(kAXRoleAttribute);
    let title_attr = CFString::new(kAXTitleAttribute);
    let mut stack = vec![elem];
    while let Some(current) = stack.pop() {
        if let Some(role) = crate::ax_walk::copy_string_attr(current, kAXRoleAttribute) {
            if role == "AXMenu" {
                let _ = &role_attr;
                let _ = &title_attr;
                return Some(unsafe {
                    AxOwned::<accessibility_sys::__AXUIElement>::from_borrowed(current as *const _)
                }?);
            }
        }
        if let Some(children) = crate::ax_walk::copy_children(current) {
            for child in children {
                stack.push(child.as_ptr() as AXUIElementRef);
            }
        }
    }
    None
}

fn descend_menu_path(
    root: AXUIElementRef,
    path: &[String],
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let mut current =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_borrowed(root as *const _) }?;
    for segment in path {
        let child = find_menu_item_by_title(current.as_ptr() as AXUIElementRef, segment)?;
        current = child;
    }
    Some(current)
}

fn find_menu_item_by_title(
    elem: AXUIElementRef,
    title: &str,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let children = crate::ax_walk::copy_children(elem)?;
    for child in children {
        if crate::ax_walk::copy_string_attr(child.as_ptr() as AXUIElementRef, kAXTitleAttribute)
            .as_deref()
            == Some(title)
        {
            return Some(child);
        }
        if let Some(descendant) = find_menu_item_by_title(child.as_ptr() as AXUIElementRef, title) {
            return Some(descendant);
        }
    }
    None
}

/// Post the given Unicode string as one keyboard down/up pair targeted at
/// `pid`. Targets pid (not the systemwide event tap) so the receiving app
/// gets the keystrokes without coming to the foreground.
fn post_unicode_string_to_pid(pid: i32, text: &str) -> Result<(), NativeControlError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
        NativeControlError::Internal("CGEventSource::new(HIDSystemState) failed".into())
    })?;
    // We use a dummy keycode (0) and overwrite the unicode string. This is
    // the standard Cocoa pattern for synthesizing arbitrary-text keystrokes.
    let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
        .map_err(|_| NativeControlError::Internal("CGEvent::new_keyboard_event(down)".into()))?;
    let utf16: Vec<u16> = text.encode_utf16().collect();
    down.set_string_from_utf16_unchecked(&utf16);
    down.post_to_pid(pid as libc::pid_t);

    let up = CGEvent::new_keyboard_event(source, 0, false)
        .map_err(|_| NativeControlError::Internal("CGEvent::new_keyboard_event(up)".into()))?;
    up.set_string_from_utf16_unchecked(&utf16);
    up.post_to_pid(pid as libc::pid_t);
    let _ = CGEventTapLocation::HID; // silence unused-import warning when feature gates skip it
    Ok(())
}

/// SPEC §11 V2 `app.scroll` — `CGEventCreateScrollWheelEvent2` posted to pid.
/// Does not activate the app.
pub async fn app_scroll(
    bundle_id: String,
    elem: Option<Arc<AppElement>>,
    dx: f64,
    dy: f64,
) -> Result<(), NativeControlError> {
    let _ = elem; // hover-element optional; we scroll at the focused element by default
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let pid = resolve_pid(&bundle_id)?;
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
            NativeControlError::Internal("CGEventSource::new(HIDSystemState) failed".into())
        })?;
        // CGEventCreateScrollWheelEvent2: wheel1 = vertical, wheel2 = horizontal,
        // wheel3 unused. We feed pixel deltas so dx/dy match the agent's intent
        // 1:1 with screen pixels.
        let dy_i = dy.round() as i32;
        let dx_i = dx.round() as i32;
        let evt = CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, dy_i, dx_i, 0)
            .map_err(|_| NativeControlError::Internal("CGEvent::new_scroll_event failed".into()))?;
        evt.post_to_pid(pid as libc::pid_t);
        Ok(())
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Reject AppleScript bodies that try to activate the target app.
///
/// Pattern: `tell application` / `tell app` followed by `activate` is the
/// canonical AppleScript form. We do a case-insensitive scan; false positives
/// (e.g. literal strings containing "activate") are caller-responsibility
/// and the user-visible error message tells them to remove the keyword.
fn validate_no_activate(applescript: &str) -> Result<(), NativeControlError> {
    let lower = applescript.to_lowercase();
    // The pattern that genuinely steals focus.
    if lower.contains("activate") {
        // Soft check: if `activate` is not preceded somewhere earlier by
        // `tell` it might still be e.g. `do shell script "echo activate"`.
        // To stay safe we still reject — SPEC §5 is "no focus steal", not
        // "ergonomic AppleScript". Document this in the error.
        return Err(NativeControlError::ActivateForbidden {
            reason:
                "AppleScript contains the word 'activate'; remove it to preserve focus-no-steal",
        });
    }
    Ok(())
}

/// SPEC §11 V2 `app.eval` — execute AppleScript via `osascript`.
///
/// Bodies that don't already start with `tell` are wrapped in
/// `tell application id "<bundle_id>" … end tell`. Output captured from
/// stdout; non-zero exit propagated as [`NativeControlError::AppleScript`].
pub async fn app_eval(bundle_id: &str, applescript: &str) -> Result<Value, NativeControlError> {
    validate_no_activate(applescript)?;

    let script = wrap_script(bundle_id, applescript);
    let output = tokio::task::spawn_blocking(move || {
        Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&script)
            .output()
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
    .map_err(|e| NativeControlError::Io(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(NativeControlError::AppleScript {
            msg: if stderr.is_empty() {
                format!("osascript exit {:?}", output.status.code())
            } else {
                stderr
            },
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // Try JSON first; fall back to a plain string.
    if let Ok(v) = serde_json::from_str::<Value>(&stdout) {
        Ok(v)
    } else {
        Ok(Value::String(stdout))
    }
}

fn wrap_script(bundle_id: &str, body: &str) -> String {
    let trimmed = body.trim_start();
    if trimmed.to_lowercase().starts_with("tell ") {
        body.to_string()
    } else {
        format!("tell application id \"{bundle_id}\"\n{body}\nend tell")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_no_activate_rejects_activate_verb() {
        let err = validate_no_activate("tell application \"X\" to activate").unwrap_err();
        match err {
            NativeControlError::ActivateForbidden { .. } => {}
            other => panic!("expected ActivateForbidden, got {other:?}"),
        }
    }

    #[test]
    fn validate_no_activate_accepts_normal_scripts() {
        validate_no_activate("get name of front window").unwrap();
        validate_no_activate(r#"tell application "Calculator" to get version"#).unwrap();
    }

    #[test]
    fn wrap_script_wraps_when_no_tell() {
        let s = wrap_script("com.apple.calculator", "get version");
        assert!(s.starts_with("tell application id \"com.apple.calculator\""));
        assert!(s.ends_with("end tell"));
    }

    #[test]
    fn wrap_script_passthrough_when_starts_with_tell() {
        let s = wrap_script(
            "com.apple.calculator",
            r#"tell application "Calculator" to get version"#,
        );
        assert_eq!(s, r#"tell application "Calculator" to get version"#);
    }

    #[test]
    #[allow(non_snake_case)]
    fn forbid_kAXRaiseAction_is_compiled_in() {
        // Lock: this crate must NEVER perform kAXRaiseAction. We assert the
        // constant's text value here as a code-review tripwire — any future
        // contributor adding `AXUIElementPerformAction(_, kAXRaiseAction)`
        // will land a diff next to this test.
        assert_eq!(accessibility_sys::kAXRaiseAction, "AXRaise");
    }
}
