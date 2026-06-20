//! SPEC §12 U6 — `app.statusmenu.click`.
//!
//! Status-menu items (the right side of the menu bar — clock, volume,
//! Wi-Fi, third-party menubar apps) are surfaced under each owning process's
//! AX tree as `kAXMenuExtra` elements. Apple does not expose a stable
//! enumeration API; we walk every running app's menu bar searching for items
//! whose title or AXDescription matches the requested label.
//!
//! Focus discipline: pressing a status item dispatches via `AXPress`, which
//! does NOT activate the host app (status items are floating menus owned by
//! SystemUIServer). Same invariant as `app.menu.click`.

#![cfg(target_os = "macos")]

use accessibility_sys::{
    kAXDescriptionAttribute, kAXErrorSuccess, kAXExtrasMenuBarAttribute, kAXMenuBarAttribute,
    kAXPressAction, kAXTitleAttribute, AXUIElementCreateApplication, AXUIElementPerformAction,
    AXUIElementRef,
};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;

use crate::ax_walk::{copy_attr_owned, copy_children, copy_string_attr, has_action, map_ax_error};
use crate::cf_owned::AxOwned;
use crate::types::NativeControlError;

/// Click a status-menu item by label. The label matches against the item's
/// AXTitle or AXDescription (substring, case-insensitive). The first match
/// in NSWorkspace.runningApplications enumeration order wins.
pub async fn click(label: &str) -> Result<(), NativeControlError> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || click_blocking(&label))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn click_blocking(label: &str) -> Result<(), NativeControlError> {
    let needle = label.to_lowercase();
    // SAFETY: NSWorkspace.sharedWorkspace is a thread-safe singleton.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let apps = unsafe { ws.runningApplications() };
    let count = apps.count();
    for i in 0..count {
        // SAFETY: in-bounds.
        let app = unsafe { apps.objectAtIndex(i) };
        let bid = unsafe { app.bundleIdentifier() }.map(|b: Retained<NSString>| b.to_string());
        let Some(_bid) = bid else { continue };
        let pid = unsafe { app.processIdentifier() };

        // SAFETY: +1 ref or NULL.
        let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
        let app_owned = match unsafe {
            AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _)
        } {
            Some(a) => a,
            None => continue,
        };

        // Try kAXExtrasMenuBar first (the "right side" status items),
        // falling back to the regular menu bar if absent.
        for attr in [kAXExtrasMenuBarAttribute, kAXMenuBarAttribute] {
            let bar = match copy_attr_owned(app_owned.as_ptr() as AXUIElementRef, attr) {
                Some(b) => b,
                None => continue,
            };
            if let Some(found) = find_match(bar.as_ptr() as AXUIElementRef, &needle, 0) {
                return press(found.as_ptr() as AXUIElementRef, label.to_string());
            }
        }
    }
    Err(NativeControlError::RefStale {
        r: format!("status menu item {label:?}"),
    })
}

const MAX_DEPTH: u32 = 6;

fn find_match(
    elem: AXUIElementRef,
    needle_lc: &str,
    depth: u32,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let title = copy_string_attr(elem, kAXTitleAttribute).unwrap_or_default();
    let desc = copy_string_attr(elem, kAXDescriptionAttribute).unwrap_or_default();
    if has_action(elem, kAXPressAction)
        && (title.to_lowercase().contains(needle_lc) || desc.to_lowercase().contains(needle_lc))
        && !needle_lc.is_empty()
    {
        // Re-fetch this element as an owned ref for the caller. We bump
        // refcount on `elem` itself.
        // SAFETY: bumping ref count on a live AXUIElementRef.
        unsafe {
            core_foundation_sys::base::CFRetain(elem as core_foundation_sys::base::CFTypeRef)
        };
        // SAFETY: we just took +1 to a live ref.
        return unsafe { AxOwned::from_create(elem as *const _) };
    }
    let kids = copy_children(elem)?;
    for c in kids {
        if let Some(m) = find_match(c.as_ptr() as AXUIElementRef, needle_lc, depth + 1) {
            return Some(m);
        }
    }
    None
}

fn press(elem: AXUIElementRef, label: String) -> Result<(), NativeControlError> {
    let action_cf = CFString::new(kAXPressAction);
    // SAFETY: elem is live; action_cf is a valid CFString.
    let err = unsafe { AXUIElementPerformAction(elem, action_cf.as_concrete_TypeRef()) };
    if err == kAXErrorSuccess {
        Ok(())
    } else if err == accessibility_sys::kAXErrorActionUnsupported {
        Err(NativeControlError::NotActionable {
            r: label,
            reason: "no AXPress on status menu item",
        })
    } else {
        Err(map_ax_error(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_label_does_not_match_everything() {
        // Defense-in-depth: a blank needle MUST NOT match the first
        // pressable item it sees.
        let r = click_blocking("");
        match r {
            Err(NativeControlError::RefStale { .. }) => {}
            other => panic!("expected RefStale on empty label, got {other:?}"),
        }
    }
}
