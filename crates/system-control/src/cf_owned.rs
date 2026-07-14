//! Tiny RAII wrapper for raw Core Foundation references not covered by the
//! typed `core-foundation` crate. Mirrors `native_control::cf_owned` byte-for-
//! byte; we keep a local copy because pulling a public dependency on
//! `native-control` would couple the AX surface to the system surface for no
//! engineering benefit (12 lines).

#![cfg(target_os = "macos")]

use core_foundation_sys::base::{CFRelease, CFTypeRef};

/// Owned `CFTypeRef`. Drops via `CFRelease`. Constructed only via
/// [`CfOwned::from_create`].
pub(crate) struct CfOwned<T> {
    raw: *const T,
}

impl<T> CfOwned<T> {
    /// Wrap a +1 ref obtained from a `Create*` / `Copy*` API.
    ///
    /// # Safety
    /// `raw` must be a non-null Core Foundation reference whose ownership
    /// has been transferred to us (Create Rule). Caller must NOT release.
    pub(crate) unsafe fn from_create(raw: *const T) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        self.raw
    }
}

impl<T> Drop for CfOwned<T> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: by construction we hold a +1 ref; CFRelease balances it.
            unsafe { CFRelease(self.raw as CFTypeRef) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::CFRetain;
    use core_foundation_sys::string::{__CFString, CFStringRef};

    #[test]
    fn from_create_handles_null() {
        let n = unsafe { CfOwned::<u8>::from_create(std::ptr::null()) };
        assert!(n.is_none());
    }

    #[test]
    fn drop_releases_real_cf_object() {
        let s = CFString::from_static_string("test");
        let ptr: CFStringRef = s.as_concrete_TypeRef();
        unsafe { CFRetain(ptr as _) };
        let owned = unsafe { CfOwned::<__CFString>::from_create(ptr) };
        assert!(owned.is_some());
        drop(owned);
        assert_eq!(s.to_string(), "test");
    }
}
