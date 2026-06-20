//! Tiny RAII wrapper for Core Foundation references that the typed
//! `core-foundation` crate doesn't already cover.
//!
//! The `accessibility-sys` crate gives us raw `AXUIElementRef` (= `CFTypeRef`)
//! handles. They follow Core Foundation's "Get Rule"/"Create Rule" ownership
//! conventions: a `Copy*` function returns a +1 ref the caller must release.
//! Forgetting to release leaks; double-releasing crashes. Wrap every owned
//! ref in [`AxOwned`] so `Drop` does the `CFRelease`.

#![cfg(target_os = "macos")]

use core_foundation_sys::base::{CFRelease, CFRetain, CFTypeRef};

/// Owned `AXUIElementRef` (or any other CFTypeRef). Drops via `CFRelease`.
///
/// Always construct via [`AxOwned::from_create`] to make the ownership
/// transfer explicit at the FFI boundary.
pub(crate) struct AxOwned<T> {
    raw: *const T,
}

impl<T> AxOwned<T> {
    /// Wrap a +1 ref obtained from a `Create*` / `Copy*` API.
    ///
    /// # Safety
    /// `raw` must be a non-null Core Foundation reference whose ownership
    /// has been transferred to us (i.e. created with the Create Rule). The
    /// caller must NOT release it themselves.
    pub(crate) unsafe fn from_create(raw: *const T) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    /// Wrap a borrowed CF reference by first retaining it into an owned +1 ref.
    ///
    /// # Safety
    /// `raw` must point to a live Core Foundation object. We retain it before
    /// constructing the wrapper, so the caller keeps their original ownership.
    pub(crate) unsafe fn from_borrowed(raw: *const T) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            CFRetain(raw as CFTypeRef);
            Some(Self { raw })
        }
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        self.raw
    }
}

impl<T> Drop for AxOwned<T> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: we own a +1 ref by construction (`from_create`).
            // CFRelease balances it. After this returns the ref is gone;
            // we won't touch `self.raw` again because `Self` is being
            // dropped.
            unsafe { CFRelease(self.raw as CFTypeRef) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use core_foundation_sys::string::{__CFString, CFStringRef};

    #[test]
    fn from_create_handles_null() {
        // SAFETY: passing null returns None without dereferencing.
        let n = unsafe { AxOwned::<u8>::from_create(std::ptr::null()) };
        assert!(n.is_none());
    }

    #[test]
    fn from_borrowed_handles_null() {
        // SAFETY: passing null returns None without dereferencing.
        let n = unsafe { AxOwned::<u8>::from_borrowed(std::ptr::null()) };
        assert!(n.is_none());
    }

    #[test]
    fn drop_releases_real_cf_object() {
        // Build a real CFString, simulate a Create-Rule return by adding +1
        // via CFRetain, hand it to AxOwned. If our Drop is wrong this would
        // either leak (caught only by leaks tooling) or double-free (would
        // crash on the second drop).
        let s = CFString::from_static_string("test");
        let ptr: CFStringRef = s.as_concrete_TypeRef();
        // SAFETY: s is alive; CFRetain bumps ref count by 1; we hand that +1
        // to AxOwned so the count stays balanced when both s and owned drop.
        unsafe { core_foundation_sys::base::CFRetain(ptr as _) };
        let owned = unsafe { AxOwned::<__CFString>::from_create(ptr) };
        assert!(owned.is_some());
        drop(owned);
        // s is still alive here (we only added a balanced +1/-1).
        assert_eq!(s.to_string(), "test");
    }
}
