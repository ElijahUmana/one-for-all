//! [`BrowserContext`] — wraps a [`Browser`] and exposes per-tab operations.
//!
//! Per SPEC D2 reconcile note: in v1 each session is its own Chromium
//! process, so `context_id == session_id`. The `BrowserContext` API is
//! preserved for forward compatibility with a future shared-Chromium mode.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use cdp_client::generated::domains::{page as cdp_page, target as cdp_target};
use cdp_client::SessionId;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::browser::{validate_navigable_url, Browser};
use crate::page::{Page, TabId};
use crate::WaitUntil;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContextId(pub String);

impl ContextId {
    pub fn from_session(session_id: &str) -> Self {
        Self(session_id.to_owned())
    }
}

pub struct BrowserContext {
    browser: Browser,
    id: ContextId,
    pages: Mutex<HashMap<TabId, Arc<Page>>>,
    /// Per-context stealth on/off. SPEC §10 M3 — default true.
    stealth: bool,
    /// Per-context M10 trace flag.
    trace: bool,
    /// Deterministic seed used by stealth canvas/WebGL noise.
    stealth_seed: u64,
}

impl BrowserContext {
    pub(crate) fn new_default(browser: Browser) -> Self {
        Self::with_options(browser, "default", true, false, default_seed())
    }

    /// Create a context with explicit stealth + trace toggles per SPEC
    /// §10 M3 / M10. `seed` keys the stealth canvas/WebGL noise so the
    /// fingerprint is consistent across reloads in the same context.
    pub fn with_options(browser: Browser, id: &str, stealth: bool, trace: bool, seed: u64) -> Self {
        Self {
            browser,
            id: ContextId(id.to_owned()),
            pages: Mutex::new(HashMap::new()),
            stealth,
            trace,
            stealth_seed: seed,
        }
    }

    pub fn id(&self) -> &ContextId {
        &self.id
    }

    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    /// True if stealth scripts should be injected into new pages.
    pub fn stealth_enabled(&self) -> bool {
        self.stealth
    }

    /// True if M10 trace recording should be performed for this context.
    pub fn trace_enabled(&self) -> bool {
        self.trace
    }

    pub fn stealth_seed(&self) -> u64 {
        self.stealth_seed
    }

    /// Open a new tab. Implements `tab.open` from SPEC §7.
    ///
    /// CANCELLATION: not safe — partial state can leak (orphan target). Caller
    /// must `tab.close` if the result is dropped.
    pub async fn open_tab(&self, url: &str, wait_until: WaitUntil) -> Result<Arc<Page>> {
        validate_navigable_url(url)?;
        // Target.createTarget on the root session.
        let res = self
            .browser
            .cdp()
            .root_session()
            .send(cdp_target::CreateTargetParams {
                url: url.to_owned(),
                new_window: Some(false),
                background: Some(true),
                ..Default::default()
            })
            .await
            .context("Target.createTarget")?;

        let target_id = res
            .target_id
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "Target.createTarget returned non-string targetId: {:?}",
                    res.target_id
                )
            })?
            .to_owned();

        // Attach to the target to get a CDP session.
        let attach = self
            .browser
            .cdp()
            .root_session()
            .send(cdp_target::AttachToTargetParams {
                target_id: Value::String(target_id.clone()),
                flatten: Some(true),
            })
            .await
            .context("Target.attachToTarget")?;

        let session_id = attach
            .session_id
            .as_str()
            .ok_or_else(|| {
                anyhow!(
                    "Target.attachToTarget returned non-string sessionId: {:?}",
                    attach.session_id
                )
            })?
            .to_owned();

        let page = Arc::new(
            Page::bootstrap(
                self.browser.clone(),
                target_id.clone(),
                SessionId(session_id),
            )
            .await?,
        );

        // SPEC §10 M3 stealth: inject the bundle on every new document for
        // this page. Idempotent across navigations.
        if self.stealth {
            page.cdp_send(cdp_page::AddScriptToEvaluateOnNewDocumentParams {
                source: crate::stealth::script(self.stealth_seed),
                ..Default::default()
            })
            .await
            .context("Page.addScriptToEvaluateOnNewDocument (stealth)")?;
        }

        // Wait predicate per `tab.open` semantics.
        if wait_until != WaitUntil::None {
            page.wait_for_lifecycle(wait_until, std::time::Duration::from_secs(30))
                .await?;
        }

        self.pages
            .lock()
            .insert(page.tab_id().clone(), Arc::clone(&page));
        Ok(page)
    }

    /// Implements `tab.list` (SPEC §7).
    pub fn list_tabs(&self) -> Vec<Arc<Page>> {
        self.pages.lock().values().cloned().collect()
    }

    /// Implements `tab.close` (SPEC §7).
    pub async fn close_tab(&self, tab_id: &TabId) -> Result<()> {
        let page = self
            .pages
            .lock()
            .remove(tab_id)
            .ok_or_else(|| anyhow!("tab not found: {tab_id:?}"))?;
        if let Err(e) = page.close().await {
            warn!(?tab_id, error = %e, "page close error");
        }
        debug!(?tab_id, "closed tab");
        Ok(())
    }

    /// Lookup a page by `tab_id`.
    pub fn get(&self, tab_id: &TabId) -> Option<Arc<Page>> {
        self.pages.lock().get(tab_id).cloned()
    }

    /// SPEC §10 M4 — after a Chromium crash + respawn, Chromium itself
    /// rehydrates open tabs from `Default/Sessions/` in the persistent
    /// user-data dir. This helper enumerates those restored targets via
    /// `Target.getTargets`, attaches a fresh CDP session to each, and
    /// builds new [`Page`] handles. The previous `pages` map (whose
    /// handles point at the dead CDP connection) is fully replaced.
    ///
    /// Stealth: when `self.stealth_enabled()` is true the bootstrap script
    /// is re-injected on each restored page so the fingerprint matches
    /// the pre-crash one (same `stealth_seed`).
    ///
    /// CANCELLATION: not safe — the function performs N independent CDP
    /// round-trips and a partial result would leave `self.pages` in a
    /// half-rebuilt state. Caller must `await` it to completion.
    pub async fn reattach_existing_targets(&self) -> Result<Vec<Arc<Page>>> {
        let res = self
            .browser
            .cdp()
            .root_session()
            .send_with_retry(cdp_target::GetTargetsParams::default())
            .await
            .context("Target.getTargets")?;

        let infos = res.target_infos.as_array().cloned().unwrap_or_default();

        let mut restored: Vec<Arc<Page>> = Vec::new();
        for info in infos {
            let kind = info.get("type").and_then(Value::as_str).unwrap_or("");
            if kind != "page" {
                continue;
            }
            let target_id = match info.get("targetId").and_then(Value::as_str) {
                Some(t) => t.to_owned(),
                None => {
                    warn!(?info, "Target.getTargets entry missing targetId; skipping");
                    continue;
                }
            };

            let attach = match self
                .browser
                .cdp()
                .root_session()
                .send(cdp_target::AttachToTargetParams {
                    target_id: Value::String(target_id.clone()),
                    flatten: Some(true),
                })
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, %target_id, "Target.attachToTarget failed; skipping");
                    continue;
                }
            };
            let session_id = match attach.session_id.as_str() {
                Some(s) => s.to_owned(),
                None => {
                    warn!(?attach.session_id, %target_id, "attach response missing sessionId; skipping");
                    continue;
                }
            };

            let page = match Page::bootstrap(
                self.browser.clone(),
                target_id.clone(),
                SessionId(session_id),
            )
            .await
            {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    warn!(error = %e, %target_id, "Page::bootstrap failed during reattach; skipping");
                    continue;
                }
            };

            // Re-inject stealth on each restored page so canvas/WebGL noise
            // matches the pre-crash fingerprint (same seed → same hash).
            if self.stealth {
                if let Err(e) = page
                    .cdp_send(cdp_page::AddScriptToEvaluateOnNewDocumentParams {
                        source: crate::stealth::script(self.stealth_seed),
                        ..Default::default()
                    })
                    .await
                {
                    warn!(error = %e, %target_id, "stealth re-inject failed");
                }
            }

            // Best-effort: refresh URL/title so the next `tab.list` is
            // populated. A failure here doesn't disqualify the page.
            let _ = page.refresh_target_info().await;

            restored.push(page);
        }

        // Atomically replace the page map. The old Page handles' event-pump
        // tasks die naturally because their CDP receivers close when the
        // dead connection is dropped (they're keyed off the old Browser).
        let mut g = self.pages.lock();
        g.clear();
        for p in &restored {
            g.insert(p.tab_id().clone(), Arc::clone(p));
        }
        debug!(
            count = restored.len(),
            "BrowserContext: restored targets after recovery"
        );

        Ok(restored)
    }
}

/// Cheap process-wide entropy fallback. Used so that stealth seeds vary
/// across contexts without requiring the spec-banned `Date.now()`/random
/// surfaces inside workflow-style harnesses.
fn default_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0xCAFE_BABE_DEAD_BEEF);
    let n = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    n
}
