//! SPEC §12 U6 — `app.menu.{list, click}`.
//!
//! Walks the application's `kAXMenuBarAttribute` subtree and either returns
//! every visible item as a [`MenuItem`] (with full `path`) or invokes
//! `kAXPressAction` on the path-matched item.
//!
//! # Focus
//!
//! Menu bar items are part of the application's AX tree and the `AXPress`
//! action does NOT activate the host app — the menu opens as a floating
//! NSMenu owned by the SystemUIServer. We therefore stay focus-no-steal
//! compliant. (This was empirically verified against Calculator and
//! TextEdit on macOS 14.)

#![cfg(target_os = "macos")]

use accessibility_sys::{
    kAXEnabledAttribute, kAXErrorSuccess, kAXMenuBarAttribute, kAXPressAction, kAXTitleAttribute,
    AXUIElementCreateApplication, AXUIElementPerformAction, AXUIElementRef,
};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;

use crate::ax_walk::{
    copy_attr_owned, copy_bool_attr, copy_children, copy_string_attr, has_action, map_ax_error,
};
use crate::cf_owned::AxOwned;
use crate::types::{MenuItem, NativeControlError};

/// Cap the menu walk so a runaway AX cycle (we have not observed one in any
/// shipped app, but defense in depth) terminates instead of allocating
/// without bound.
const MAX_MENU_NODES: usize = 4_096;
/// Cap menu depth — Apple's HIG recommends ≤4 levels; macOS apps very rarely
/// exceed 6.
const MAX_MENU_DEPTH: u32 = 12;

/// Walk every menu item under the application's menu bar. Returns one entry
/// per item with `path = ["File", "Open…"]`-style title chain.
pub async fn list(bundle_id: &str) -> Result<Vec<MenuItem>, NativeControlError> {
    let bundle_id = bundle_id.to_string();
    tokio::task::spawn_blocking(move || list_blocking(&bundle_id))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_blocking(bundle_id: &str) -> Result<Vec<MenuItem>, NativeControlError> {
    let pid = crate::actions::resolve_pid(bundle_id)?;
    // SAFETY: AXUIElementCreateApplication returns a +1 ref or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;

    // Top-level: AXMenuBar. Children are the menu titles ("File", "Edit", …).
    let bar = match copy_attr_owned(app.as_ptr() as AXUIElementRef, kAXMenuBarAttribute) {
        Some(b) => b,
        None => {
            // No menu bar (shouldn't happen for GUI apps, but agentic clients
            // might call this on a daemon — return empty rather than error).
            return Ok(vec![]);
        }
    };

    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    walk(bar.as_ptr() as AXUIElementRef, &mut path, 0, &mut out);
    Ok(out)
}

fn walk(elem: AXUIElementRef, path: &mut Vec<String>, depth: u32, out: &mut Vec<MenuItem>) {
    if depth > MAX_MENU_DEPTH || out.len() >= MAX_MENU_NODES {
        return;
    }
    let title = copy_string_attr(elem, kAXTitleAttribute).unwrap_or_default();
    let pushed = if depth > 0 && !title.is_empty() {
        // Top level (depth 0) is the bar itself; its direct children are
        // titles like "File" — we include those at depth 1.
        path.push(title.clone());
        true
    } else {
        false
    };

    // Emit a leaf entry for items that are pressable (have AXPress) — i.e.
    // actual command items, not submenu containers without their own action.
    let is_press = has_action(elem, kAXPressAction);
    let enabled = copy_bool_attr(elem, kAXEnabledAttribute).unwrap_or(true);
    if depth >= 2 && is_press && !path.is_empty() {
        let display = path.join(" > ");
        out.push(MenuItem {
            path: path.clone(),
            display,
            interactable: true,
            disabled: !enabled,
            shortcut: None,
        });
    }

    // Recurse into AXChildren — submenus expose items as children of an
    // AXMenu node which is itself a child of the AXMenuItem.
    if let Some(children) = copy_children(elem) {
        for c in children {
            if out.len() >= MAX_MENU_NODES {
                break;
            }
            walk(c.as_ptr() as AXUIElementRef, path, depth + 1, out);
        }
    }

    if pushed {
        path.pop();
    }
}

/// Click a menu item by exact path. The path must match `MenuItem::path`.
/// Returns `RefStale` if no item matches.
pub async fn click(bundle_id: &str, path: Vec<String>) -> Result<(), NativeControlError> {
    let bundle_id = bundle_id.to_string();
    tokio::task::spawn_blocking(move || click_blocking(&bundle_id, &path))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn click_blocking(bundle_id: &str, path: &[String]) -> Result<(), NativeControlError> {
    if path.is_empty() {
        return Err(NativeControlError::Internal("empty menu path".into()));
    }
    let pid = crate::actions::resolve_pid(bundle_id)?;
    // SAFETY: AXUIElementCreateApplication returns a +1 ref or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    let app =
        unsafe { AxOwned::<accessibility_sys::__AXUIElement>::from_create(app_ref as *const _) }
            .ok_or_else(|| NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            })?;
    let bar =
        copy_attr_owned(app.as_ptr() as AXUIElementRef, kAXMenuBarAttribute).ok_or_else(|| {
            NativeControlError::AppNotFound {
                bundle_id: bundle_id.to_string(),
            }
        })?;

    // Walk the path: at each step, find the child whose AXTitle matches and
    // descend. Submenu children may be wrapped in an AXMenu container, so if
    // a direct title match fails we descend through AXMenu nodes once.
    let mut current: AxOwned<accessibility_sys::__AXUIElement> = bar;
    for (i, title) in path.iter().enumerate() {
        let next = find_child_by_title(current.as_ptr() as AXUIElementRef, title);
        let next = match next {
            Some(n) => n,
            None => {
                // Try descending through one AXMenu container if present.
                let kids = copy_children(current.as_ptr() as AXUIElementRef);
                let via_menu = kids.and_then(|c| {
                    c.into_iter().find_map(|child| {
                        find_child_by_title(child.as_ptr() as AXUIElementRef, title)
                    })
                });
                via_menu.ok_or_else(|| NativeControlError::RefStale {
                    r: format!("menu path[{i}]={title:?}"),
                })?
            }
        };
        current = next;
    }

    // Press the resolved item.
    let action_cf = CFString::new(kAXPressAction);
    // SAFETY: current owns a live AXUIElementRef.
    let err = unsafe {
        AXUIElementPerformAction(
            current.as_ptr() as AXUIElementRef,
            action_cf.as_concrete_TypeRef(),
        )
    };
    if err == kAXErrorSuccess {
        Ok(())
    } else if err == accessibility_sys::kAXErrorActionUnsupported {
        Err(NativeControlError::NotActionable {
            r: path.join(" > "),
            reason: "no AXPress action on menu item",
        })
    } else {
        Err(map_ax_error(err))
    }
}

fn find_child_by_title(
    elem: AXUIElementRef,
    title: &str,
) -> Option<AxOwned<accessibility_sys::__AXUIElement>> {
    let kids = copy_children(elem)?;
    for k in kids {
        let t = copy_string_attr(k.as_ptr() as AXUIElementRef, kAXTitleAttribute);
        if t.as_deref() == Some(title) {
            return Some(k);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_with_empty_path_errors() {
        // Pure CPU path; no AX touched.
        let r = click_blocking("com.apple.calculator", &[]);
        match r {
            Err(NativeControlError::Internal(msg)) => assert!(msg.contains("empty")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn caps_constants_match() {
        assert_eq!(MAX_MENU_DEPTH, 12);
        assert_eq!(MAX_MENU_NODES, 4_096);
    }
}
