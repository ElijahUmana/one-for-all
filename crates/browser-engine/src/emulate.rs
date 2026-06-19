//! M7 + M8 — Network conditions and locale/timezone/CPU emulation.
//!
//! Implements two new SPEC §7 tools (added in §10):
//! - `page.network_conditions` → `Network.emulateNetworkConditions`
//! - `page.emulate {locale?, timezone?, cpu_throttle?}` → three Emulation.* calls

use anyhow::Result;
use cdp_client::generated::domains::{emulation as cdp_emulation, network as cdp_network};
use serde_json::Value;

use crate::page::Page;

/// Throttle parameters for `page.network_conditions`.
#[derive(Debug, Clone, Default)]
pub struct NetworkConditions {
    pub offline: bool,
    /// Round-trip latency in ms.
    pub latency_ms: f64,
    /// Download throughput in bytes per second. `-1` means unthrottled.
    pub download_bps: f64,
    /// Upload throughput in bytes per second. `-1` means unthrottled.
    pub upload_bps: f64,
}

#[derive(Debug, Clone, Default)]
pub struct EmulateOptions {
    pub locale: Option<String>,
    pub timezone: Option<String>,
    /// CPU throttle factor (1.0 = unthrottled, 4.0 = 4x slower).
    pub cpu_throttle: Option<f64>,
}

impl Page {
    /// Implements SPEC §7 (added in §10) `page.network_conditions`.
    ///
    /// CANCELLATION: safe — each CDP call is awaited individually; cancel
    /// before completion leaves Chromium in its previous state.
    pub async fn set_network_conditions(&self, c: &NetworkConditions) -> Result<()> {
        // Network.emulateNetworkConditions is marked deprecated in CDP for
        // request-id-scoped throttling, but the unscoped form is still the
        // canonical (and only) way to set page-wide conditions.
        #[allow(deprecated)]
        self.cdp_send(cdp_network::EmulateNetworkConditionsParams {
            offline: c.offline,
            latency: c.latency_ms,
            download_throughput: c.download_bps,
            upload_throughput: c.upload_bps,
            ..Default::default()
        })
        .await?;
        Ok(())
    }

    /// Implements SPEC §7 (added in §10) `page.emulate`. Each option fires
    /// the corresponding `Emulation.*` CDP call; absent options are skipped.
    ///
    /// CANCELLATION: safe — composed of independent CDP calls; mid-cancel
    /// leaves only the unsent overrides un-applied.
    pub async fn emulate(&self, opts: &EmulateOptions) -> Result<()> {
        if let Some(loc) = &opts.locale {
            self.cdp_send(cdp_emulation::SetLocaleOverrideParams {
                locale: Some(loc.clone()),
            })
            .await?;
        }
        if let Some(tz) = &opts.timezone {
            self.cdp_send(cdp_emulation::SetTimezoneOverrideParams {
                timezone_id: tz.clone(),
            })
            .await?;
        }
        if let Some(rate) = opts.cpu_throttle {
            self.cdp_send(cdp_emulation::SetCpuThrottlingRateParams { rate })
                .await?;
        }
        Ok(())
    }

    /// Returns a "no throttling" `NetworkConditions`. Helper for clients
    /// that want to lift throttling without reaching for the magic values.
    pub fn unthrottled_network_conditions() -> NetworkConditions {
        NetworkConditions {
            offline: false,
            latency_ms: 0.0,
            download_bps: -1.0,
            upload_bps: -1.0,
        }
    }
}

/// Parse an `EmulateOptions` from a JSON-RPC `params` object. Pulls values
/// permissively — missing keys are silently None.
pub fn parse_emulate_options(params: &Value) -> EmulateOptions {
    EmulateOptions {
        locale: params
            .get("locale")
            .and_then(Value::as_str)
            .map(str::to_owned),
        timezone: params
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cpu_throttle: params.get("cpu_throttle").and_then(Value::as_f64),
    }
}

/// Parse a `NetworkConditions` from a JSON-RPC `params` object.
pub fn parse_network_conditions(params: &Value) -> NetworkConditions {
    NetworkConditions {
        offline: params
            .get("offline")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        latency_ms: params
            .get("latency_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        download_bps: params
            .get("download_bps")
            .and_then(Value::as_f64)
            .unwrap_or(-1.0),
        upload_bps: params
            .get("upload_bps")
            .and_then(Value::as_f64)
            .unwrap_or(-1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_emulate_options_picks_up_known_fields() {
        let v = json!({"locale": "fr-FR", "timezone": "Europe/Paris", "cpu_throttle": 4.0});
        let o = parse_emulate_options(&v);
        assert_eq!(o.locale.as_deref(), Some("fr-FR"));
        assert_eq!(o.timezone.as_deref(), Some("Europe/Paris"));
        assert_eq!(o.cpu_throttle, Some(4.0));
    }

    #[test]
    fn parse_network_conditions_defaults() {
        let v = json!({"offline": true});
        let n = parse_network_conditions(&v);
        assert!(n.offline);
        assert_eq!(n.download_bps, -1.0);
        assert_eq!(n.upload_bps, -1.0);
    }
}
