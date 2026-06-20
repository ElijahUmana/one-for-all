//! Non-macOS placeholder keeping the workspace compilable on Linux/Windows
//! CI runners. All public functions return
//! [`crate::types::NativeControlError::UnsupportedPlatform`].

#![cfg(not(target_os = "macos"))]
#![allow(dead_code)]

// Intentionally empty — `lib.rs` already cfg-gates the implementations. This
// module only exists so the crate has SOMETHING to compile on non-macOS, even
// when macOS-only modules are excluded.
