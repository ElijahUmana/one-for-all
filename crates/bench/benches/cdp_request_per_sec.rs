//! SPEC §11 V5 — `cdp_request_per_sec` SLO bench.
//!
//! Target: ≥ 10 000 `Browser.getVersion` requests / second / session.
//!
//! This bench measures the round-trip latency of a CDP request over the
//! `cdp-client` framing layer using an in-memory loopback transport. It
//! isolates framing + serde cost from the Chromium subprocess; that
//! end-to-end path is benched in `page_click`. The SLO target is on the
//! framing layer — the broker must add zero measurable overhead on top.

use std::time::Instant;

use bench::{assert_slo, hist, report_throughput, SloMode};
use cdp_client::framing::{encode_frame_into, Decoder};
use criterion::{criterion_group, criterion_main, Criterion};
use observability::LatencyTimer;
use serde_json::{json, Value};

const SLO_REQ_PER_SEC: u64 = 10_000;
const ITERATIONS: usize = 50_000;

fn loopback_round_trip(scratch_req: &mut Vec<u8>, scratch_resp: &mut Vec<u8>, dec: &mut Decoder) {
    scratch_req.clear();
    let req = json!({"id": 1, "method": "Browser.getVersion"});
    encode_frame_into(scratch_req, &req).expect("encode request");

    // Server-side decode (a single-shot decoder per round-trip avoids
    // accidentally amortizing decoder reuse into a single side of the
    // hop and overstating throughput).
    let mut server_dec = Decoder::default();
    let mut server_out: Vec<Value> = Vec::with_capacity(1);
    server_dec
        .feed_into(scratch_req, &mut server_out)
        .expect("server decode");
    debug_assert_eq!(server_out.len(), 1);

    scratch_resp.clear();
    let resp = json!({
        "id": 1,
        "result": {
            "protocolVersion": "1.3",
            "product": "HeadlessChrome/149.0",
            "revision": "@abcdef",
            "userAgent": "...",
            "jsVersion": "..."
        }
    });
    encode_frame_into(scratch_resp, &resp).expect("encode response");

    let mut out: Vec<Value> = Vec::with_capacity(1);
    dec.feed_into(scratch_resp, &mut out)
        .expect("client decode");
    debug_assert_eq!(out.len(), 1);
}

fn bench_cdp_request_per_sec(c: &mut Criterion) {
    let mode = SloMode::from_env();
    let h = hist("cdp_request_per_sec");

    c.bench_function("cdp_request_per_sec_loopback", |b| {
        let mut scratch_req = Vec::with_capacity(256);
        let mut scratch_resp = Vec::with_capacity(512);
        let mut dec = Decoder::default();
        b.iter(|| {
            let _t = LatencyTimer::new(&h);
            loopback_round_trip(&mut scratch_req, &mut scratch_resp, &mut dec);
        });
    });

    // Throughput pass — drives ITERATIONS round-trips and measures
    // wall-clock per-second rate against the SLO.
    let mut scratch_req = Vec::with_capacity(256);
    let mut scratch_resp = Vec::with_capacity(512);
    let mut dec = Decoder::default();
    h.reset();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let _t = LatencyTimer::new(&h);
        loopback_round_trip(&mut scratch_req, &mut scratch_resp, &mut dec);
    }
    let elapsed = start.elapsed();
    let per_sec = (ITERATIONS as f64 / elapsed.as_secs_f64()) as u64;
    let snap = h.snapshot();
    let passed = report_throughput("cdp_request_per_sec", per_sec, SLO_REQ_PER_SEC, &snap);
    assert_slo(
        passed,
        mode,
        &format!(
            "cdp_request_per_sec: measured {per_sec} req/s vs target {SLO_REQ_PER_SEC} (elapsed {:?}, p99_us {})",
            elapsed, snap.p99_us
        ),
    );
}

criterion_group!(benches, bench_cdp_request_per_sec);
criterion_main!(benches);
