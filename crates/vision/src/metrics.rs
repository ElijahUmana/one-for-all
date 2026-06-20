//! Per-stage histograms for SLO enforcement (SPEC §11 V4).
//!
//! Lock-free, fixed-bucket HDR-style histograms backed by `AtomicU64`. Each
//! bucket covers a log-spaced millisecond range; `record(ms)` finds the
//! right bucket via leading-zero math (constant time).
//!
//! Exported names match SPEC §11 V4 latency-budget rows:
//!
//! - `vision.capture_ms`         — CDP frame → decoded RGBA
//! - `vision.diff_ms`            — per-frame SIMD diff
//! - `vision.ocr_ms`             — per-tile OCR
//! - `vision.vlm_ms`             — optional pre-action VLM call
//! - `vision.pipeline_total_ms`  — capture → vision.frame event end-to-end
//! - `vision.find_text_ms`       — `vision.find_text` query latency

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;

/// 32 buckets covers 0–~4 seconds with log2 spacing. Buckets:
/// [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, ...].
pub const BUCKETS: usize = 32;

#[derive(Debug)]
pub struct Histogram {
    name: String,
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum: AtomicU64,
}

impl Histogram {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Record a sample in milliseconds. Saturates at the top bucket.
    pub fn record(&self, ms: u64) {
        let bucket = bucket_for(ms);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(ms, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn sum(&self) -> u64 {
        self.sum.load(Ordering::Relaxed)
    }

    pub fn percentile(&self, p: f32) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        let target = ((p.clamp(0.0, 1.0) as f64) * (total as f64)).ceil() as u64;
        let mut acc = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            acc += b.load(Ordering::Relaxed);
            if acc >= target {
                return bucket_upper_bound(i);
            }
        }
        bucket_upper_bound(BUCKETS - 1)
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            name: self.name.clone(),
            count: self.count(),
            sum_ms: self.sum(),
            p50: self.percentile(0.50),
            p90: self.percentile(0.90),
            p99: self.percentile(0.99),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistogramSnapshot {
    pub name: String,
    pub count: u64,
    pub sum_ms: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
}

/// Bucket index for a given millisecond reading.
fn bucket_for(ms: u64) -> usize {
    if ms == 0 {
        return 0;
    }
    let leading = ms.leading_zeros() as usize;
    let idx = 64 - leading; // ms ∈ (2^(idx-1), 2^idx]
    idx.min(BUCKETS - 1)
}

fn bucket_upper_bound(idx: usize) -> u64 {
    if idx == 0 {
        0
    } else if idx >= BUCKETS - 1 {
        u64::MAX / 2
    } else {
        1u64 << idx
    }
}

/// Bundle of every vision histogram, owned by the broker State.
#[derive(Debug)]
pub struct Histograms {
    inner: Arc<HistogramsInner>,
}

#[derive(Debug)]
struct HistogramsInner {
    pub capture_ms: Histogram,
    pub diff_ms: Histogram,
    pub ocr_ms: Histogram,
    pub vlm_ms: Histogram,
    pub pipeline_total_ms: Histogram,
    pub find_text_ms: Histogram,
    extras: RwLock<Vec<Arc<Histogram>>>,
}

impl Histograms {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HistogramsInner {
                capture_ms: Histogram::new("vision.capture_ms"),
                diff_ms: Histogram::new("vision.diff_ms"),
                ocr_ms: Histogram::new("vision.ocr_ms"),
                vlm_ms: Histogram::new("vision.vlm_ms"),
                pipeline_total_ms: Histogram::new("vision.pipeline_total_ms"),
                find_text_ms: Histogram::new("vision.find_text_ms"),
                extras: RwLock::new(Vec::new()),
            }),
        }
    }

    pub fn capture_ms(&self) -> &Histogram {
        &self.inner.capture_ms
    }
    pub fn diff_ms(&self) -> &Histogram {
        &self.inner.diff_ms
    }
    pub fn ocr_ms(&self) -> &Histogram {
        &self.inner.ocr_ms
    }
    pub fn vlm_ms(&self) -> &Histogram {
        &self.inner.vlm_ms
    }
    pub fn pipeline_total_ms(&self) -> &Histogram {
        &self.inner.pipeline_total_ms
    }
    pub fn find_text_ms(&self) -> &Histogram {
        &self.inner.find_text_ms
    }

    /// Snapshot every named histogram. Stable insertion order: built-ins
    /// first, then extras in registration order.
    pub fn snapshot_all(&self) -> Vec<HistogramSnapshot> {
        let mut out = vec![
            self.inner.capture_ms.snapshot(),
            self.inner.diff_ms.snapshot(),
            self.inner.ocr_ms.snapshot(),
            self.inner.vlm_ms.snapshot(),
            self.inner.pipeline_total_ms.snapshot(),
            self.inner.find_text_ms.snapshot(),
        ];
        for h in self.inner.extras.read().iter() {
            out.push(h.snapshot());
        }
        out
    }
}

impl Clone for Histograms {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for Histograms {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_basic() {
        let h = Histogram::new("test");
        // Record uniform 1..=100ms.
        for i in 1u64..=100 {
            h.record(i);
        }
        assert_eq!(h.count(), 100);
        let p50 = h.percentile(0.50);
        let p90 = h.percentile(0.90);
        let p99 = h.percentile(0.99);
        assert!(p50 <= 64, "p50 = {p50}");
        assert!(p90 <= 128, "p90 = {p90}");
        assert!(p99 <= 128, "p99 = {p99}");
    }

    #[test]
    fn record_zero_is_bucket_zero() {
        let h = Histogram::new("test");
        h.record(0);
        assert_eq!(h.count(), 1);
        assert_eq!(h.percentile(0.99), 0);
    }

    #[test]
    fn snapshot_includes_all() {
        let hs = Histograms::new();
        hs.capture_ms().record(2);
        hs.diff_ms().record(3);
        hs.ocr_ms().record(7);
        hs.find_text_ms().record(1);
        let snap = hs.snapshot_all();
        let names: Vec<_> = snap.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"vision.capture_ms"));
        assert!(names.contains(&"vision.diff_ms"));
        assert!(names.contains(&"vision.ocr_ms"));
        assert!(names.contains(&"vision.find_text_ms"));
    }
}
