//! SPEC §11 V5 — `frame_capture_to_event_p99` SLO bench.
//!
//! Target: < 50 ms p99 from a synthetic frame producer pushing into the
//! shared-memory ring through bincode encoding on the broker socket out
//! to a consumer that decodes the bincode envelope.
//!
//! The vision-architect (#18) owns the actual frame-ring library at
//! `crates/vision/src/frame_ring.rs`. Until it lands this bench drives
//! the bincode envelope path against an in-memory pipe — which is the
//! hop SPEC §11 V5 names as the one that must hit p99 < 50 ms.

use std::time::{Duration, Instant};

use bench::{assert_slo, hist, report_p99, stub_frame_producer, SloMode};
use broker::protocol::{
    decode_line, encode_vision_frame_into, FrameHandle, OcrEntry, TileRect, Viewport,
    VisionFrameEvent, WireFrame,
};
use criterion::{criterion_group, criterion_main, Criterion};
use observability::LatencyTimer;

const SLO_P99_US: u64 = 50_000; // 50 ms
const FRAMES: usize = 100;

fn make_event(seq: u64) -> VisionFrameEvent {
    VisionFrameEvent {
        session_id: "bench".into(),
        tab_id: "T1".into(),
        ts_ms: stub_frame_producer::now_ms(),
        frame_seq: seq,
        captured_us: seq * 1_000,
        frame_handle: FrameHandle {
            ring_path: "/tmp/ofa-frame-ring-bench.bin".into(),
            slot: (seq % 128) as u32,
            slot_seq: seq,
            offset: (seq % 128) * 512 * 1024,
            len: 64 * 1024,
            ts_us: seq * 1_000,
        },
        viewport: Viewport {
            offset_top: 0.0,
            page_scale_factor: 2.0,
            device_width: 1280.0,
            device_height: 720.0,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
            timestamp: seq as f64,
        },
        changed_tiles: (0..8u32)
            .map(|i| TileRect {
                tile_x: i,
                tile_y: 0,
                x: i * 64,
                y: 0,
                w: 64,
                h: 64,
                prev_hash: ((seq.saturating_sub(1)) << 16) ^ i as u64,
                next_hash: (seq << 16) ^ i as u64,
            })
            .collect(),
        ocr_delta: vec![OcrEntry {
            x: 32,
            y: 200,
            w: 256,
            h: 24,
            text: "Stub OCR".into(),
            confidence: 0.9,
        }],
        stability: None,
        state: None,
    }
}

fn bench_frame_to_event(c: &mut Criterion) {
    let mode = SloMode::from_env();
    let h = hist("frame_capture_to_event");

    // Pre-build the frames so we measure only encode + decode hop.
    let _frames = stub_frame_producer::build(FRAMES, 64 * 1024);
    let mut scratch = Vec::with_capacity(8 * 1024);

    c.bench_function("frame_capture_to_event_p99", |b| {
        b.iter(|| {
            let _t = LatencyTimer::new(&h);
            for seq in 0..FRAMES as u64 {
                let ev = make_event(seq);
                scratch.clear();
                encode_vision_frame_into(&mut scratch, &ev).expect("encode");
                // Strip trailing newline that the framing inserts so
                // decode_line gets the line content as the broker reader
                // would yield it.
                let line_len = scratch.len() - 1;
                let line = &scratch[..line_len];
                let frame = decode_line(line).expect("decode");
                match frame {
                    WireFrame::VisionFrame(decoded) => {
                        debug_assert_eq!(decoded.frame_seq, seq);
                    }
                    _ => panic!("expected vision frame variant"),
                }
            }
        });
    });

    // Standalone p99 capture across exactly FRAMES iterations.
    h.reset();
    let mut scratch_solo = Vec::with_capacity(8 * 1024);
    let start = Instant::now();
    for seq in 0..FRAMES as u64 {
        let _t = LatencyTimer::new(&h);
        let ev = make_event(seq);
        scratch_solo.clear();
        encode_vision_frame_into(&mut scratch_solo, &ev).expect("encode");
        let line_len = scratch_solo.len() - 1;
        let line = &scratch_solo[..line_len];
        let _ = decode_line(line).expect("decode");
    }
    let total: Duration = start.elapsed();
    let snap = h.snapshot();
    let passed = report_p99("frame_capture_to_event_p99", &snap, SLO_P99_US);
    assert_slo(
        passed,
        mode,
        &format!(
            "frame_capture_to_event_p99 missed: p99_us {} (target {SLO_P99_US}); total {total:?}",
            snap.p99_us
        ),
    );
}

criterion_group!(benches, bench_frame_to_event);
criterion_main!(benches);
