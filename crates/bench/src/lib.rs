//! Shared harness for SPEC §11 V5 SLO benchmarks.
//!
//! The benches in this crate are not micro-benchmarks of the kind
//! criterion ships for by default. They measure end-to-end p99 latencies
//! against real Chromium / real broker flows and assert against the
//! SPEC §11 V4 SLO table:
//!
//! | Path | Target |
//! |---|---|
//! | `cdp_request_per_sec` | ≥ 10k req/sec/session |
//! | `frame_capture_to_event_p99` | < 50ms |
//! | `page_click_p99` | < 100ms |
//! | `vision_find_text_p99` | < 10ms |
//! | `sandbox_spawn_p99` (cold) | < 3s |
//!
//! Each bench:
//!
//! 1. Records per-iteration latency into an [`observability::LatencyHistogram`].
//! 2. Emits a structured `BENCH_RESULT` line (JSON) on stdout so the
//!    `scripts/ci-bench-gate.sh` harness can parse and gate.
//! 3. Asserts on its own SLO via `assert_slo()`.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use observability::{LatencyHistogram, LatencySnapshot};
use serde::Serialize;

/// Whether to fail-loud or warn-and-continue when an SLO is missed. The
/// default is fail-loud (`mode == Strict`); CI passes `BENCH_MODE=warn`
/// for triage runs that want a histogram dump even on a miss.
#[derive(Debug, Clone, Copy)]
pub enum SloMode {
    Strict,
    Warn,
}

impl SloMode {
    pub fn from_env() -> Self {
        match std::env::var("BENCH_MODE").ok().as_deref() {
            Some("warn") => Self::Warn,
            _ => Self::Strict,
        }
    }
}

/// Metadata published on the `BENCH_RESULT` stdout line. The CI gate
/// parses one of these per bench and asserts against `target`.
#[derive(Debug, Serialize)]
pub struct BenchResult {
    pub name: &'static str,
    pub kind: &'static str, // "p99_us" | "throughput_per_sec"
    pub target: u64,
    pub measured: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub count: u64,
    pub passed: bool,
}

/// Build a fresh histogram suited to per-bench latency capture.
pub fn hist(name: &'static str) -> Arc<LatencyHistogram> {
    LatencyHistogram::shared(name)
}

/// Convenience: drop a measured snapshot to stdout in the
/// `BENCH_RESULT=…` shape. Returns whether the SLO passed.
pub fn report_p99(name: &'static str, snap: &LatencySnapshot, target_us: u64) -> bool {
    let passed = snap.p99_us <= target_us;
    let result = BenchResult {
        name,
        kind: "p99_us",
        target: target_us,
        measured: snap.p99_us,
        p50_us: snap.p50_us,
        p95_us: snap.p95_us,
        p99_us: snap.p99_us,
        max_us: snap.max_us,
        count: snap.count,
        passed,
    };
    let line = serde_json::to_string(&result).unwrap_or_default();
    // Emit on stdout so criterion's stderr capture doesn't swallow it.
    println!("BENCH_RESULT={line}");
    passed
}

/// Reporter for throughput benches (`cdp_request_per_sec`).
pub fn report_throughput(
    name: &'static str,
    measured_per_sec: u64,
    target_per_sec: u64,
    snap: &LatencySnapshot,
) -> bool {
    let passed = measured_per_sec >= target_per_sec;
    let result = BenchResult {
        name,
        kind: "throughput_per_sec",
        target: target_per_sec,
        measured: measured_per_sec,
        p50_us: snap.p50_us,
        p95_us: snap.p95_us,
        p99_us: snap.p99_us,
        max_us: snap.max_us,
        count: snap.count,
        passed,
    };
    let line = serde_json::to_string(&result).unwrap_or_default();
    println!("BENCH_RESULT={line}");
    passed
}

/// Assert against `passed`. In `Warn` mode, log and continue.
pub fn assert_slo(passed: bool, mode: SloMode, msg: &str) {
    if !passed {
        match mode {
            SloMode::Strict => panic!("SLO miss: {msg}"),
            SloMode::Warn => eprintln!("SLO miss (warn): {msg}"),
        }
    }
}

/// Whether real-Chromium-launching benches are enabled. Off by default
/// because CI environments may not have the binary fetched. Set
/// `BENCH_REAL_CHROMIUM=1` to opt in.
pub fn real_chromium_enabled() -> bool {
    matches!(
        std::env::var("BENCH_REAL_CHROMIUM").ok().as_deref(),
        Some("1")
    )
}

/// Whether the V3 sandbox-spawn bench is enabled. Off by default — the
/// V3 milestone (#16) ships the `sandbox::spawn_cold` API; until it
/// merges the bench is feature-gated and `scripts/ci-bench-gate.sh`
/// surfaces a tracked WARN (no silent skip per "no silent caps").
pub fn sandbox_v3_enabled() -> bool {
    matches!(std::env::var("BENCH_SANDBOX_V3").ok().as_deref(), Some("1"))
}

/// Stub frame-ring producer: writes synthetic frames to a memory-backed
/// ring without going through Chromium's screencast API. Useful for the
/// `frame_capture_to_event` bench — it isolates the broker→bincode→
/// mcp-server hop and the consumer drain from the upstream capture
/// stage. SPEC §11 V5 declares this hop as the one that must hit
/// <50 ms p99 for the binary topic to be worthwhile.
pub mod stub_frame_producer {
    use std::path::PathBuf;
    use std::time::Instant;

    /// One synthetic frame: 64 KiB random-ish payload.
    pub struct StubFrame {
        pub seq: u64,
        pub ts_ms: u64,
        pub bytes: Vec<u8>,
    }

    /// Build N synthetic frames. Deterministic for reproducible benches.
    pub fn build(n: usize, payload_bytes: usize) -> Vec<StubFrame> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let mut bytes = vec![0u8; payload_bytes];
            // Vary tile content per frame so the diff layer in vision-architect's
            // pipeline doesn't short-circuit to "no change". We don't read the
            // payload here, but downstream consumers may.
            for (j, b) in bytes.iter_mut().enumerate() {
                *b = ((i.wrapping_mul(31) + j) & 0xFF) as u8;
            }
            out.push(StubFrame {
                seq: i as u64,
                ts_ms: i as u64,
                bytes,
            });
        }
        out
    }

    /// Path under the bench tempdir for the (real or simulated) ring.
    pub fn ring_path(dir: &std::path::Path, session_id: &str) -> PathBuf {
        dir.join(format!("frame-ring-{session_id}.bin"))
    }

    /// Convenience: monotonic millisecond timestamp for synthetic events.
    pub fn now_ms() -> u64 {
        // Bench-local epoch; we only care about deltas between frames.
        let start = once_cell();
        start.elapsed().as_millis() as u64
    }

    fn once_cell() -> &'static Instant {
        use std::sync::OnceLock;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_p99_pass_path() {
        let snap = LatencySnapshot {
            p50_us: 10,
            p95_us: 40,
            p99_us: 49_000,
            max_us: 60_000,
            count: 100,
        };
        assert!(report_p99("test", &snap, 50_000));
    }

    #[test]
    fn report_p99_fail_path() {
        let snap = LatencySnapshot {
            p50_us: 10,
            p95_us: 40,
            p99_us: 51_000,
            max_us: 80_000,
            count: 100,
        };
        assert!(!report_p99("test", &snap, 50_000));
    }

    #[test]
    fn stub_frames_have_distinct_payloads() {
        let frames = stub_frame_producer::build(4, 64);
        // Frame 0 vs frame 1 should differ — diff stage must not see "no change".
        assert_ne!(frames[0].bytes, frames[1].bytes);
        assert_eq!(frames[0].seq, 0);
        assert_eq!(frames[3].seq, 3);
    }
}
