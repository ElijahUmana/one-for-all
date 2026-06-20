//! `native-control` — universal control surface for native macOS apps via
//! the system Accessibility API.
//!
//! SPEC §11 V2 + §12 U6/U7: every surface (browser, native macOS, terminal,
//! future Linux/Windows GUI) exposes the same primitive shape — `snapshot`
//! (read state), `act` (dispatch input), `subscribe` (event stream). This
//! crate is the native-macOS limb. It mirrors `browser_engine`'s
//! `Page::snapshot` / `Page::click` / `Page::type_text` / `Page::scroll` /
//! `Page::eval` against `AXUIElement*` from `ApplicationServices/HIServices`
//! instead of CDP, then expands to the deep-input surfaces of SPEC §12 (menu
//! bar, status menu, notification center, Spotlight, Spaces, Dock, Window
//! manipulation, TouchBar, gestures, IME, scripting, QuickLook, Clipboard,
//! Drag, AX events).
//!
//! # Public surface
//!
//! - [`AppController`] — per-session snapshot cache + ref resolver +
//!   subscriber registry. One per `broker::SessionEntry`.
//! - [`list_apps`] — enumerate running apps with bundle id, pid, name, focus
//!   flag (mirrors `tab.list`).
//! - [`AppController::snapshot`] — depth-first AX walk; returns
//!   `AppSnapshot { snapshot_seq, app_id, elements, … }`. Refs scoped to
//!   `(app_id, snapshot_seq)`; using a stale ref on a later action returns
//!   [`NativeControlError::RefStale`].
//! - [`AppController::click`] / `type_text` / `scroll` — dispatch input via
//!   `AXUIElementPerformAction` / `AXUIElementSetAttributeValue` /
//!   `CGEventPostToPid`. **None of these activate the target app** — we never
//!   call `kAXRaiseAction` or `[NSRunningApplication activate*]` against
//!   anyone (SPEC §5 forbidden APIs, extended for V2).
//! - [`app_eval`] — AppleScript bridge. Bodies that contain `activate` against
//!   the target are rejected to keep the focus-no-steal invariant intact.
//! - [`permission`] — `AXIsProcessTrusted` probe + first-run prompt + System
//!   Settings deeplink for the failure path. Also probes Screen Recording for
//!   the Notes/QuickLook AX-text caveat.
//! - [`privacy`] — `RedactionEngine` with regex `redact_patterns` +
//!   substring-any `app_blocklist`. Per-session, hot-reloadable, never panics
//!   on bad regex.
//! - [`menu`] / [`statusmenu`] / [`dock`] / [`window`] / [`spotlight`] /
//!   [`spaces`] / [`notification_center`] / [`quicklook`] / [`gesture`] /
//!   [`touchbar`] / [`ime`] / [`scripting`] / [`clipboard`] / [`drag`] —
//!   one module per SPEC §12 U6/U7 surface.
//! - [`subscribe`] — `app.subscribe` AX event stream via `AXObserver` +
//!   `CFRunLoop`. Bounded mpsc(1024, drop-oldest); cleaned up on Drop.
//!
//! # Cross-platform stub
//!
//! On non-macOS targets every public function returns
//! [`NativeControlError::UnsupportedPlatform`]. The crate compiles on Linux/
//! Windows so the broker workspace builds end-to-end on CI; AX-touching tests
//! gate themselves on `cfg(target_os = "macos")`.

#![deny(rust_2018_idioms)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod controller;
pub mod permission;
pub mod privacy;
pub mod types;

#[cfg(target_os = "macos")]
pub mod actions;
#[cfg(target_os = "macos")]
pub mod ax_walk;
#[cfg(target_os = "macos")]
mod cf_owned;

// SPEC §12 U6/U7/AX events — one module per surface. macOS-only; the
// non-macOS stub below returns `UnsupportedPlatform` for every call.
#[cfg(target_os = "macos")]
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod dock;
#[cfg(target_os = "macos")]
pub mod drag;
#[cfg(target_os = "macos")]
pub mod gesture;
#[cfg(target_os = "macos")]
pub mod ime;
#[cfg(target_os = "macos")]
pub mod menu;
#[cfg(target_os = "macos")]
pub mod notification_center;
#[cfg(target_os = "macos")]
pub mod quicklook;
#[cfg(target_os = "macos")]
pub mod scripting;
#[cfg(target_os = "macos")]
pub mod spaces;
#[cfg(target_os = "macos")]
pub mod spotlight;
#[cfg(target_os = "macos")]
pub mod statusmenu;
#[cfg(target_os = "macos")]
pub mod subscribe;
#[cfg(target_os = "macos")]
pub mod touchbar;
#[cfg(target_os = "macos")]
pub mod window;

#[cfg(not(target_os = "macos"))]
mod stub;

pub use controller::AppController;
pub use privacy::RedactionEngine;
pub use types::{
    AppElement, AppHandle, AppId, AppSnapshot, AxEvent, AxEventTopic, AxSubscription, BBox,
    ClipboardItem, ClipboardKind, DockItem, ElementState, MenuItem, NativeControlError,
    PrivacyPolicy, SpaceInfo, WindowHandle,
};

/// SPEC §11 V2 — list currently running native applications.
///
/// Returns `Vec<AppHandle>` in macOS `NSWorkspace::runningApplications` order
/// (NOT sorted — caller sorts if it wants stability across calls). Includes
/// only apps with a bundle id; daemons and helpers without one are filtered.
///
/// macOS-only. On other platforms returns [`NativeControlError::UnsupportedPlatform`].
pub async fn list_apps() -> Result<Vec<AppHandle>, NativeControlError> {
    #[cfg(target_os = "macos")]
    {
        actions::list_apps().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(NativeControlError::UnsupportedPlatform)
    }
}

/// SPEC §11 V2 — execute an AppleScript snippet via `osascript` against the
/// target bundle id (or raw script if it already begins with `tell`).
///
/// `tell application … to activate` strings are rejected to preserve the
/// SPEC §5 focus-no-steal invariant against the target app.
pub async fn app_eval(
    bundle_id: &str,
    applescript: &str,
) -> Result<serde_json::Value, NativeControlError> {
    #[cfg(target_os = "macos")]
    {
        actions::app_eval(bundle_id, applescript).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (bundle_id, applescript);
        Err(NativeControlError::UnsupportedPlatform)
    }
}
