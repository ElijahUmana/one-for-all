//! Wait predicates: lifecycle, selector, URL regex, network-idle.
//!
//! Implements SPEC §7 `tab.wait` and the `wait_until` parameter on
//! `tab.open` / `tab.navigate`.

use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use cdp_client::generated::domains::runtime as cdp_runtime;
use serde_json::{json, Value};
use tokio::time::{timeout, Instant};
use tracing::trace;

use crate::page::Page;
use crate::WaitUntil;

impl Page {
    /// Wait for the page lifecycle to advance to a SPEC-named milestone.
    ///
    /// CDP's lifecycle event stream emits `name` strings: `init`,
    /// `firstPaint`, `firstContentfulPaint`, `DOMContentLoaded`, `load`,
    /// `networkAlmostIdle`, `networkIdle`. We map our spec names to those.
    pub async fn wait_for_lifecycle(&self, until: WaitUntil, deadline: Duration) -> Result<()> {
        let target = match until {
            WaitUntil::None => return Ok(()),
            WaitUntil::Load => "load",
            WaitUntil::DomContentLoaded => "DOMContentLoaded",
            WaitUntil::NetworkIdle => "networkIdle",
        };

        let mut rx = self.lifecycle_subscribe();
        timeout(deadline, async {
            loop {
                let ev = rx
                    .recv()
                    .await
                    .map_err(|e| anyhow!("lifecycle channel error: {e}"))?;
                trace!(name = %ev.name, "lifecycle");
                if ev.name == target {
                    return Ok::<(), anyhow::Error>(());
                }
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for lifecycle {target} after {deadline:?}"))??;
        Ok(())
    }

    /// Wait for an element matching `selector` (CSS) to appear in the
    /// document. Polls via `Runtime.evaluate` every 100ms.
    pub async fn wait_for_selector(&self, selector: &str, deadline: Duration) -> Result<()> {
        let start = Instant::now();
        let expr = format!(
            "(()=>{{const e=document.querySelector({}); return !!e}})()",
            json!(selector)
        );
        loop {
            let res = self
                .cdp_send(cdp_runtime::EvaluateParams {
                    expression: expr.clone(),
                    return_by_value: Some(true),
                    await_promise: Some(false),
                    ..Default::default()
                })
                .await
                .context("Runtime.evaluate while wait_for_selector")?;
            if res
                .result
                .get("value")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(());
            }
            if start.elapsed() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for selector {selector:?} after {deadline:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait for the page URL to match `pattern` (regex against the live URL).
    ///
    /// CANCELLATION: safe — bails on whichever fires first between the
    /// `nav` broadcast and the deadline.
    pub async fn wait_for_url(&self, pattern: &str, deadline: Duration) -> Result<String> {
        // Use the full `regex` crate per SPEC §7 `tab.wait` semantics.
        let re = regex::Regex::new(pattern).map_err(|e| anyhow!("invalid url_regex: {e}"))?;

        // Check the current URL first.
        let url = self.url();
        if !url.is_empty() && re.is_match(&url) {
            return Ok(url);
        }

        let mut rx = self.nav_subscribe();
        timeout(deadline, async {
            loop {
                let url = rx
                    .recv()
                    .await
                    .map_err(|e| anyhow!("nav channel error: {e}"))?;
                if re.is_match(&url) {
                    return Ok::<String, anyhow::Error>(url);
                }
            }
        })
        .await
        .map_err(|_| anyhow!("timed out waiting for url {pattern:?} after {deadline:?}"))?
    }

    /// Wait until the page has had no in-flight network requests for at
    /// least `idle_window`. The hard ceiling is `deadline`.
    pub async fn wait_for_network_idle(
        &self,
        idle_window: Duration,
        deadline: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        let mut last_busy_at = Instant::now();
        loop {
            if self.in_flight_count() == 0 {
                if last_busy_at.elapsed() >= idle_window {
                    return Ok(());
                }
            } else {
                last_busy_at = Instant::now();
            }
            if start.elapsed() >= deadline {
                return Err(anyhow!(
                    "network-idle window {idle_window:?} not reached within {deadline:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Real PCRE-flavor URL regex via the `regex` crate. Replaced the
/// substring-fallback shim that violated SPEC §7 `tab.wait` `url_regex`
/// semantics.
#[cfg(test)]
mod regex_tests {
    use regex::Regex;

    #[test]
    fn url_regex_matches_path_segments() {
        let re = Regex::new(r"^https://example\.com/login(\?.*)?$").expect("test regex");
        assert!(re.is_match("https://example.com/login"));
        assert!(re.is_match("https://example.com/login?next=%2F"));
        assert!(!re.is_match("https://example.com/login/extra"));
    }

    #[test]
    fn url_regex_anchors_required_for_strict_match() {
        // Without ^…$ the pattern is unanchored — same as substring search,
        // which is what most callers expect.
        let re = Regex::new(r"oauth").expect("test regex");
        assert!(re.is_match("https://accounts.google.com/oauth/callback"));
    }
}
