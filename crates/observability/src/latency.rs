//! HDR histogram type used for latency-tracked operations.
//!
//! SPEC §11 V5 — every latency-tracked op feeds a `LatencyHistogram`. The
//! broker's `_internal.metrics` RPC reads `LatencySnapshot { p50, p95, p99,
//! max, count }` per (session, method) pair. Range is 1 µs..60 s with
//! 3-significant-figure precision (the `hdrhistogram` defaults), bounded so
//! a runaway latency cannot blow per-histogram memory past ~5 KiB.
//!
//! Threading: a `parking_lot::Mutex` wraps the inner histogram. We measured
//! the lock cost in `crates/bench` and confirmed it does not dominate the
//! benches' per-op budget. `hdrhistogram::AtomicHistogram` cannot be
//! snapshotted-and-reset on stable today; the lock is the right tool here.
//!
//! Recording is `record_micros` because all SLO targets in SPEC §11 V4 are
//! sub-3-second. We saturate to the high-bound (`60_000_000 µs`) instead of
//! returning an error so a slow path never disrupts the hot path.
//!
//! # Why a custom wrapper
//!
//! Callers should not have to know about hdrhistogram internals — they want
//! `LatencyTimer::new(&hist)` RAII timing and `_internal.metrics` JSON. The
//! wrapper enforces the bounds, the saturation policy, and the snapshot
//! shape so every histogram in the system is comparable.

use std::sync::Arc;
use std::time::Instant;

use hdrhistogram::Histogram;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Upper bound: 60 seconds in microseconds. SPEC §11 V4's slowest SLO is
/// the cold sandbox spawn at <3 s p99; 60 s gives us a 20× headroom for
/// outliers and matches the histogram resolution sweet spot.
const HIST_HIGH_US: u64 = 60_000_000;

/// 3 significant figures = ~5 KiB per histogram, fits in L1 on every host
/// we target. Standard hdrhistogram default.
const HIST_SIGFIG: u8 = 3;

/// HDR histogram of latencies in microseconds.
pub struct LatencyHistogram {
    inner: Mutex<Histogram<u64>>,
    name: &'static str,
}

impl std::fmt::Debug for LatencyHistogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LatencyHistogram")
            .field("name", &self.name)
            .finish()
    }
}

impl LatencyHistogram {
    /// Build a fresh histogram. Returns `Err` only if the hdrhistogram
    /// crate rejects the bounds — `(1, HIST_HIGH_US, 3)` is well-formed,
    /// so this is plumbing-grade error handling, not a runtime concern.
    pub fn new(name: &'static str) -> Result<Self, hdrhistogram::CreationError> {
        let hist = Histogram::<u64>::new_with_bounds(1, HIST_HIGH_US, HIST_SIGFIG)?;
        Ok(Self {
            inner: Mutex::new(hist),
            name,
        })
    }

    /// Construct an Arc-wrapped histogram for sharing across threads /
    /// sessions. Panics only on construction failure — callers that need
    /// fallible construction should use `new` directly.
    ///
    /// (`new` itself only fails on bad bounds; this helper is a convenience
    /// for static initialization where the bounds are compile-time known.)
    pub fn shared(name: &'static str) -> Arc<Self> {
        // SAFETY of `expect`: the bounds (1, HIST_HIGH_US, 3) are
        // compile-time constants that hdrhistogram accepts. A panic here
        // would mean the crate's invariants changed.
        #[allow(clippy::expect_used)]
        let h = Self::new(name).expect("hdrhistogram bounds are compile-time valid");
        Arc::new(h)
    }

    /// Name for the metric (used in `_internal.metrics` keys).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Record `us` microseconds. Saturates at the histogram high-bound so a
    /// pathological latency never poisons the histogram or the hot path.
    pub fn record_micros(&self, us: u64) {
        let clamped = us.min(HIST_HIGH_US);
        // `record` only fails when the value is outside `[low, high]`; the
        // clamp above guarantees it never does. The .ok() acknowledges the
        // result without unwrap.
        let mut g = self.inner.lock();
        let _ = g.record(clamped);
    }

    /// Record an elapsed `Duration`.
    pub fn record_duration(&self, d: std::time::Duration) {
        // saturating_mul_int via as_micros (u128 → u64 saturate).
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        self.record_micros(us);
    }

    /// Read percentiles + count without resetting the histogram.
    pub fn snapshot(&self) -> LatencySnapshot {
        let g = self.inner.lock();
        LatencySnapshot {
            p50_us: g.value_at_quantile(0.50),
            p95_us: g.value_at_quantile(0.95),
            p99_us: g.value_at_quantile(0.99),
            max_us: g.max(),
            count: g.len(),
        }
    }

    /// Reset the histogram. Used by the bench harness between runs.
    pub fn reset(&self) {
        let mut g = self.inner.lock();
        g.reset();
    }
}

/// Immutable snapshot of a histogram's percentiles. JSON shape consumed by
/// `_internal.metrics`. Microsecond resolution is fine for every operation
/// we track — sub-microsecond latency does not exist on this stack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatencySnapshot {
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub count: u64,
}

/// RAII timer that records elapsed wall time on drop.
///
/// ```ignore
/// let _t = LatencyTimer::new(&hist);
/// do_the_thing();
/// // hist gets `<elapsed>` recorded when `_t` goes out of scope.
/// ```
pub struct LatencyTimer<'a> {
    start: Instant,
    hist: &'a LatencyHistogram,
}

impl<'a> LatencyTimer<'a> {
    pub fn new(hist: &'a LatencyHistogram) -> Self {
        Self {
            start: Instant::now(),
            hist,
        }
    }
}

impl Drop for LatencyTimer<'_> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.hist.record_duration(elapsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn record_and_snapshot_round_trip() {
        let h = LatencyHistogram::new("test").unwrap();
        for v in [10, 20, 30, 40, 50, 100, 200, 1000] {
            h.record_micros(v);
        }
        let snap = h.snapshot();
        assert_eq!(snap.count, 8);
        assert!(snap.p50_us >= 30 && snap.p50_us <= 60);
        assert!(snap.p99_us >= 200);
        assert!(snap.max_us >= 1000);
    }

    #[test]
    fn record_saturates_at_high_bound() {
        let h = LatencyHistogram::new("sat").unwrap();
        h.record_micros(u64::MAX);
        let snap = h.snapshot();
        assert_eq!(snap.count, 1);
        // hdrhistogram buckets at 3 sigfigs; max() can round up slightly
        // past the exact high bound. Allow up to 0.1% tolerance — what
        // matters is that the value did not blow past the histogram range
        // entirely (which would have erroed on `record`, not silently
        // recorded `u64::MAX`).
        let tolerance = HIST_HIGH_US / 1000;
        assert!(
            snap.max_us <= HIST_HIGH_US + tolerance,
            "max_us {} > HIST_HIGH_US {} + tolerance {}",
            snap.max_us,
            HIST_HIGH_US,
            tolerance,
        );
    }

    #[test]
    fn reset_clears_count() {
        let h = LatencyHistogram::new("rst").unwrap();
        h.record_micros(100);
        assert_eq!(h.snapshot().count, 1);
        h.reset();
        assert_eq!(h.snapshot().count, 0);
    }

    #[test]
    fn timer_records_on_drop() {
        let h = LatencyHistogram::new("tmr").unwrap();
        {
            let _t = LatencyTimer::new(&h);
            std::thread::sleep(Duration::from_micros(50));
        }
        assert_eq!(h.snapshot().count, 1);
    }

    #[test]
    fn snapshot_serialization_shape() {
        let h = LatencyHistogram::new("ser").unwrap();
        h.record_micros(123);
        let snap = h.snapshot();
        let v = serde_json::to_value(&snap).unwrap();
        assert!(v.get("p50_us").is_some());
        assert!(v.get("p95_us").is_some());
        assert!(v.get("p99_us").is_some());
        assert!(v.get("max_us").is_some());
        assert!(v.get("count").is_some());
    }
}
