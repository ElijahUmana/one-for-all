//! Non-macOS stub. Focus stealing is not a concern on headless Linux/Windows;
//! the high-level API still works (returns `None` / `false` everywhere) and
//! Layer E (`set_accessory_activation_policy`) is a no-op.

#![cfg(not(target_os = "macos"))]

use thiserror::Error;

#[allow(dead_code)]
pub fn frontmost_app() -> Option<i32> {
    None
}

#[allow(dead_code)]
pub fn activate_pid(_pid: i32) -> bool {
    false
}

#[allow(dead_code)]
pub fn is_running(_pid: i32) -> bool {
    false
}

/// Stub mirror of the macOS `AccessoryPolicyError`. The non-macOS arm of
/// `set_accessory_activation_policy` is infallible, so this enum is empty
/// and uninhabited — but it's still re-exported so cross-platform callers
/// can name the type without `#[cfg]` gates.
#[derive(Debug, Error)]
pub enum AccessoryPolicyError {}

/// Stub mirror of the macOS `AccessoryPolicyGuard`. Holds no state on
/// non-macOS targets but exists so `main` can bind the result for the
/// process lifetime without conditional compilation at the call site.
pub struct AccessoryPolicyGuard {
    _private: (),
}

/// Non-macOS no-op. There is no Dock and no `NSApplication` to register
/// against; Layer E is a macOS concept. Returns `Ok` with an inert guard
/// so cross-platform `main` code stays straight-line.
#[allow(dead_code)]
pub fn set_accessory_activation_policy() -> Result<AccessoryPolicyGuard, AccessoryPolicyError> {
    Ok(AccessoryPolicyGuard { _private: () })
}
