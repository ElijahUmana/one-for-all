//! SPEC §12 U6 — `app.notification_center.{open, list, click, dismiss}`.
//!
//! Notification Center is owned by `com.apple.notificationcenterui`. Its
//! pulldown is exposed via the menu bar's clock item; opening it requires a
//! click on that item which is a focus-stealing operation. We default-deny
//! `open` unless the caller passes `confirm + focus_steal`.
//!
//! Once open, banners and the notification list expose themselves under the
//! `com.apple.notificationcenterui` AX root; `list` enumerates them by their
//! AXTitle / AXDescription. `click` re-uses the same path-matching strategy
//! as `dock::press_item`. `dismiss` posts the per-banner "Close" sub-button.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    kAXErrorSuccess, kAXPressAction, kAXTitleAttribute, AXUIElementCreateApplication,
    AXUIElementPerformAction, AXUIElementRef,
};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;

use crate::ax_walk::{copy_children, copy_string_attr, has_action, map_ax_error};
use crate::cf_owned::AxOwned;
use crate::types::NativeControlError;

const NC_BUNDLE: &str = "com.apple.notificationcenterui";

/// Open the Notification Center pulldown. Focus-stealing.
pub async fn open(confirm: bool, focus_steal: bool) -> Result<(), NativeControlError> {
    if !(confirm && focus_steal) {
        return Err(NativeControlError::FocusStealForbidden {
            what: "app.notification_center.open",
        });
    }
    // System Events: emulate the Notification Center toggle by sending the
    // user's keybinding ⌃⌥⌘N if set, falling back to clicking the menu-bar
    // clock. We use the AppleScript click on the `controlcenter` UI which
    // reliably opens the panel on macOS 12+.
    let script = "tell application \"System Events\" to tell process \"ControlCenter\"\n    click menu bar item \"Notification Center\" of menu bar 1\nend tell";
    crate::actions::app_eval("com.apple.systemevents", script).await?;
    Ok(())
}

/// List banner titles currently visible in Notification Center.
pub async fn list() -> Result<Vec<String>, NativeControlError> {
    tokio::task::spawn_blocking(list_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_blocking() -> Result<Vec<String>, NativeControlError> {
    let pid = match crate::actions::resolve_pid(NC_BUNDLE) {
        Ok(p) => p,
        Err(_) => return Ok(vec![]),
    };
    // SAFETY: AXUIElementCreateApplication +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: NC_BUNDLE.into(),
            })?;
    let mut out = Vec::new();
    walk_titles(app.as_ptr() as AXUIElementRef, 0, &mut out);
    Ok(out)
}

fn walk_titles(elem: AXUIElementRef, depth: u32, out: &mut Vec<String>) {
    if depth > 8 || out.len() > 64 {
        return;
    }
    let title = copy_string_attr(elem, kAXTitleAttribute).unwrap_or_default();
    if !title.is_empty() && has_action(elem, kAXPressAction) {
        out.push(title);
    }
    if let Some(kids) = copy_children(elem) {
        for c in kids {
            walk_titles(c.as_ptr() as AXUIElementRef, depth + 1, out);
        }
    }
}

/// Click the first banner whose AXTitle matches `title` exactly. Not
/// focus-stealing per se — the banner's app may activate as a result, but
/// that's the user's notification choosing to surface its own UI, not us
/// raising it.
pub async fn click(title: &str) -> Result<(), NativeControlError> {
    press_by_title(title.to_string(), kAXPressAction).await
}

/// Dismiss a banner by title (presses the banner's "Close" subaction).
pub async fn dismiss(title: &str) -> Result<(), NativeControlError> {
    // macOS exposes the dismiss as a "showAlternateAction"-style verb; we use
    // AXShowMenu to surface options then press Close. For simplicity v1 we
    // dispatch AXCancel which most banners honor.
    let title = title.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let pid = crate::actions::resolve_pid(NC_BUNDLE)?;
        let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
        let app = unsafe {
            AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _)
        }
        .ok_or_else(|| NativeControlError::AppNotFound {
            bundle_id: NC_BUNDLE.into(),
        })?;
        let banner = find_by_title(app.as_ptr() as AXUIElementRef, &title).ok_or_else(|| {
            NativeControlError::RefStale {
                r: format!("notification {title:?}"),
            }
        })?;
        let action_cf = CFString::new("AXCancel");
        // SAFETY: banner is a live AX ref.
        let err = unsafe {
            AXUIElementPerformAction(
                banner.as_ptr() as AXUIElementRef,
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

async fn press_by_title(title: String, action: &'static str) -> Result<(), NativeControlError> {
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        let pid = crate::actions::resolve_pid(NC_BUNDLE)?;
        // SAFETY: +1 or NULL.
        let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
        let app = unsafe {
            AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _)
        }
        .ok_or_else(|| NativeControlError::AppNotFound {
            bundle_id: NC_BUNDLE.into(),
        })?;
        let banner = find_by_title(app.as_ptr() as AXUIElementRef, &title).ok_or_else(|| {
            NativeControlError::RefStale {
                r: format!("notification {title:?}"),
            }
        })?;
        let action_cf = CFString::new(action);
        // SAFETY: banner is a live AX ref.
        let err = unsafe {
            AXUIElementPerformAction(
                banner.as_ptr() as AXUIElementRef,
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

fn find_by_title(
    elem: AXUIElementRef,
    title: &str,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let kids = copy_children(elem)?;
    for k in kids {
        let t = copy_string_attr(k.as_ptr() as AXUIElementRef, kAXTitleAttribute);
        if t.as_deref() == Some(title) {
            return Some(k);
        }
        if let Some(found) = find_by_title(k.as_ptr() as AXUIElementRef, title) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_default_deny() {
        let r = open(false, true).await;
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.notification_center.open");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
    }
}
