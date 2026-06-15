//! Per-method CDP latency histograms + outcome counters.
//!
//! `cdp-client` exposes a [`MetricsSink`] hook that fires once per
//! `CdpSession::send*` (success, protocol error, transport error, retry).
//! This module is the broker-side implementation: it keys on the
//! `&'static str` method name (interned by codegen — no allocation per call)
//! and records:
//!
//! - an HDR latency histogram per method, surfaced via
//!   [`crate::LatencySnapshot`] in `_internal.metrics`;
//! - per-outcome counters (`ok`, `protocol_error`, `transport_error`,
//!   `internal_error`);
//! - a `retries` counter — `attempts.saturating_sub(1)` per call. Distinguishes
//!   "succeeded after retry" (visible) from "single-shot success" (not).
//!
//! The hot path is one `DashMap::entry().or_insert_with` followed by a
//! `LatencyHistogram::record_duration` plus three `AtomicU64::fetch_add` —
//! no allocation, no broadcast, no async.
//!
//! [`MetricsSink`]: cdp_client::MetricsSink

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cdp_client::{MetricsSink, Outcome};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::latency::{LatencyHistogram, LatencySnapshot};

const DYNAMIC_METHOD_METRIC_NAME: &str = "cdp.raw.dynamic";

/// Counters + latency histogram for a single CDP method (e.g.
/// `Page.captureScreenshot`).
struct MethodMetrics {
    /// Wrapped in an `Arc` because [`LatencyHistogram::shared`] is the
    /// crate's blessed infallible constructor — it returns `Arc<Self>` and
    /// guarantees the bounds are compile-time valid. We hold the `Arc`
    /// directly so calls go through `record_duration` without an extra
    /// indirection.
    latency: Arc<LatencyHistogram>,
    ok: AtomicU64,
    protocol_error: AtomicU64,
    transport_error: AtomicU64,
    internal_error: AtomicU64,
    /// Sum of `attempts.saturating_sub(1)` across every call. Equal to the
    /// number of retries the caller did NOT see (because the call ultimately
    /// succeeded or surfaced an error after >1 attempt).
    retries: AtomicU64,
}

impl MethodMetrics {
    fn new(method: &'static str) -> Self {
        Self::with_histogram_name(method)
    }

    fn with_histogram_name(name: &'static str) -> Self {
        Self {
            // `LatencyHistogram::shared` is the crate's infallible constructor;
            // its bounds (1, 60_000_000, 3) are compile-time valid.
            latency: LatencyHistogram::shared(name),
            ok: AtomicU64::new(0),
            protocol_error: AtomicU64::new(0),
            transport_error: AtomicU64::new(0),
            internal_error: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        }
    }

    fn record(&self, latency: Duration, outcome: Outcome, attempts: u32) {
        self.latency.record_duration(latency);
        let counter = match outcome {
            Outcome::Ok => &self.ok,
            Outcome::ProtocolError => &self.protocol_error,
            Outcome::Transport => &self.transport_error,
            Outcome::Internal => &self.internal_error,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let extra = u64::from(attempts.saturating_sub(1));
        if extra > 0 {
            self.retries.fetch_add(extra, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> CdpMethodSnapshot {
        CdpMethodSnapshot {
            latency: self.latency.snapshot(),
            ok: self.ok.load(Ordering::Relaxed),
            protocol_error: self.protocol_error.load(Ordering::Relaxed),
            transport_error: self.transport_error.load(Ordering::Relaxed),
            internal_error: self.internal_error.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
        }
    }
}

/// `CdpMetricsSink` implements [`cdp_client::MetricsSink`] for the broker.
///
/// One sink per `cdp-client::Connection` (i.e. one per Browser / per session).
/// Cheap to clone via the wrapping `Arc<CdpMetricsSink>` consumers hold.
///
/// ## Hot path
///
/// `record_call` is called from inside `CdpSession::send*` after the wire
/// reply arrives (or the transport error fires). It must not block, allocate,
/// or `.await`:
///
/// ```text
/// dashmap.entry(method).or_insert_with(...)           // pointer compare
///   ↓ (returns &MethodMetrics)
/// latency.record_duration(...)                        // ~µs Mutex<HDR>
/// outcome counter.fetch_add(1, Relaxed)               // single atomic add
/// retries.fetch_add(extra, Relaxed)                   // skipped if extra=0
/// ```
pub struct CdpMetricsSink {
    methods: DashMap<&'static str, Arc<MethodMetrics>>,
    dynamic_methods: DashMap<String, Arc<MethodMetrics>>,
}

impl CdpMetricsSink {
    pub fn new() -> Self {
        Self {
            methods: DashMap::new(),
            dynamic_methods: DashMap::new(),
        }
    }

    /// Snapshot every recorded method, keyed by wire method name. Sorted
    /// alphabetically so JSON output is stable across runs (matters for
    /// integration-test golden comparisons and for humans reading
    /// `_internal.metrics`).
    pub fn snapshot(&self) -> CdpMethodsSnapshot {
        let mut methods = BTreeMap::new();
        for kv in self.methods.iter() {
            methods.insert((*kv.key()).to_owned(), kv.value().snapshot());
        }
        for kv in self.dynamic_methods.iter() {
            methods.insert(kv.key().clone(), kv.value().snapshot());
        }
        CdpMethodsSnapshot { methods }
    }

    /// Reset every histogram and counter. Used by integration tests; not
    /// exposed on the broker control plane (resetting metrics from a client
    /// would let one tab erase another's history).
    #[cfg(test)]
    fn reset(&self) {
        for kv in self.methods.iter() {
            kv.value().latency.reset();
            kv.value().ok.store(0, Ordering::Relaxed);
            kv.value().protocol_error.store(0, Ordering::Relaxed);
            kv.value().transport_error.store(0, Ordering::Relaxed);
            kv.value().internal_error.store(0, Ordering::Relaxed);
            kv.value().retries.store(0, Ordering::Relaxed);
        }
        for kv in self.dynamic_methods.iter() {
            kv.value().latency.reset();
            kv.value().ok.store(0, Ordering::Relaxed);
            kv.value().protocol_error.store(0, Ordering::Relaxed);
            kv.value().transport_error.store(0, Ordering::Relaxed);
            kv.value().internal_error.store(0, Ordering::Relaxed);
            kv.value().retries.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for CdpMetricsSink {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CdpMetricsSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpMetricsSink")
            .field("method_count", &self.methods.len())
            .field("dynamic_method_count", &self.dynamic_methods.len())
            .finish()
    }
}

impl MetricsSink for CdpMetricsSink {
    fn record_call(
        &self,
        method: &'static str,
        latency: Duration,
        outcome: Outcome,
        attempts: u32,
    ) {
        // `entry().or_insert_with` is the lock-free fast path on DashMap when
        // the key is already present (the common case after warmup). On first
        // sight of a method we pay one shard-write to install the entry, then
        // every subsequent call hits the read fast path.
        let m = self
            .methods
            .entry(method)
            .or_insert_with(|| Arc::new(MethodMetrics::new(method)));
        m.value().record(latency, outcome, attempts);
    }

    fn record_dynamic_call(
        &self,
        method: &str,
        latency: Duration,
        outcome: Outcome,
        attempts: u32,
    ) {
        if let Some(m) = self.dynamic_methods.get(method) {
            m.value().record(latency, outcome, attempts);
            return;
        }

        let m = self
            .dynamic_methods
            .entry(method.to_owned())
            .or_insert_with(|| {
                Arc::new(MethodMetrics::with_histogram_name(
                    DYNAMIC_METHOD_METRIC_NAME,
                ))
            });
        m.value().record(latency, outcome, attempts);
    }
}

/// Immutable snapshot returned by [`CdpMetricsSink::snapshot`]. JSON shape
/// is `{ "methods": { "<method>": <CdpMethodSnapshot>, ... } }` — stable
/// across runs because the inner map is `BTreeMap`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CdpMethodsSnapshot {
    pub methods: BTreeMap<String, CdpMethodSnapshot>,
}

/// Per-method snapshot. Mirrors the counters + histogram on
/// [`MethodMetrics`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CdpMethodSnapshot {
    pub latency: LatencySnapshot,
    pub ok: u64,
    pub protocol_error: u64,
    pub transport_error: u64,
    pub internal_error: u64,
    pub retries: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn record_round_trip() {
        let s = CdpMetricsSink::new();
        s.record_call(
            "Page.navigate",
            Duration::from_micros(1_500),
            Outcome::Ok,
            1,
        );
        s.record_call(
            "Page.navigate",
            Duration::from_micros(2_500),
            Outcome::Ok,
            1,
        );
        s.record_call(
            "Page.navigate",
            Duration::from_micros(900),
            Outcome::ProtocolError,
            1,
        );
        s.record_call(
            "Browser.getVersion",
            Duration::from_micros(120),
            Outcome::Transport,
            3,
        );

        let snap = s.snapshot();
        let nav = snap
            .methods
            .get("Page.navigate")
            .expect("Page.navigate recorded");
        assert_eq!(nav.ok, 2);
        assert_eq!(nav.protocol_error, 1);
        assert_eq!(nav.transport_error, 0);
        assert_eq!(nav.retries, 0);
        assert_eq!(nav.latency.count, 3);
        assert!(nav.latency.p99_us >= 2_500);

        let gv = snap
            .methods
            .get("Browser.getVersion")
            .expect("Browser.getVersion recorded");
        assert_eq!(gv.transport_error, 1);
        assert_eq!(gv.retries, 2, "attempts=3 → 2 retries");
    }

    #[test]
    fn snapshot_is_alphabetically_sorted() {
        let s = CdpMetricsSink::new();
        s.record_call("Z.last", Duration::from_micros(1), Outcome::Ok, 1);
        s.record_call("A.first", Duration::from_micros(1), Outcome::Ok, 1);
        s.record_call("M.middle", Duration::from_micros(1), Outcome::Ok, 1);
        let snap = s.snapshot();
        let keys: Vec<&str> = snap.methods.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["A.first", "M.middle", "Z.last"]);
    }

    #[test]
    fn outcome_counters_split_correctly() {
        let s = CdpMetricsSink::new();
        for o in [
            Outcome::Ok,
            Outcome::ProtocolError,
            Outcome::Transport,
            Outcome::Internal,
        ] {
            s.record_call("X.method", Duration::from_micros(10), o, 1);
        }
        let snap = s.snapshot();
        let m = snap.methods.get("X.method").expect("recorded");
        assert_eq!(m.ok, 1);
        assert_eq!(m.protocol_error, 1);
        assert_eq!(m.transport_error, 1);
        assert_eq!(m.internal_error, 1);
        assert_eq!(m.latency.count, 4);
    }

    #[test]
    fn reset_clears_counters() {
        let s = CdpMetricsSink::new();
        s.record_call("A.b", Duration::from_micros(10), Outcome::Ok, 1);
        s.reset();
        let snap = s.snapshot();
        let m = snap.methods.get("A.b").expect("entry survives reset");
        assert_eq!(m.ok, 0);
        assert_eq!(m.latency.count, 0);
    }

    #[test]
    fn snapshot_is_serializable_as_stable_json() {
        let s = CdpMetricsSink::new();
        s.record_call("Page.navigate", Duration::from_micros(123), Outcome::Ok, 1);
        let snap = s.snapshot();
        let v = serde_json::to_value(&snap).expect("serialize");
        assert!(v
            .get("methods")
            .and_then(|m| m.get("Page.navigate"))
            .is_some());
    }

    #[test]
    fn dynamic_methods_are_included_in_snapshot() {
        let s = CdpMetricsSink::new();
        s.record_dynamic_call(
            "Runtime.evaluate",
            Duration::from_micros(250),
            Outcome::Ok,
            2,
        );
        let snap = s.snapshot();
        let m = snap
            .methods
            .get("Runtime.evaluate")
            .expect("dynamic method recorded");
        assert_eq!(m.ok, 1);
        assert_eq!(m.retries, 1);
        assert_eq!(m.latency.count, 1);
    }
}
