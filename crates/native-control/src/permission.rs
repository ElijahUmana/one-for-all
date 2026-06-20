//! `AXIsProcessTrusted` probe + first-run prompt + System Settings deeplink.
//!
//! The broker (and the `--check-ax` doctor flag in `broker/src/main.rs`)
//! consult these three primitives to decide:
//!  - whether to allow `app.*` calls at all,
//!  - whether to surface the OS prompt the first time, and
//!  - what URL to hand the user when they need to grant access.

use crate::types::NativeControlError;

/// SPEC §5: System Settings deeplink for the Privacy & Security ▸ Accessibility
/// pane. Open this URL with `open(1)` and the user lands on the right list.
///
/// Verified URI scheme on macOS 12+ (Monterey) through 15 (Sequoia). Earlier
/// versions used `x-apple.systempreferences:com.apple.preference.security`
/// without the anchor; the anchor is ignored when unsupported.
pub const SETTINGS_DEEPLINK: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// SPEC §12 — Notes / QuickLook AX text content requires Screen Recording
/// permission (Apple TCC quirk). Deeplink jumps to the Screen Recording pane.
pub const SCREEN_RECORDING_DEEPLINK: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// True if this process is currently trusted to use the Accessibility API.
///
/// Wraps `AXIsProcessTrusted()`. Cheap (~microseconds) — safe to call on
/// every `app.*` request as a fast gate.
#[cfg(target_os = "macos")]
pub fn is_trusted() -> bool {
    // SAFETY: `AXIsProcessTrusted` takes no arguments and returns a Boolean.
    // No memory ownership, no callback, no thread requirement. Safe to call
    // from any thread per Apple docs.
    unsafe { accessibility_sys::AXIsProcessTrusted() }
}

#[cfg(not(target_os = "macos"))]
pub fn is_trusted() -> bool {
    false
}

/// Probe trust and, if missing, ask the OS to display the standard "grant
/// Accessibility permission" prompt. Returns `Ok(())` if currently trusted,
/// `Err(PermissionMissing { settings_url })` otherwise.
///
/// The prompt is shown at most once per process lifetime by macOS; subsequent
/// calls when still untrusted are silent — that's why `install.sh` invokes
/// this and then keeps going (non-fatal warn) rather than blocking.
#[cfg(target_os = "macos")]
pub fn ensure_trusted_with_prompt() -> Result<(), NativeControlError> {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    // Build `{ kAXTrustedCheckOptionPrompt: kCFBooleanTrue }` as a CFDictionary.
    // SAFETY: `kAXTrustedCheckOptionPrompt` is a `CFStringRef` published as a
    // static by HIServices. We convert it to an owned CFString via
    // `wrap_under_get_rule` (retain).
    let key =
        unsafe { CFString::wrap_under_get_rule(accessibility_sys::kAXTrustedCheckOptionPrompt) };
    let value = CFBoolean::true_value();
    let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);

    // SAFETY: opts is a CFDictionaryRef we own; the function reads it and
    // returns a Boolean. No ownership transfer.
    let trusted =
        unsafe { accessibility_sys::AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) };
    if trusted {
        Ok(())
    } else {
        Err(NativeControlError::PermissionMissing {
            settings_url: SETTINGS_DEEPLINK,
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_trusted_with_prompt() -> Result<(), NativeControlError> {
    Err(NativeControlError::UnsupportedPlatform)
}

/// Convenience accessor for the deeplink. Forced through a function so the
/// JSON serializer in the broker doesn't reach into a `pub const` directly.
pub fn settings_deeplink() -> &'static str {
    SETTINGS_DEEPLINK
}

pub fn screen_recording_deeplink() -> &'static str {
    SCREEN_RECORDING_DEEPLINK
}

/// SPEC §12 — probe whether the broker has Screen Recording permission. Notes
/// and QuickLook expose AX text content only when this is granted (Apple TCC
/// quirk). We use `CGPreflightScreenCaptureAccess` (macOS 11+) which does NOT
/// trigger the consent prompt — purely a non-disruptive probe.
///
/// On macOS versions older than 11 the symbol is absent; we resolve via
/// `dlsym` and return `false` if unavailable, matching Apple's documented
/// "treat as not-granted" fallback.
#[cfg(target_os = "macos")]
pub fn is_screen_recording_granted() -> bool {
    use libc::{c_void, dlsym, RTLD_DEFAULT};
    use std::ffi::CString;
    use std::sync::OnceLock;

    type CGPreflight = unsafe extern "C" fn() -> bool;
    static CACHED: OnceLock<Option<CGPreflight>> = OnceLock::new();

    let f = CACHED.get_or_init(|| {
        // SAFETY: CString from a static literal is always valid; dlsym with
        // RTLD_DEFAULT searches loaded images. Returns NULL on miss, which
        // we explicitly handle.
        let name = match CString::new("CGPreflightScreenCaptureAccess") {
            Ok(c) => c,
            Err(_) => return None,
        };
        let sym = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
        if sym.is_null() {
            None
        } else {
            // SAFETY: dlsym returned non-null for the named C function whose
            // signature we know. The transmute is the FFI standard pattern.
            let f: CGPreflight = unsafe { std::mem::transmute::<*mut c_void, CGPreflight>(sym) };
            Some(f)
        }
    });
    match f {
        // SAFETY: f is a valid C function pointer we resolved above.
        Some(probe) => unsafe { probe() },
        None => false,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn is_screen_recording_granted() -> bool {
    false
}

/// Same shape as [`ensure_trusted_with_prompt`] but for the Screen Recording
/// pane. There's no public Apple API to trigger the prompt without actually
/// capturing — so we do not synthesize a prompt; we just probe.
pub fn ensure_screen_recording_granted() -> Result<(), NativeControlError> {
    if is_screen_recording_granted() {
        Ok(())
    } else {
        Err(NativeControlError::ScreenRecordingMissing {
            settings_url: SCREEN_RECORDING_DEEPLINK,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_deeplink_format_stable() {
        // Locked: any change here would break installer/doctor.sh and broker
        // error data payloads. Update both if you change the URI.
        assert_eq!(
            SETTINGS_DEEPLINK,
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        );
        assert_eq!(settings_deeplink(), SETTINGS_DEEPLINK);
        // The URI scheme must be the macOS deeplink form, not http(s).
        assert!(SETTINGS_DEEPLINK.starts_with("x-apple.systempreferences:"));
        assert!(SETTINGS_DEEPLINK.contains("Privacy_Accessibility"));
    }

    #[test]
    fn is_trusted_returns_bool_without_panicking() {
        // We don't assert true/false — that depends on the run environment.
        // The point is the FFI links and returns cleanly.
        let _ = is_trusted();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_stub_reports_untrusted() {
        assert!(!is_trusted());
        match ensure_trusted_with_prompt() {
            Err(NativeControlError::UnsupportedPlatform) => {}
            other => panic!("expected UnsupportedPlatform, got {other:?}"),
        }
    }
}
