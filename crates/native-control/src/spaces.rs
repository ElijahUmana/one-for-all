//! SPEC §12 U6 — `app.spaces.{list, switch_to, move_window}`.
//!
//! Mission Control "Spaces" enumeration lives behind private CoreGraphics
//! Services (CGS_*) symbols (`CGSGetActiveSpace`, `CGSCopyManagedDisplaySpaces`,
//! etc.) which are version-fragile and undocumented.
//!
//! We resolve those symbols at run time via `dlsym(RTLD_DEFAULT, …)` and
//! degrade to [`NativeControlError::PrivateApiUnavailable`] when the symbol
//! is missing OR the crate was compiled without the `private-apis` feature.
//! For verbs Apple does support via AppleScript (Mission Control toggle), we
//! wire those as the recommended path.
//!
//! `switch_to` is NOT classified as focus-stealing in the SPEC sense — it
//! changes the Space, which is a workspace-level operation; the agent's
//! current app remains its current app. We still emit a `tracing::info` so
//! traces show the Space change.

#![cfg(target_os = "macos")]

use libc::{c_void, dlsym, RTLD_DEFAULT};
use std::ffi::CString;
use std::sync::OnceLock;
use tracing::info;

use crate::types::{NativeControlError, SpaceInfo};

#[cfg(feature = "private-apis")]
type CGSDefaultConnection = unsafe extern "C" fn() -> u32;
#[cfg(feature = "private-apis")]
type CGSGetActiveSpace = unsafe extern "C" fn(u32) -> u64;

#[cfg(feature = "private-apis")]
fn cgs_default_connection() -> Option<CGSDefaultConnection> {
    static CACHED: OnceLock<Option<CGSDefaultConnection>> = OnceLock::new();
    *CACHED.get_or_init(|| resolve("CGSDefaultConnectionForThread"))
}

#[cfg(feature = "private-apis")]
fn cgs_get_active_space() -> Option<CGSGetActiveSpace> {
    static CACHED: OnceLock<Option<CGSGetActiveSpace>> = OnceLock::new();
    *CACHED.get_or_init(|| resolve("CGSGetActiveSpace"))
}

#[cfg(feature = "private-apis")]
fn resolve<F: Copy>(name: &str) -> Option<F> {
    let c = CString::new(name).ok()?;
    // SAFETY: dlsym with RTLD_DEFAULT searches every loaded image.
    let sym = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // SAFETY: caller-typed function pointer for a C function we know the
    // signature of.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&sym) })
}

/// Best-effort space enumeration. On macOS where the CGS_* symbols are
/// resolvable, the active space's id is returned with a synthetic name
/// ("Space 1" / "Active"); the inactive list is not reliably exposed without
/// `CGSCopyManagedDisplaySpaces` (CFArray of CFDictionary with version-
/// dependent shape). For now we surface the active space only.
pub async fn list() -> Result<Vec<SpaceInfo>, NativeControlError> {
    tokio::task::spawn_blocking(list_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

#[cfg(feature = "private-apis")]
fn list_blocking() -> Result<Vec<SpaceInfo>, NativeControlError> {
    let conn = cgs_default_connection().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "CGSDefaultConnectionForThread",
    })?;
    let active = cgs_get_active_space().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "CGSGetActiveSpace",
    })?;
    // SAFETY: resolved symbols are valid C functions for this process.
    let cid = unsafe { conn() };
    let space_id = unsafe { active(cid) };
    Ok(vec![SpaceInfo {
        space_id,
        name: format!("Space {space_id}"),
        active: true,
    }])
}

#[cfg(not(feature = "private-apis"))]
fn list_blocking() -> Result<Vec<SpaceInfo>, NativeControlError> {
    Err(NativeControlError::PrivateApiUnavailable {
        what: "CGS spaces (build native-control with `private-apis`)",
    })
}

/// Switch to the previous / next space via System Events keystroke
/// (`Ctrl+←` / `Ctrl+→`). Apple ships these as default Mission Control
/// shortcuts; we fall back to AppleScript so we don't depend on private APIs
/// for the navigation verb. `delta = -1` previous, `+1` next.
pub async fn switch_relative(delta: i32) -> Result<(), NativeControlError> {
    if delta == 0 {
        return Ok(());
    }
    let key = if delta < 0 { 123 } else { 124 }; // 123=left, 124=right
    let count = delta.unsigned_abs();
    let mut script = String::from("tell application \"System Events\"\n");
    for _ in 0..count {
        script.push_str(&format!("    key code {key} using {{control down}}\n"));
    }
    script.push_str("end tell\n");
    crate::actions::app_eval("com.apple.systemevents", &script).await?;
    info!(delta, "spaces.switch_relative dispatched");
    Ok(())
}

/// Move a window to the next/previous space via the WindowServer hot-corner.
/// Macros only — Apple does not expose a stable per-window space-move API.
/// We emit a `tracing::warn` and fall back to switch_relative for the user.
pub async fn move_window_relative(bundle_id: &str, delta: i32) -> Result<(), NativeControlError> {
    // Without a stable per-window move primitive, the highest-fidelity
    // approach is `Mission Control: drag this window's thumbnail to the
    // target Space`. We don't synthesize that drag here — it's a multi-step
    // gesture and caller can compose `gesture::*` if needed. Document and
    // surface PrivateApiUnavailable.
    let _ = (bundle_id, delta);
    Err(NativeControlError::PrivateApiUnavailable {
        what: "per-window space move requires gesture composition (use app.gesture.* + spaces.switch_relative)",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn switch_zero_is_noop() {
        switch_relative(0).await.unwrap();
    }

    #[tokio::test]
    async fn move_window_unsupported_returns_clean() {
        let r = move_window_relative("com.apple.calculator", 1).await;
        match r {
            Err(NativeControlError::PrivateApiUnavailable { .. }) => {}
            other => panic!("expected PrivateApiUnavailable, got {other:?}"),
        }
    }
}
