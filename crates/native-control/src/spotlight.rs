//! SPEC §12 U6 — `app.spotlight.{open, query, select}`.
//!
//! Spotlight is owned by `Spotlight.app` (`com.apple.Spotlight`) and exposed
//! via the magnifying-glass status menu and the `⌘-Space` keystroke. The AX
//! tree is private (Apple does not expose stable IDs for the result list);
//! we therefore drive Spotlight through AppleScript where each verb is a
//! short focused-keystroke sequence sent to the system, plus a brief
//! [`tokio::time::sleep`] for the search engine to settle.
//!
//! Focus discipline: `spotlight.open` IS focus-stealing — opening Spotlight
//! moves the keystroke target away from the current app. Default-deny.
//! `spotlight.query` and `spotlight.select` operate on an already-open
//! Spotlight and so are not focus-stealing on their own.

#![cfg(target_os = "macos")]

use std::time::Duration;

use crate::types::NativeControlError;

/// Open the Spotlight bar. Focus-stealing — requires `confirm + focus_steal`.
pub async fn open(confirm: bool, focus_steal: bool) -> Result<(), NativeControlError> {
    if !(confirm && focus_steal) {
        return Err(NativeControlError::FocusStealForbidden {
            what: "app.spotlight.open",
        });
    }
    // Use a System Events keystroke. AppleScript: `tell application "System
    // Events" to key code 49 using {command down}` — keycode 49 = space.
    let script = "tell application \"System Events\" to key code 49 using {command down}";
    crate::actions::app_eval("com.apple.systemevents", script).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    Ok(())
}

/// Type a Spotlight query into an already-open Spotlight bar. Returns
/// `Internal` if no Spotlight bar is up. Caller is responsible for the open.
pub async fn query(text: &str) -> Result<(), NativeControlError> {
    if text.is_empty() {
        return Err(NativeControlError::Internal(
            "spotlight query text is empty".into(),
        ));
    }
    // Escape AppleScript quotes — backslash-quote inside a quoted string.
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("tell application \"System Events\" to keystroke \"{escaped}\"");
    crate::actions::app_eval("com.apple.systemevents", &script).await?;
    Ok(())
}

/// Select the Nth result in the Spotlight list (0-indexed) and press return.
/// `index = 0` selects the top hit.
pub async fn select(index: u32) -> Result<(), NativeControlError> {
    if index >= 64 {
        return Err(NativeControlError::Internal(
            "spotlight select index out of bounds".into(),
        ));
    }
    // Press the down-arrow `index` times then return.
    let mut script = String::from("tell application \"System Events\"\n");
    for _ in 0..index {
        script.push_str("    key code 125\n"); // down arrow
    }
    script.push_str("    key code 36\nend tell\n"); // return
    crate::actions::app_eval("com.apple.systemevents", &script).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_default_deny() {
        let r = open(false, true).await;
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.spotlight.open");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
        let r = open(true, false).await;
        match r {
            Err(NativeControlError::FocusStealForbidden { .. }) => {}
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_query_rejected() {
        let r = query("").await;
        match r {
            Err(NativeControlError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn out_of_range_index_rejected() {
        let r = select(64).await;
        match r {
            Err(NativeControlError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
