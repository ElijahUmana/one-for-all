//! SPEC §12 U6 — `app.touchbar.tap`.
//!
//! The macOS Touch Bar lives behind private framework
//! `/System/Library/PrivateFrameworks/DFRFoundation.framework`. The relevant
//! symbol is `DFRElementSetControlStripPresetIdentifier`, plus
//! `DFRSetStatus(2)` to force the simulator on for non-Touch-Bar Macs.
//!
//! We do not link the framework at compile time — we resolve via `dlopen` +
//! `dlsym` at runtime. If the framework is absent (Apple Silicon Mac with
//! external display, or macOS without the Touch Bar simulator), every entry
//! point degrades to [`NativeControlError::PrivateApiUnavailable`].

#![cfg(target_os = "macos")]

#[cfg(feature = "private-apis")]
use libc::{c_char, c_int, c_void, dlopen, dlsym, RTLD_LAZY};
#[cfg(feature = "private-apis")]
use std::ffi::{CStr, CString};
#[cfg(feature = "private-apis")]
use std::sync::OnceLock;

use crate::types::NativeControlError;

#[cfg(feature = "private-apis")]
type DFRSetStatus = unsafe extern "C" fn(c_int);
#[cfg(feature = "private-apis")]
type DFRElementSetControlStripPresetIdentifier = unsafe extern "C" fn(*const c_char) -> bool;

#[cfg(feature = "private-apis")]
fn lib_handle() -> Option<*mut c_void> {
    static H: OnceLock<usize> = OnceLock::new();
    let raw = *H.get_or_init(|| {
        let Ok(c) =
            CString::new("/System/Library/PrivateFrameworks/DFRFoundation.framework/DFRFoundation")
        else {
            return 0;
        };
        // SAFETY: dlopen with RTLD_LAZY against a string we control.
        let h = unsafe { dlopen(c.as_ptr(), RTLD_LAZY) };
        h as usize
    });
    if raw == 0 {
        None
    } else {
        Some(raw as *mut c_void)
    }
}

#[cfg(feature = "private-apis")]
fn resolve<F: Copy>(name: &str) -> Option<F> {
    let h = lib_handle()?;
    let c = CString::new(name).ok()?;
    // SAFETY: dlsym on a valid handle with a valid CString name.
    let sym = unsafe { dlsym(h, c.as_ptr()) };
    if sym.is_null() {
        return None;
    }
    // SAFETY: we know the symbol's C signature.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&sym) })
}

/// Tap a control-strip identifier on the Touch Bar (e.g. `"com.apple.system.brightness.up"`).
/// Returns [`NativeControlError::PrivateApiUnavailable`] when DFRFoundation is
/// missing or unloadable.
pub async fn tap(identifier: &str) -> Result<(), NativeControlError> {
    let id = identifier.to_string();
    tokio::task::spawn_blocking(move || tap_blocking(&id))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

#[cfg(feature = "private-apis")]
fn tap_blocking(id: &str) -> Result<(), NativeControlError> {
    let set_status: DFRSetStatus =
        resolve("DFRSetStatus").ok_or(NativeControlError::PrivateApiUnavailable {
            what: "DFRSetStatus",
        })?;
    let set_preset: DFRElementSetControlStripPresetIdentifier = resolve(
        "DFRElementSetControlStripPresetIdentifier",
    )
    .ok_or(NativeControlError::PrivateApiUnavailable {
        what: "DFRElementSetControlStripPresetIdentifier",
    })?;
    let c_id = CString::new(id)
        .map_err(|_| NativeControlError::Internal("identifier had embedded NUL".into()))?;
    // SAFETY: resolved symbols with known C signatures.
    unsafe {
        set_status(2);
        let ok = set_preset(c_id.as_ptr());
        // Logging only — re-confirm the identifier just for diagnostics.
        let _ = CStr::from_ptr(c_id.as_ptr());
        if !ok {
            return Err(NativeControlError::PrivateApiUnavailable {
                what: "DFRElementSetControlStripPresetIdentifier returned false",
            });
        }
    }
    Ok(())
}

#[cfg(not(feature = "private-apis"))]
fn tap_blocking(id: &str) -> Result<(), NativeControlError> {
    let _ = id;
    Err(NativeControlError::PrivateApiUnavailable {
        what: "Touch Bar (build native-control with `private-apis`)",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tap_handles_clean_failure() {
        // We don't assert success — most test runners are headless and don't
        // have DFRFoundation. The point is the call doesn't panic.
        let _ = tap("com.apple.system.brightness.up").await;
    }
}
