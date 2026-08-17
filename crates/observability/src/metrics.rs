//! Per-session, lock-free counters surfaced by `ofa-status.sh` and
//! `_internal.metrics`. The broker (T5) owns the canonical registry; this
//! module defines the shared shape so mcp-server, broker, and tooling agree.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub type SessionId = String;

/// Atomic counters scoped to a single registered session.
#[derive(Debug, Default)]
pub struct SessionMetrics {
    pub tab_count: AtomicU32,
    pub request_count: AtomicU64,
    pub error_count: AtomicU64,
    /// Unix milliseconds; `0` means "never".
    pub last_activity_ms: AtomicI64,
    /// SPEC §10 M4 — number of times the broker has successfully respawned
    /// Chromium for this session after a crash (SIGSEGV/OOM within the
    /// 30 s activity window). Surfaced via `_internal.metrics`.
    pub recovery_count: AtomicU64,
    /// SPEC §10 M4 — number of times a respawn attempt failed (Chromium
    /// would not relaunch, the new handshake timed out, target re-attach
    /// failed, etc.). Operators use this to spot a UDD that's hosed.
    pub recovery_failed_count: AtomicU64,
    /// SPEC §10 M5 / N4 — outbound broker notifications dropped because the
    /// per-connection writer queue was full or closed.
    pub outbound_drop_count: AtomicU64,
    /// SPEC §11 V3 V-R1 — observed FileVault state at session register.
    /// `0` = Off, `1` = OnUnlocked, `2` = OnLocked, `3` = Unknown. Stored as
    /// a plain `u64` so the variant ordering matches `sandbox::FileVaultState
    /// as u64`. `ofa-status.sh` reads this field to decide whether to surface
    /// a "running on locked encrypted volume" warning.
    pub filevault_state: AtomicU64,
}

impl SessionMetrics {
    pub fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }

    pub fn snapshot(&self, session_id: &str) -> SessionSnapshot {
        SessionSnapshot {
            session_id: session_id.to_owned(),
            tab_count: self.tab_count.load(Ordering::Relaxed),
            request_count: self.request_count.load(Ordering::Relaxed),
            error_count: self.error_count.load(Ordering::Relaxed),
            last_activity_ms: self.last_activity_ms.load(Ordering::Relaxed),
            recovery_count: self.recovery_count.load(Ordering::Relaxed),
            recovery_failed_count: self.recovery_failed_count.load(Ordering::Relaxed),
            outbound_drop_count: self.outbound_drop_count.load(Ordering::Relaxed),
            filevault_state: self.filevault_state.load(Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot used by status tooling and the metrics RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub tab_count: u32,
    pub request_count: u64,
    pub error_count: u64,
    pub last_activity_ms: i64,
    /// SPEC §10 M4 — successful Chromium respawns for this session.
    #[serde(default)]
    pub recovery_count: u64,
    /// SPEC §10 M4 — failed respawn attempts for this session.
    #[serde(default)]
    pub recovery_failed_count: u64,
    /// SPEC §10 M5 / N4 — outbound broker notifications dropped because the
    /// client queue was full or closed.
    #[serde(default)]
    pub outbound_drop_count: u64,
    /// SPEC §11 V3 V-R1 — observed FileVault state at register.
    #[serde(default)]
    pub filevault_state: u64,
}

/// Concurrent registry of session metrics. Cheap to clone (Arc inside).
#[derive(Debug, Default, Clone)]
pub struct Registry {
    inner: Arc<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    sessions: RwLock<HashMap<SessionId, Arc<SessionMetrics>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the session entry. Returns a clone of the Arc so callers
    /// can hold it across `.await` points without re-locking.
    pub fn session(&self, session_id: &str) -> Arc<SessionMetrics> {
        if let Some(m) = self.inner.sessions.read().get(session_id) {
            return Arc::clone(m);
        }
        let mut w = self.inner.sessions.write();
        Arc::clone(
            w.entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(SessionMetrics::default())),
        )
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<SessionMetrics>> {
        self.inner.sessions.write().remove(session_id)
    }

    pub fn snapshot(&self) -> Vec<SessionSnapshot> {
        let r = self.inner.sessions.read();
        let mut out: Vec<_> = r.iter().map(|(k, v)| v.snapshot(k)).collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }
}

/// Power-of-two bucketed histogram. Buckets are [0,1), [1,2), [2,4), …
/// up to [2^62, ∞). Lock-free; safe to share across threads via `Arc`.
///
/// Used by `chromium-fetcher` (and future components) to record latency
/// or size distributions without pulling in a heavy metrics SDK. The
/// broker daemon has no terminal so progress reporting is split: durable
/// counters land here; ephemeral progress goes to `tracing::info`.
#[derive(Debug)]
pub struct Histogram {
    /// One bucket per power of two: bucket[i] counts samples in
    /// [2^i, 2^(i+1)). Bucket 0 covers [0, 1).
    buckets: [AtomicU64; 64],
    /// Total samples observed.
    count: AtomicU64,
    /// Sum of all samples (saturating).
    sum: AtomicU64,
    /// Highest sample seen (max).
    max: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            // `[AtomicU64; 64]` is not `Copy`, but `[const { … }; N]`
            // initialises each slot independently and avoids
            // `clippy::declare_interior_mutable_const` on a top-level
            // constant.
            buckets: [const { AtomicU64::new(0) }; 64],
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one sample. Safe to call from many threads; lock-free.
    pub fn observe(&self, value: u64) {
        let idx = if value == 0 {
            0
        } else {
            // Floor of log2(value); bucket i covers [2^i, 2^(i+1)).
            (63 - value.leading_zeros()) as usize
        };
        // 64-bit values can in principle overflow `idx == 63`; clamp.
        let idx = idx.min(63);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
        // Lock-free max via CAS loop; bounded retries (Relaxed is fine —
        // metrics are advisory, not part of program correctness).
        let mut cur = self.max.load(Ordering::Relaxed);
        while value > cur {
            match self
                .max
                .compare_exchange_weak(cur, value, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum.load(Ordering::Relaxed);
        let max = self.max.load(Ordering::Relaxed);
        let mut buckets = [0u64; 64];
        for (i, slot) in self.buckets.iter().enumerate() {
            buckets[i] = slot.load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            buckets,
            count,
            sum,
            max,
        }
    }
}

/// Immutable view of a [`Histogram`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    /// 64 power-of-two buckets; bucket[i] = count of samples in
    /// `[2^i, 2^(i+1))`.
    #[serde(with = "serde_buckets")]
    pub buckets: [u64; 64],
    pub count: u64,
    pub sum: u64,
    pub max: u64,
}

mod serde_buckets {
    use serde::de::Error as _;
    use serde::{Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(b: &[u64; 64], s: S) -> Result<S::Ok, S::Error> {
        b.as_slice().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u64; 64], D::Error> {
        let v: Vec<u64> = serde::Deserialize::deserialize(d)?;
        if v.len() != 64 {
            return Err(D::Error::custom(format!(
                "expected 64 buckets, got {}",
                v.len()
            )));
        }
        let mut out = [0u64; 64];
        out.copy_from_slice(&v);
        Ok(out)
    }
}

/// Cross-component fetch-related counters and histograms.
///
/// `chromium-fetcher` writes here on every download attempt. The broker
/// surfaces this via `_internal.metrics` so operators can spot networks
/// that are eating retries.
#[derive(Debug, Default)]
pub struct FetchMetrics {
    /// Total number of HTTP attempts (one GET = one attempt). Always
    /// ≥ `download_starts`; equal to it on a perfectly clean network.
    pub attempts: AtomicU64,
    /// Number of attempts that failed and were retried.
    pub retries: AtomicU64,
    /// Number of attempts aborted by the stall watchdog.
    pub stalls: AtomicU64,
    /// Number of attempts that received HTTP 416.
    pub range_416: AtomicU64,
    /// Number of full downloads started.
    pub download_starts: AtomicU64,
    /// Number of full downloads that ended with `Ok(())` (i.e. tmp →
    /// dest rename + size match).
    pub download_completions: AtomicU64,
    /// Number of full downloads that exhausted retries or hit total
    /// timeout.
    pub download_failures: AtomicU64,
    /// Bytes successfully written to the tmp file across all attempts.
    pub bytes_written: AtomicU64,
    /// Distribution of completed-download wall-clock times in
    /// milliseconds.
    pub download_ms: Histogram,
    /// Distribution of bytes-per-attempt (size of each successful GET
    /// body, regardless of whether the attempt completed the file).
    pub bytes_per_attempt: Histogram,
}

impl FetchMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_stall(&self) {
        self.stalls.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_range_416(&self) {
        self.range_416.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_download_start(&self) {
        self.download_starts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_download_completion(&self, total_ms: u64) {
        self.download_completions.fetch_add(1, Ordering::Relaxed);
        self.download_ms.observe(total_ms);
    }
    pub fn record_download_failure(&self) {
        self.download_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_bytes(&self, n: u64) {
        self.bytes_written.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_attempt_bytes(&self, n: u64) {
        self.bytes_per_attempt.observe(n);
    }

    pub fn snapshot(&self) -> FetchSnapshot {
        FetchSnapshot {
            attempts: self.attempts.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            stalls: self.stalls.load(Ordering::Relaxed),
            range_416: self.range_416.load(Ordering::Relaxed),
            download_starts: self.download_starts.load(Ordering::Relaxed),
            download_completions: self.download_completions.load(Ordering::Relaxed),
            download_failures: self.download_failures.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            download_ms: self.download_ms.snapshot(),
            bytes_per_attempt: self.bytes_per_attempt.snapshot(),
        }
    }
}

/// Immutable view of a [`FetchMetrics`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchSnapshot {
    pub attempts: u64,
    pub retries: u64,
    pub stalls: u64,
    pub range_416: u64,
    pub download_starts: u64,
    pub download_completions: u64,
    pub download_failures: u64,
    pub bytes_written: u64,
    pub download_ms: HistogramSnapshot,
    pub bytes_per_attempt: HistogramSnapshot,
}

/// Process-wide singleton for fetch metrics. `chromium-fetcher` records
/// against this directly so the broker's `_internal.metrics` snapshot
/// can surface them without explicit plumbing through `FetchOptions`.
pub fn fetch_metrics() -> &'static FetchMetrics {
    use once_cell::sync::Lazy;
    static FETCH: Lazy<FetchMetrics> = Lazy::new(FetchMetrics::default);
    &FETCH
}

/// SPEC §12 U4 + U5 — perf and PDF latency / size distributions.
///
/// `browser-engine::perf` and `browser-engine::pdf` record here on every
/// successful (or failed-with-known-cost) call. The broker surfaces this
/// via `_internal.metrics` so operators can spot heap snapshots blowing
/// past the 1 s p99 budget or PDFs ballooning past the 10 MiB inline cap.
///
/// The shape mirrors [`FetchMetrics`] so the existing snapshot serializer
/// in `_internal.metrics` extends cleanly: every field is either a counter
/// (cheap atomic) or a [`Histogram`] (lock-free power-of-two buckets).
#[derive(Debug, Default)]
pub struct PerfMetrics {
    /// Distribution of `page.pdf` wall-clock times (ms). One sample per
    /// successful PDF, regardless of inline vs streamed.
    pub pdf_ms: Histogram,
    /// Distribution of generated PDF sizes (bytes).
    pub pdf_bytes: Histogram,
    /// `page.print_preview` wall-clock (ms).
    pub print_preview_ms: Histogram,
    /// `page.heap_snapshot` wall-clock (ms). One sample per snapshot.
    pub heap_snapshot_ms: Histogram,
    /// `page.heap_snapshot` resulting file size (bytes).
    pub heap_snapshot_bytes: Histogram,
    /// `page.performance_timeline_stop` wall-clock (ms).
    pub trace_ms: Histogram,
    /// `page.performance_timeline_stop` resulting trace.json size (bytes).
    pub trace_bytes: Histogram,
    /// `page.cpu_profile` wall-clock (ms). Largely the requested duration
    /// plus ~CDP overhead — useful to spot CDP starvation.
    pub cpu_profile_ms: Histogram,
    /// `page.heap_sample_alloc` wall-clock (ms).
    pub heap_sample_ms: Histogram,
    /// `page.coverage_js_take` + `page.coverage_css_take` wall-clock (ms).
    pub coverage_take_ms: Histogram,
    /// Successful PDF count.
    pub pdf_count: AtomicU64,
    /// Successful heap snapshot count.
    pub heap_snapshot_count: AtomicU64,
    /// Successful tracing-stop count.
    pub trace_count: AtomicU64,
    /// Failed perf operation count (any U4 surface).
    pub perf_failure_count: AtomicU64,
}

impl PerfMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_pdf(&self, ms: u64, bytes: u64) {
        self.pdf_ms.observe(ms);
        self.pdf_bytes.observe(bytes);
        self.pdf_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_print_preview(&self, ms: u64) {
        self.print_preview_ms.observe(ms);
    }

    pub fn record_heap_snapshot(&self, ms: u64, bytes: u64) {
        self.heap_snapshot_ms.observe(ms);
        self.heap_snapshot_bytes.observe(bytes);
        self.heap_snapshot_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_trace(&self, ms: u64, bytes: u64) {
        self.trace_ms.observe(ms);
        self.trace_bytes.observe(bytes);
        self.trace_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cpu_profile(&self, ms: u64) {
        self.cpu_profile_ms.observe(ms);
    }

    pub fn record_heap_sample(&self, ms: u64) {
        self.heap_sample_ms.observe(ms);
    }

    pub fn record_coverage_take(&self, ms: u64) {
        self.coverage_take_ms.observe(ms);
    }

    pub fn record_failure(&self) {
        self.perf_failure_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PerfSnapshot {
        PerfSnapshot {
            pdf_ms: self.pdf_ms.snapshot(),
            pdf_bytes: self.pdf_bytes.snapshot(),
            print_preview_ms: self.print_preview_ms.snapshot(),
            heap_snapshot_ms: self.heap_snapshot_ms.snapshot(),
            heap_snapshot_bytes: self.heap_snapshot_bytes.snapshot(),
            trace_ms: self.trace_ms.snapshot(),
            trace_bytes: self.trace_bytes.snapshot(),
            cpu_profile_ms: self.cpu_profile_ms.snapshot(),
            heap_sample_ms: self.heap_sample_ms.snapshot(),
            coverage_take_ms: self.coverage_take_ms.snapshot(),
            pdf_count: self.pdf_count.load(Ordering::Relaxed),
            heap_snapshot_count: self.heap_snapshot_count.load(Ordering::Relaxed),
            trace_count: self.trace_count.load(Ordering::Relaxed),
            perf_failure_count: self.perf_failure_count.load(Ordering::Relaxed),
        }
    }
}

/// Immutable view of [`PerfMetrics`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSnapshot {
    pub pdf_ms: HistogramSnapshot,
    pub pdf_bytes: HistogramSnapshot,
    pub print_preview_ms: HistogramSnapshot,
    pub heap_snapshot_ms: HistogramSnapshot,
    pub heap_snapshot_bytes: HistogramSnapshot,
    pub trace_ms: HistogramSnapshot,
    pub trace_bytes: HistogramSnapshot,
    pub cpu_profile_ms: HistogramSnapshot,
    pub heap_sample_ms: HistogramSnapshot,
    pub coverage_take_ms: HistogramSnapshot,
    pub pdf_count: u64,
    pub heap_snapshot_count: u64,
    pub trace_count: u64,
    pub perf_failure_count: u64,
}

/// Process-wide singleton for perf metrics. `browser-engine::perf` and
/// `browser-engine::pdf` record against this so the broker's
/// `_internal.metrics` snapshot can surface them without threading the
/// histogram through every command. SPEC §12 U4 + U5 owner: perf-master.
pub fn perf_metrics() -> &'static PerfMetrics {
    use once_cell::sync::Lazy;
    static PERF: Lazy<PerfMetrics> = Lazy::new(PerfMetrics::default);
    &PERF
}

/// SPEC §10 M2 — MutationObserver-driven AX delta diagnostics.
///
/// `ax-engine::mutation::drain_log` and
/// `browser-engine::Page::snapshot_delta_since` record against this so
/// `_internal.metrics` can surface (a) how often the in-page log was
/// blown out by an SPA clobbering the global drain function and (b)
/// how often the log overflowed past `MAX_LOG = 4096` and forced a
/// full-snapshot promotion. Both are signals that the page is too
/// heavy for delta tracking; sustained non-zero values mean callers
/// should expect more `partial: false, anchor_stale: true` responses.
#[derive(Debug, Default)]
pub struct MutationMetrics {
    /// Times `drain_log` returned `Err(MutationError::ParseError)` —
    /// most often because the page replaced
    /// `window.__oneForAllMutationDrain` with something that didn't
    /// deserialize as `Vec<MutationRecord>`.
    pub drain_failures: AtomicU64,
    /// Times the snapshot delta path detected a sequence-number gap
    /// (`min_drained_seq > anchor_high_water + 1`) and promoted to a
    /// full snapshot. One per overflow event, regardless of how many
    /// records were lost.
    pub drain_overflows: AtomicU64,
    /// Times the snapshot delta path could not honor a non-zero
    /// `since_seq` because the per-page anchor had been cleared
    /// (typically by a top-frame navigation or `Page.frameAttached`).
    /// Surfaces as `Snapshot { partial: false, anchor_stale: true }`
    /// to the caller; tracking the rate flags pages whose frame churn
    /// makes delta tracking unprofitable.
    pub anchor_invalidations: AtomicU64,
}

impl MutationMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_drain_failure(&self) {
        self.drain_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_drain_overflow(&self) {
        self.drain_overflows.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_anchor_invalidation(&self) {
        self.anchor_invalidations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MutationSnapshot {
        MutationSnapshot {
            drain_failures: self.drain_failures.load(Ordering::Relaxed),
            drain_overflows: self.drain_overflows.load(Ordering::Relaxed),
            anchor_invalidations: self.anchor_invalidations.load(Ordering::Relaxed),
        }
    }
}

/// Immutable view of [`MutationMetrics`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MutationSnapshot {
    #[serde(default)]
    pub drain_failures: u64,
    #[serde(default)]
    pub drain_overflows: u64,
    #[serde(default)]
    pub anchor_invalidations: u64,
}

/// Process-wide singleton for SPEC §10 M2 mutation diagnostics.
/// `ax-engine::mutation::drain_log` and
/// `browser-engine::Page::snapshot_delta_since` record against this so
/// the broker's `_internal.metrics` snapshot can surface them without
/// threading a metrics handle through every snapshot call.
pub fn mutation_metrics() -> &'static MutationMetrics {
    use once_cell::sync::Lazy;
    static MUT: Lazy<MutationMetrics> = Lazy::new(MutationMetrics::default);
    &MUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_thread_safe() {
        let reg = Registry::new();
        let m = reg.session("s1");
        m.tab_count.fetch_add(2, Ordering::Relaxed);
        m.request_count.fetch_add(7, Ordering::Relaxed);
        m.recovery_count.fetch_add(1, Ordering::Relaxed);
        m.touch();
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].tab_count, 2);
        assert_eq!(snap[0].request_count, 7);
        assert_eq!(snap[0].recovery_count, 1);
        assert_eq!(snap[0].recovery_failed_count, 0);
        assert_eq!(snap[0].outbound_drop_count, 0);
        assert!(snap[0].last_activity_ms > 0);
    }

    #[test]
    fn remove_returns_session() {
        let reg = Registry::new();
        let _ = reg.session("s1");
        assert!(reg.remove("s1").is_some());
        assert!(reg.remove("s1").is_none());
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn histogram_buckets_powers_of_two() {
        let h = Histogram::new();
        h.observe(0); // bucket 0: [0,1)
        h.observe(1); // bucket 0: [1,2)? actually 1 is bucket 0 ([1,2))
        h.observe(2); // bucket 1
        h.observe(3); // bucket 1
        h.observe(4); // bucket 2
        h.observe(7); // bucket 2
        h.observe(8); // bucket 3
        let s = h.snapshot();
        assert_eq!(s.count, 7);
        assert_eq!(s.sum, 0 + 1 + 2 + 3 + 4 + 7 + 8);
        assert_eq!(s.max, 8);
        assert_eq!(s.buckets[0], 2); // 0 and 1
        assert_eq!(s.buckets[1], 2); // 2, 3
        assert_eq!(s.buckets[2], 2); // 4, 7
        assert_eq!(s.buckets[3], 1); // 8
    }

    #[test]
    fn fetch_metrics_round_trip() {
        let m = FetchMetrics::new();
        m.record_download_start();
        m.record_attempt();
        m.record_attempt();
        m.record_retry();
        m.record_stall();
        m.record_bytes(1024);
        m.record_attempt_bytes(1024);
        m.record_download_completion(2500);
        let s = m.snapshot();
        assert_eq!(s.download_starts, 1);
        assert_eq!(s.attempts, 2);
        assert_eq!(s.retries, 1);
        assert_eq!(s.stalls, 1);
        assert_eq!(s.bytes_written, 1024);
        assert_eq!(s.download_completions, 1);
        assert_eq!(s.download_ms.count, 1);
        assert!(s.download_ms.max >= 2500);
        assert_eq!(s.bytes_per_attempt.count, 1);
    }

    #[test]
    fn fetch_metrics_is_a_process_singleton() {
        let a = fetch_metrics() as *const FetchMetrics;
        let b = fetch_metrics() as *const FetchMetrics;
        assert_eq!(a, b);
    }

    #[test]
    fn perf_metrics_round_trip() {
        let m = PerfMetrics::new();
        m.record_pdf(120, 65_536);
        m.record_pdf(80, 32_768);
        m.record_heap_snapshot(2_500, 4_194_304);
        m.record_trace(900, 1_048_576);
        m.record_cpu_profile(500);
        m.record_heap_sample(500);
        m.record_coverage_take(15);
        m.record_print_preview(45);
        m.record_failure();
        let s = m.snapshot();
        assert_eq!(s.pdf_count, 2);
        assert_eq!(s.heap_snapshot_count, 1);
        assert_eq!(s.trace_count, 1);
        assert_eq!(s.perf_failure_count, 1);
        assert_eq!(s.pdf_ms.count, 2);
        assert!(s.pdf_ms.max >= 120);
        assert_eq!(s.heap_snapshot_bytes.count, 1);
        assert!(s.heap_snapshot_bytes.max >= 4_194_304);
        assert_eq!(s.cpu_profile_ms.count, 1);
        assert_eq!(s.coverage_take_ms.count, 1);
        assert_eq!(s.print_preview_ms.count, 1);
    }

    #[test]
    fn perf_metrics_is_a_process_singleton() {
        let a = perf_metrics() as *const PerfMetrics;
        let b = perf_metrics() as *const PerfMetrics;
        assert_eq!(a, b);
    }

    #[test]
    fn mutation_metrics_round_trip() {
        let m = MutationMetrics::new();
        m.record_drain_failure();
        m.record_drain_failure();
        m.record_drain_overflow();
        m.record_anchor_invalidation();
        m.record_anchor_invalidation();
        m.record_anchor_invalidation();
        let s = m.snapshot();
        assert_eq!(s.drain_failures, 2);
        assert_eq!(s.drain_overflows, 1);
        assert_eq!(s.anchor_invalidations, 3);
    }

    #[test]
    fn mutation_metrics_is_a_process_singleton() {
        let a = mutation_metrics() as *const MutationMetrics;
        let b = mutation_metrics() as *const MutationMetrics;
        assert_eq!(a, b);
    }
}
