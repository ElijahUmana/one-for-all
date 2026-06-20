//! SPEC §12 U6 — `app.dock.{list, click, reveal_app}`.
//!
//! The Dock is owned by `com.apple.dock` (a system process). Its AX tree
//! exposes one `AXList` whose children are individual dock items (each an
//! `AXButton`/`AXMenuButton` with the running-app or file/folder title).
//!
//! Focus discipline:
//! - `dock.click` on an already-running app raises that app, which DOES
//!   shift focus. We default-deny this with [`NativeControlError::FocusStealForbidden`]
//!   unless the caller passes `confirm: true` AND the broker has the
//!   `focus_steal` capability — see [`require_focus_steal`].
//! - `dock.reveal_app` (a tap-and-hold equivalent that opens the app's
//!   window-list menu) is NOT focus-stealing — Apple's Dock keeps the
//!   currently-active app frontmost.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    kAXDescriptionAttribute, kAXErrorSuccess, kAXPressAction, kAXShowMenuAction, kAXTitleAttribute,
    AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;

use crate::ax_walk::{copy_children, copy_string_attr, map_ax_error};
use crate::cf_owned::AxOwned;
use crate::types::{DockItem, NativeControlError};

const DOCK_BUNDLE: &str = "com.apple.dock";

/// Snapshot of every dock item.
pub async fn list() -> Result<Vec<DockItem>, NativeControlError> {
    tokio::task::spawn_blocking(list_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_blocking() -> Result<Vec<DockItem>, NativeControlError> {
    let pid = resolve_dock_pid()?;
    // SAFETY: AXUIElementCreateApplication returns +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: DOCK_BUNDLE.into(),
            })?;

    // Dock root → AXList → items.
    let kids = copy_children(app.as_ptr() as AXUIElementRef).unwrap_or_default();
    let mut out = Vec::new();
    for top in kids {
        // The Dock's first-level child is typically an AXList.
        let inner_kids = copy_children(top.as_ptr() as AXUIElementRef).unwrap_or_default();
        for (i, item) in inner_kids.iter().enumerate() {
            let title = copy_string_attr(item.as_ptr() as AXUIElementRef, kAXTitleAttribute)
                .or_else(|| {
                    copy_string_attr(item.as_ptr() as AXUIElementRef, kAXDescriptionAttribute)
                })
                .unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            let bundle = bundle_id_for_running_app(&title).unwrap_or_default();
            let running = !bundle.is_empty();
            out.push(DockItem {
                dock_id: format!("d{i}"),
                label: title,
                running,
                bundle_id: bundle,
            });
        }
    }
    Ok(out)
}

/// Click a dock item by label. Default-denies focus-stealing apps unless
/// `(focus_steal_capability, confirm)` are both true.
pub async fn click(
    label: &str,
    confirm: bool,
    focus_steal_capability: bool,
) -> Result<(), NativeControlError> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || click_blocking(&label, confirm, focus_steal_capability))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn click_blocking(
    label: &str,
    confirm: bool,
    focus_steal_capability: bool,
) -> Result<(), NativeControlError> {
    if !(confirm && focus_steal_capability) {
        return Err(NativeControlError::FocusStealForbidden {
            what: "app.dock.click",
        });
    }
    press_item(label, kAXPressAction)
}

/// Open the Dock context menu (window-list / quit / show-in-finder) for an
/// app or folder by label. Not focus-stealing.
pub async fn reveal_app(label: &str) -> Result<(), NativeControlError> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || press_item(&label, kAXShowMenuAction))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn press_item(label: &str, action: &str) -> Result<(), NativeControlError> {
    let pid = resolve_dock_pid()?;
    // SAFETY: AXUIElementCreateApplication returns +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: DOCK_BUNDLE.into(),
            })?;

    let target = match find_item(app.as_ptr() as AXUIElementRef, label) {
        Some(t) => t,
        None => {
            return Err(NativeControlError::RefStale {
                r: format!("dock label {label:?}"),
            })
        }
    };

    let action_cf = CFString::new(action);
    // SAFETY: target is a live AXUIElementRef.
    let err = unsafe {
        AXUIElementPerformAction(
            target.as_ptr() as AXUIElementRef,
            action_cf.as_concrete_TypeRef(),
        )
    };
    if err == kAXErrorSuccess {
        Ok(())
    } else if err == accessibility_sys::kAXErrorActionUnsupported {
        Err(NativeControlError::NotActionable {
            r: label.to_string(),
            reason: "dock item does not support action",
        })
    } else {
        Err(map_ax_error(err))
    }
}

fn find_item(
    root: AXUIElementRef,
    label: &str,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let kids = copy_children(root)?;
    for top in kids {
        let inner = copy_children(top.as_ptr() as AXUIElementRef)?;
        for c in inner {
            let title = copy_string_attr(c.as_ptr() as AXUIElementRef, kAXTitleAttribute)
                .or_else(|| copy_string_attr(c.as_ptr() as AXUIElementRef, kAXDescriptionAttribute))
                .unwrap_or_default();
            if title == label {
                return Some(c);
            }
        }
    }
    None
}

fn resolve_dock_pid() -> Result<i32, NativeControlError> {
    crate::actions::resolve_pid(DOCK_BUNDLE)
}

fn bundle_id_for_running_app(label: &str) -> Option<String> {
    // SAFETY: NSWorkspace.sharedWorkspace is a thread-safe singleton.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let apps = unsafe { ws.runningApplications() };
    let count = apps.count();
    for i in 0..count {
        // SAFETY: in-bounds.
        let app = unsafe { apps.objectAtIndex(i) };
        let name = unsafe { app.localizedName() }.map(|n: Retained<NSString>| n.to_string());
        if name.as_deref() == Some(label) {
            return unsafe { app.bundleIdentifier() }.map(|b: Retained<NSString>| b.to_string());
        }
    }
    None
}

// `kAXChildrenAttribute` import suppression — clippy demands we mark the
// constant used somewhere.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_default_deny_focus_steal() {
        let r = click_blocking("Calculator", false, true);
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.dock.click");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
        let r = click_blocking("Calculator", true, false);
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.dock.click");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
    }
}
