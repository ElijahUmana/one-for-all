//! MutationObserver-driven AX deltas (SPEC §10 M2).
//!
//! Owned by `ax-engine`. Two pure pieces of glue:
//!
//! 1. [`OBSERVER_BOOTSTRAP_JS`] — a JS string registered via
//!    `Page.addScriptToEvaluateOnNewDocument` so every new document gets
//!    a `window.__oneForAllMutationLog` array that a `MutationObserver`
//!    pushes records into. Idempotent: re-installs are no-ops.
//! 2. [`install_observer`] / [`drain_log`] — async helpers that run those
//!    bootstrap and drain steps via [`cdp_client::CdpSession`].
//!
//! The drained log is a `Vec<MutationRecord>` whose `backend_node_id`s feed
//! the snapshot delta filter in [`crate::merge`].

use anyhow::{Context, Result};
use cdp_client::CdpSession;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;

/// JS bootstrap. Registered once per page via
/// `Page.addScriptToEvaluateOnNewDocument`. Idempotent guard:
/// re-running after a top-level navigation just re-arms the observer.
///
/// Closed shadow roots: per SPEC §10 v1 contract, content inside a
/// `shadowRootMode: "closed"` is opaque to AX and DOMSnapshot, so the
/// only thing the agent can interact with is the **host element**. The
/// `climbToShadowHost` helper walks `target.getRootNode()` and, when
/// the root is a closed `ShadowRoot`, reports the host's `tagName` /
/// `data-ofa-node` instead of the shadow-internal target. Open shadow
/// roots are walked transparently and continue to surface their
/// internal targets unchanged.
pub const OBSERVER_BOOTSTRAP_JS: &str = r#"
(function () {
  if (window.__oneForAllMutationInstalled) return;
  window.__oneForAllMutationInstalled = true;
  window.__oneForAllMutationLog = [];
  window.__oneForAllMutationSeq = 0;
  var MAX_LOG = 4096;
  function climbToShadowHost(node) {
    // Walk getRootNode() chains; if we land in a closed shadow root,
    // report the host instead. SPEC §10 closed-shadow contract: the
    // agent can only interact with the host, so deltas should reference
    // the host's identity, not the opaque internal target.
    try {
      var cur = node;
      var guard = 0;
      while (cur && guard++ < 16) {
        var root = (cur.getRootNode && cur.getRootNode()) || null;
        if (!root) return cur;
        // ShadowRoot has a `mode` property ("open" | "closed") and a `host`.
        if (root.host && typeof root.mode === 'string') {
          if (root.mode === 'closed') {
            // Climb out of the closed shadow; keep climbing in case the
            // host itself is inside another closed shadow.
            cur = root.host;
            continue;
          }
          // Open shadow — surface the original target.
          return node;
        }
        // Document or DocumentFragment without a host — done climbing.
        return cur;
      }
      return cur;
    } catch (e) {
      return node;
    }
  }
  function record(kind, target) {
    if (!target) return;
    var t = climbToShadowHost(target);
    var seq = ++window.__oneForAllMutationSeq;
    window.__oneForAllMutationLog.push({
      seq: seq,
      kind: kind,
      // backendNodeId is not directly exposed to JS — broker resolves
      // backend ids via CDP. We push a stable internal node ref via a
      // weakly-attached `data-ofa-node` attribute when an Element is targeted.
      tag: (t && t.tagName) ? t.tagName : '',
      cb: (t && t.dataset && t.dataset.cbNode) || ''
    });
    if (window.__oneForAllMutationLog.length > MAX_LOG) {
      window.__oneForAllMutationLog.splice(
        0, window.__oneForAllMutationLog.length - MAX_LOG);
    }
  }
  var obs = new MutationObserver(function (records) {
    for (var i = 0; i < records.length; i++) {
      var r = records[i];
      record(r.type, r.target);
      if (r.type === 'childList') {
        for (var j = 0; j < r.addedNodes.length; j++) record('added', r.addedNodes[j]);
        for (var k = 0; k < r.removedNodes.length; k++) record('removed', r.removedNodes[k]);
      }
    }
  });
  function arm() {
    try {
      obs.observe(document, {
        subtree: true, childList: true, attributes: true, characterData: true
      });
    } catch (e) { /* document not ready yet */ }
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', arm);
  } else {
    arm();
  }
  window.__oneForAllMutationDrain = function () {
    var out = window.__oneForAllMutationLog;
    window.__oneForAllMutationLog = [];
    return out;
  };
})();
"#;

/// One record drained from `window.__oneForAllMutationLog`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MutationRecord {
    /// Monotonic in-page sequence number; lets the broker order records
    /// across drains without trusting wall-clock.
    pub seq: u64,
    /// `"attributes" | "childList" | "characterData" | "added" | "removed"`.
    pub kind: String,
    /// Target's `tagName`. Empty for non-element targets.
    pub tag: String,
    /// Optional `data-ofa-node` annotation set by the engine on elements it
    /// has resolved to backend ids; empty otherwise.
    pub cb: String,
}

/// Install the MutationObserver bootstrap on every new document for this
/// session, idempotently. Returns the registered script identifier so
/// callers can remove it on shutdown.
pub async fn install_observer(session: &CdpSession) -> Result<String> {
    let res = session
        .send_raw(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": OBSERVER_BOOTSTRAP_JS }),
        )
        .await
        .context("Page.addScriptToEvaluateOnNewDocument")?;
    let id = res
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Also evaluate the bootstrap on the *current* document — the
    // addScriptToEvaluateOnNewDocument hook only fires on new doc creation.
    let _ = session
        .send_raw(
            "Runtime.evaluate",
            json!({
                "expression": OBSERVER_BOOTSTRAP_JS,
                "awaitPromise": false,
                "returnByValue": true,
            }),
        )
        .await;
    Ok(id)
}

/// Drain `window.__oneForAllMutationLog`. Returns an empty Vec if the
/// observer hasn't been installed (or the page navigated and reset it).
///
/// Failure modes are made explicit via [`MutationError`] so callers can
/// distinguish "no mutations recorded" (an empty `Ok(vec![])`) from
/// "drain blew up" — the latter must trigger a full-snapshot fallback,
/// not silently strand the caller on a stale anchor.
pub async fn drain_log(session: &CdpSession) -> Result<Vec<MutationRecord>, MutationError> {
    let res = session
        .send_raw(
            "Runtime.evaluate",
            json!({
                "expression":
                    "(window.__oneForAllMutationDrain ? window.__oneForAllMutationDrain() : [])",
                "returnByValue": true,
                "awaitPromise": false,
            }),
        )
        .await
        .map_err(|e| {
            MutationError::Transport(anyhow::Error::from(e).context("Runtime.evaluate drain"))
        })?;
    // The JS `(... || [])` guarantees an array shape, so a missing
    // `result.value` is unexpected — but treat it as "no mutations
    // recorded" rather than a hard error: the most common cause is the
    // observer not yet installed on a fresh document, which is a benign
    // transient state.
    let val = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let records: Vec<MutationRecord> =
        serde_json::from_value(val).map_err(MutationError::ParseError)?;
    Ok(records)
}

/// Failure modes for [`drain_log`]. SPA pages can clobber
/// `window.__oneForAllMutationDrain` (e.g. a sandbox that recreates
/// the global object); the resulting parse failure used to be silently
/// swallowed as `Ok(vec![])`, leaving the snapshot caller stranded on a
/// stale delta anchor. Surfacing the error lets the snapshot path fall
/// back to a full snapshot and bump a metric so operators see the rate.
#[derive(Debug)]
pub enum MutationError {
    /// `Runtime.evaluate` round-trip failed (CDP transport / connection).
    Transport(anyhow::Error),
    /// The drain JS returned a value we couldn't parse as
    /// `Vec<MutationRecord>` — most often because an SPA overwrote
    /// `window.__oneForAllMutationDrain` with something incompatible.
    ParseError(serde_json::Error),
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "mutation drain transport error: {e}"),
            Self::ParseError(e) => write!(f, "mutation drain parse error: {e}"),
        }
    }
}

impl std::error::Error for MutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e.as_ref()),
            Self::ParseError(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_js_is_idempotent_guarded() {
        // Sanity: the install guard line must precede any state setup.
        let pos_guard = OBSERVER_BOOTSTRAP_JS
            .find("__oneForAllMutationInstalled")
            .expect("guard present");
        let pos_log_init = OBSERVER_BOOTSTRAP_JS
            .find("__oneForAllMutationLog = []")
            .expect("log init present");
        assert!(pos_guard < pos_log_init);
    }

    #[test]
    fn bootstrap_js_exposes_drain_function() {
        assert!(OBSERVER_BOOTSTRAP_JS.contains("__oneForAllMutationDrain"));
    }

    #[test]
    fn mutation_record_round_trips_json() {
        let r = MutationRecord {
            seq: 7,
            kind: "added".into(),
            tag: "DIV".into(),
            cb: "n42".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: MutationRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.kind, "added");
    }

    #[test]
    fn mutation_error_display_distinguishes_variants() {
        let parse_err = serde_json::from_str::<Vec<MutationRecord>>("not json").unwrap_err();
        let pe = MutationError::ParseError(parse_err);
        let pe_msg = format!("{pe}");
        assert!(pe_msg.contains("parse"), "got {pe_msg}");

        let te = MutationError::Transport(anyhow::anyhow!("websocket closed"));
        let te_msg = format!("{te}");
        assert!(te_msg.contains("transport"), "got {te_msg}");
        assert!(te_msg.contains("websocket"), "got {te_msg}");
    }

    #[test]
    fn mutation_error_source_chain_is_walkable() {
        // The fallback path in `browser-engine` logs the error; verify
        // `Error::source()` exposes the underlying serde / anyhow cause so
        // the warn line is actionable.
        use std::error::Error as _;
        let parse_err = serde_json::from_str::<Vec<MutationRecord>>("not json").unwrap_err();
        let pe = MutationError::ParseError(parse_err);
        assert!(pe.source().is_some());

        let te = MutationError::Transport(anyhow::anyhow!("transport oops"));
        assert!(te.source().is_some());
    }

    #[test]
    fn bootstrap_js_climbs_to_shadow_host_for_closed_roots() {
        // The exact closed-shadow contract is enforced by the JS runtime,
        // not Rust unit tests; what we can pin here is that the bootstrap
        // string contains the climb-helper name and references `mode ===
        // 'closed'`, so a refactor that drops the helper trips this test.
        assert!(OBSERVER_BOOTSTRAP_JS.contains("climbToShadowHost"));
        assert!(OBSERVER_BOOTSTRAP_JS.contains("'closed'"));
        assert!(OBSERVER_BOOTSTRAP_JS.contains("getRootNode"));
    }
}
