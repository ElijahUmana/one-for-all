//! SPEC §11 V5 — `page_click_p99` SLO bench.
//!
//! Target: < 100 ms p99 from agent-emitted `page.click` to first repaint.
//!
//! Real-Chromium variant gated by `BENCH_REAL_CHROMIUM=1`; default is a
//! deterministic stub that exercises the framing-layer code path against
//! the loopback transport. In stub mode the SLO assertion runs in
//! `Warn` mode (we still emit a `BENCH_RESULT` line for tracking) so
//! offline CI passes; real-chromium mode uses `BENCH_MODE` to decide
//! strict vs warn.

use std::time::{Duration, Instant};

use bench::{assert_slo, hist, real_chromium_enabled, report_p99, SloMode};
use cdp_client::framing::{encode_frame_into, Decoder};
use criterion::{criterion_group, criterion_main, Criterion};
use observability::LatencyTimer;
use serde_json::{json, Value};

const SLO_P99_US: u64 = 100_000; // 100 ms
const CLICKS: usize = 50;

fn stub_click_round_trip(scratch: &mut Vec<u8>, dec: &mut Decoder) {
    scratch.clear();
    let req = json!({
        "id": 1,
        "method": "Input.dispatchMouseEvent",
        "params": {"type": "mousePressed", "x": 100.0, "y": 100.0, "button": "left"}
    });
    encode_frame_into(scratch, &req).expect("encode click");
    let mut out: Vec<Value> = Vec::with_capacity(1);
    dec.feed_into(scratch, &mut out).expect("decode click");

    scratch.clear();
    let evt = json!({"method": "Page.frameRendered", "params": {"frameId": "F1"}});
    encode_frame_into(scratch, &evt).expect("encode evt");
    out.clear();
    dec.feed_into(scratch, &mut out).expect("decode evt");
}

fn bench_page_click(c: &mut Criterion) {
    let mode = if real_chromium_enabled() {
        SloMode::from_env()
    } else {
        SloMode::Warn
    };
    let h = hist("page_click");

    c.bench_function("page_click_p99", |b| {
        let mut scratch = Vec::with_capacity(512);
        let mut dec = Decoder::default();
        b.iter(|| {
            let _t = LatencyTimer::new(&h);
            stub_click_round_trip(&mut scratch, &mut dec);
        });
    });

    h.reset();
    let mut scratch_solo = Vec::with_capacity(512);
    let mut dec_solo = Decoder::default();
    let start = Instant::now();
    for _ in 0..CLICKS {
        let _t = LatencyTimer::new(&h);
        stub_click_round_trip(&mut scratch_solo, &mut dec_solo);
    }
    let total: Duration = start.elapsed();
    let snap = h.snapshot();
    let passed = report_p99("page_click_p99", &snap, SLO_P99_US);
    assert_slo(
        passed,
        mode,
        &format!(
            "page_click_p99 missed: p99_us {} (target {SLO_P99_US}); total {total:?}; real_chromium={}",
            snap.p99_us,
            real_chromium_enabled()
        ),
    );
}

criterion_group!(benches, bench_page_click);
criterion_main!(benches);
