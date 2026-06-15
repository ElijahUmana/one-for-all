//! Shadow DOM handling.
//!
//! Owned by `ax-engine`. Modern Chrome `DOMSnapshot.captureSnapshot` with
//! the default flags includes shadow roots in `documents[0].nodes` —
//! they're modeled as ordinary node children with a `shadowRootType`
//! attribute. Our `merge` walks the AX tree and joins by `backendNodeId`,
//! so shadow content is captured as long as Chromium reports it.
//!
//! Per SPEC §10 the v1 contract is: **open** shadow roots are walked
//! transparently; **closed** shadow roots surface as their host element
//! with no descendants — Chromium does not expose them to either the AX
//! tree or `DOMSnapshot`, so there's nothing to traverse. Web-Components
//! sites that rely heavily on `shadowRootMode: "closed"` may therefore
//! need a host-level interaction strategy (click the host, observe the
//! resulting AX changes via M2 deltas).
//!
//! Per-frame composition (the bigger lever for compound documents) is
//! handled in [`crate::iframe`]. This file exists so the merge algorithm
//! has a clear seam for shadow-specific extensions when needed.

use serde_json::Value;

/// Returns `true` if the given DOMSnapshot node descriptor names this as a
/// closed shadow root (`shadowRootType === "closed"`). `pub(crate)` —
/// internal seam for the merge algorithm, not a stable public API.
///
/// Currently no production caller in ax-engine gates on this; per SPEC §10
/// "Known limitations" closed shadow roots are opaque (Chromium does not
/// surface them to AX or DOMSnapshot), so there is nothing to walk. The
/// helper stays for forward-compatibility with future Chromium releases
/// that may expose closed-shadow content via a new flag — and the unit
/// test below pins the detection contract so the seam doesn't rot.
#[allow(dead_code)]
pub(crate) fn is_closed_shadow_root(node: &Value) -> bool {
    node.get("shadowRootType")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("closed"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_closed_shadow_root() {
        assert!(is_closed_shadow_root(&json!({"shadowRootType": "closed"})));
        assert!(!is_closed_shadow_root(&json!({"shadowRootType": "open"})));
        assert!(!is_closed_shadow_root(&json!({})));
    }
}
