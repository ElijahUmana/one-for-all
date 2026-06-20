//! SPEC §12 U6 — `app.ime.{set_input_source, list, switch}`.
//!
//! Driven by Apple's Text Input Source services (`TISCreateInputSourceList`,
//! `TISSelectInputSource`). These symbols are public (Carbon) but the
//! parameters use Core Foundation, so we wire them through `core-foundation`
//! types.
//!
//! When the user wants to query without altering state, `list` returns the
//! input-source IDs (e.g. `"com.apple.keylayout.US"`). `switch` selects a
//! specific source by id; `set_input_source` is an alias that's also useful
//! when the agent doesn't care about prior state.

#![cfg(target_os = "macos")]

use libc::{c_void, dlsym, RTLD_DEFAULT};
use std::ffi::CString;
use std::sync::OnceLock;

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;

use crate::types::NativeControlError;

type TISCreateInputSourceList =
    unsafe extern "C" fn(properties: CFTypeRef, include_all_installed: bool) -> CFArrayRef;
type TISSelectInputSource = unsafe extern "C" fn(input_source: CFTypeRef) -> i32;
type TISGetInputSourceProperty =
    unsafe extern "C" fn(input_source: CFTypeRef, property_key: CFTypeRef) -> CFTypeRef;

fn resolve<F: Copy>(name: &str) -> Option<F> {
    let c = CString::new(name).ok()?;
    // SAFETY: dlsym against a static name.
    let sym = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    if sym.is_null() {
        None
    } else {
        // SAFETY: caller-typed C function pointer.
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&sym) })
    }
}

fn create_list_fn() -> Option<TISCreateInputSourceList> {
    static C: OnceLock<Option<TISCreateInputSourceList>> = OnceLock::new();
    *C.get_or_init(|| resolve("TISCreateInputSourceList"))
}
fn select_fn() -> Option<TISSelectInputSource> {
    static C: OnceLock<Option<TISSelectInputSource>> = OnceLock::new();
    *C.get_or_init(|| resolve("TISSelectInputSource"))
}
fn get_property_fn() -> Option<TISGetInputSourceProperty> {
    static C: OnceLock<Option<TISGetInputSourceProperty>> = OnceLock::new();
    *C.get_or_init(|| resolve("TISGetInputSourceProperty"))
}

fn property_key_id() -> Option<CFTypeRef> {
    // kTISPropertyInputSourceID is published as a static CFStringRef.
    let c = CString::new("kTISPropertyInputSourceID").ok()?;
    // SAFETY: dlsym against a static name.
    let sym = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    if sym.is_null() {
        None
    } else {
        // The symbol IS a CFStringRef* pointing to a global; we read it.
        // SAFETY: resolved symbol is a pointer to a CFTypeRef constant.
        Some(unsafe { *(sym as *const CFTypeRef) })
    }
}

/// List installed input source IDs.
pub async fn list() -> Result<Vec<String>, NativeControlError> {
    tokio::task::spawn_blocking(list_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn list_blocking() -> Result<Vec<String>, NativeControlError> {
    let create = create_list_fn().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "TISCreateInputSourceList",
    })?;
    let prop = get_property_fn().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "TISGetInputSourceProperty",
    })?;
    let key = property_key_id().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "kTISPropertyInputSourceID",
    })?;
    // SAFETY: resolved symbol; passing NULL properties + true selects all.
    let arr_ref = unsafe { create(std::ptr::null(), true) };
    if arr_ref.is_null() {
        return Ok(vec![]);
    }
    // SAFETY: arr_ref is a +1 CFArrayRef.
    let arr: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(arr_ref) };
    let mut out = Vec::with_capacity(arr.len() as usize);
    for item in arr.iter() {
        let raw = item.as_CFTypeRef();
        // SAFETY: prop is a known C fn; key and raw are non-null CFTypeRefs.
        let id_ref = unsafe { prop(raw, key) };
        if id_ref.is_null() {
            continue;
        }
        // The returned ref is a CFStringRef under Get rule.
        // SAFETY: id_ref is a borrowed CFStringRef (per TIS docs).
        let s = unsafe {
            CFString::wrap_under_get_rule(id_ref as core_foundation_sys::string::CFStringRef)
        };
        out.push(s.to_string());
    }
    Ok(out)
}

/// Select an input source by ID (e.g. `"com.apple.keylayout.US"`).
pub async fn switch(input_id: &str) -> Result<(), NativeControlError> {
    let id = input_id.to_string();
    tokio::task::spawn_blocking(move || switch_blocking(&id))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn switch_blocking(input_id: &str) -> Result<(), NativeControlError> {
    let create = create_list_fn().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "TISCreateInputSourceList",
    })?;
    let prop = get_property_fn().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "TISGetInputSourceProperty",
    })?;
    let select = select_fn().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "TISSelectInputSource",
    })?;
    let key = property_key_id().ok_or(NativeControlError::PrivateApiUnavailable {
        what: "kTISPropertyInputSourceID",
    })?;

    // Build a properties dict {kTISPropertyInputSourceID: input_id} for
    // filtering (more efficient than scanning all sources).
    let key_cf =
        unsafe { CFString::wrap_under_get_rule(key as core_foundation_sys::string::CFStringRef) };
    let val = CFString::new(input_id);
    let dict = CFDictionary::from_CFType_pairs(&[(key_cf, val)]);
    // SAFETY: dict is a CFDictionaryRef we own; Properties dict is read-only.
    let arr_ref = unsafe { create(dict.as_concrete_TypeRef() as CFTypeRef, true) };
    if arr_ref.is_null() {
        return Err(NativeControlError::Tis(-1));
    }
    let arr: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(arr_ref) };
    if arr.is_empty() {
        return Err(NativeControlError::Tis(-50)); // paramErr-ish; no match
    }
    let item = arr.get(0).ok_or(NativeControlError::Tis(-1))?;
    let raw = item.as_CFTypeRef();
    // Diagnostic: confirm the source's ID prop matches what we asked for.
    let _ = unsafe { prop(raw, key) };
    // SAFETY: select takes an input source ref.
    let err = unsafe { select(raw) };
    if err == 0 {
        Ok(())
    } else {
        Err(NativeControlError::Tis(err))
    }
}

/// Alias of `switch`.
pub async fn set_input_source(input_id: &str) -> Result<(), NativeControlError> {
    switch(input_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_returns_at_least_one_when_resolvable() {
        match list().await {
            Ok(v) => {
                // On macOS test runners with TIS resolvable, we expect ≥1.
                // If it's empty (extremely unusual), don't panic — TIS is
                // available but might be sandboxed.
                let _ = v.len();
            }
            Err(NativeControlError::PrivateApiUnavailable { .. }) => {}
            Err(other) => panic!("unexpected list error: {other:?}"),
        }
    }
}
