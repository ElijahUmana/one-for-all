//! SPEC §11 V5 — `sandbox_spawn_p99` SLO bench.
//!
//! Target: < 3 s p99 for cold sandbox spawn (APFS clone of user Chrome
//! profile + Chromium boot to handshake-complete).
//!
//! The V3 milestone (#16, sandbox-architect) ships
//! `sandbox::spawn_cold(...)`. Until that lands the bench is gated on
//! `BENCH_SANDBOX_V3=1`. When unset it emits a `BENCH_RESULT` line with
//! `kind: "skipped"` and `passed: true` plus a stderr WARN —
//! `scripts/ci-bench-gate.sh` surfaces a tracked WARN in the CI output.
//! Never a silent skip: an unrun gate must be visible in CI output.

use std::time::Instant;

use bench::{assert_slo, hist, sandbox_v3_enabled, SloMode};
use criterion::{criterion_group, criterion_main, Criterion};
use observability::{LatencySnapshot, LatencyTimer};

const SLO_P99_US: u64 = 3_000_000; // 3 s
const SPAWNS: usize = 5;

#[derive(Debug, serde::Serialize)]
struct BenchSkipped {
    name: &'static str,
    kind: &'static str,
    target: u64,
    measured: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    count: u64,
    passed: bool,
    reason: &'static str,
}

fn report_skipped() {
    let v = BenchSkipped {
        name: "sandbox_spawn_p99",
        kind: "skipped",
        target: SLO_P99_US,
        measured: 0,
        p50_us: 0,
        p95_us: 0,
        p99_us: 0,
        max_us: 0,
        count: 0,
        passed: true,
        reason: "BENCH_SANDBOX_V3 not set; V3 milestone (#16) gates real measurement",
    };
    let line = serde_json::to_string(&v).unwrap_or_default();
    println!("BENCH_RESULT={line}");
    eprintln!("[sandbox_spawn] WARN: V3 not enabled; export BENCH_SANDBOX_V3=1 once #16 ships");
}

fn bench_sandbox_spawn(c: &mut Criterion) {
    if !sandbox_v3_enabled() {
        c.bench_function("sandbox_spawn_p99_skipped", |b| {
            b.iter(|| {
                std::hint::black_box(0u8);
            });
        });
        report_skipped();
        return;
    }

    let mode = SloMode::from_env();
    let h = hist("sandbox_spawn");

    c.bench_function("sandbox_spawn_p99", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let _t = LatencyTimer::new(&h);
                spawn_one_cold();
            }
            start.elapsed()
        });
    });

    h.reset();
    for _ in 0..SPAWNS {
        let _t = LatencyTimer::new(&h);
        spawn_one_cold();
    }
    let snap: LatencySnapshot = h.snapshot();
    let passed = bench::report_p99("sandbox_spawn_p99", &snap, SLO_P99_US);
    assert_slo(
        passed,
        mode,
        &format!(
            "sandbox_spawn_p99 missed: p99_us {} (target {SLO_P99_US})",
            snap.p99_us
        ),
    );
}

fn spawn_one_cold() {
    panic!(
        "BENCH_SANDBOX_V3 was set but the V3 crate is not yet wired in this build. \
         Coordinate with sandbox-architect (#16) to expose `sandbox::spawn_cold` \
         before flipping the gate."
    );
}

criterion_group!(benches, bench_sandbox_spawn);
criterion_main!(benches);
