//! SPEC §11 V5 — `vision_find_text_p99` SLO bench.
//!
//! Target: < 10 ms p99 for `vision.find_text` against the pre-OCR'd
//! cache (1000 entries — a realistic page).
//!
//! The vision-architect (#18) owns the cache structure; this bench
//! exercises the lookup contract — given a Vec of OCR entries, find
//! the entry whose `text` matches a query — and locks the SLO.

use std::time::{Duration, Instant};

use bench::{assert_slo, hist, report_p99, SloMode};
use broker::protocol::OcrEntry;
use criterion::{criterion_group, criterion_main, Criterion};
use observability::LatencyTimer;

const SLO_P99_US: u64 = 10_000; // 10 ms
const QUERIES: usize = 5_000;
const CACHE_SIZE: usize = 1_000;

fn build_cache() -> Vec<OcrEntry> {
    let mut out = Vec::with_capacity(CACHE_SIZE);
    for i in 0..CACHE_SIZE {
        out.push(OcrEntry {
            x: (i % 50) as u32 * 16,
            y: (i / 50) as u32 * 24,
            w: 100,
            h: 24,
            text: format!("entry_{i:04}"),
            confidence: 0.9,
        });
    }
    out
}

fn find_text<'a>(cache: &'a [OcrEntry], query: &str) -> Option<&'a OcrEntry> {
    cache.iter().find(|e| e.text.contains(query))
}

fn bench_vision_find_text(c: &mut Criterion) {
    let mode = SloMode::from_env();
    let h = hist("vision_find_text");
    let cache = build_cache();

    c.bench_function("vision_find_text_p99", |b| {
        b.iter(|| {
            let _t = LatencyTimer::new(&h);
            for q in 0..QUERIES {
                let needle = format!("entry_{:04}", q % CACHE_SIZE);
                let _ = find_text(&cache, &needle);
            }
        });
    });

    h.reset();
    let start = Instant::now();
    for q in 0..QUERIES {
        let needle = format!("entry_{:04}", q % CACHE_SIZE);
        let _t = LatencyTimer::new(&h);
        let _ = find_text(&cache, &needle);
    }
    let total: Duration = start.elapsed();
    let snap = h.snapshot();
    let passed = report_p99("vision_find_text_p99", &snap, SLO_P99_US);
    assert_slo(
        passed,
        mode,
        &format!(
            "vision_find_text_p99 missed: p99_us {} (target {SLO_P99_US}); total {total:?}",
            snap.p99_us
        ),
    );
}

criterion_group!(benches, bench_vision_find_text);
criterion_main!(benches);
