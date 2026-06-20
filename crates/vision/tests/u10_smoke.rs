//! SPEC §12 U10 — end-to-end smoke for the universal sub-granularity
//! vision surface. Runs every U10 + deeper-V4 entry point against a
//! synthesized 1280×800 frame and measures p99 latency. The numbers
//! reported here are CPU-only (no CDP roundtrip), so they capture the
//! per-call cost of the analysis routines themselves — exactly the
//! signal the team-lead summary needs.

use std::sync::Arc;
use std::time::Instant;

use vision::api::VisionPipeline;
use vision::diff::Bbox;
use vision::metrics::Histograms;
use vision::vlm::{ActionContext, VlmConfig};
use vision::{DecodedImage, Frame, FrameFormat, ScreencastFrame, ScreencastFrameMetadata};

const W: u32 = 1280;
const H: u32 = 800;

fn synth_frame(seed: u8) -> Frame {
    let mut bytes = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let off = ((y * W + x) * 4) as usize;
            let r = ((x.wrapping_add(seed as u32)) & 0xFF) as u8;
            let g = ((y.wrapping_add(seed as u32)) & 0xFF) as u8;
            let b = ((x ^ y) & 0xFF) as u8;
            bytes[off] = r;
            bytes[off + 1] = g;
            bytes[off + 2] = b;
            bytes[off + 3] = 255;
        }
    }
    // Scrollbar track + thumb.
    for y in 0..H {
        for x in (W - 16)..W {
            let off = ((y * W + x) * 4) as usize;
            let v = if (200..400).contains(&y) { 100 } else { 230 };
            bytes[off..off + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    Frame {
        seq: seed as u64 + 1,
        raw: ScreencastFrame {
            bytes: Arc::new(vec![]),
            format: FrameFormat::Jpeg,
            metadata: ScreencastFrameMetadata {
                offset_top: 0.0,
                page_scale_factor: 1.0,
                device_width: W as f64,
                device_height: H as f64,
                scroll_offset_x: 0.0,
                scroll_offset_y: 0.0,
                timestamp: 0.0,
            },
            session_id: "smoke".into(),
            captured_us: seed as u64 * 16_667,
        },
        decoded: Some(DecodedImage {
            width: W,
            height: H,
            rgba: Arc::new(bytes),
            captured_us: seed as u64 * 16_667,
        }),
    }
}

struct LatencySample {
    name: &'static str,
    samples_us: Vec<u64>,
}
impl LatencySample {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            samples_us: Vec::with_capacity(256),
        }
    }
    fn record_us(&mut self, us: u64) {
        self.samples_us.push(us);
    }
    fn p(&self, q: f64) -> u64 {
        if self.samples_us.is_empty() {
            return 0;
        }
        let mut v = self.samples_us.clone();
        v.sort_unstable();
        let idx = ((v.len() as f64) * q).clamp(0.0, (v.len() - 1) as f64) as usize;
        v[idx]
    }
}

#[tokio::test]
async fn u10_smoke_dispatches_all_tools_and_meets_slo() {
    let p =
        VisionPipeline::new("smoke", "tab", Histograms::new(), VlmConfig::Off).expect("pipeline");
    p.tick(synth_frame(0)).await.expect("seed");
    p.tick(synth_frame(1)).await.expect("frame2");

    const N: usize = 500;
    let mut samples: Vec<LatencySample> = Vec::new();

    macro_rules! measure_sync {
        ($name:literal, $body:block) => {{
            let mut s = LatencySample::new($name);
            for _ in 0..N {
                let t = Instant::now();
                let r = $body;
                s.record_us(t.elapsed().as_micros() as u64);
                let _ = r;
            }
            samples.push(s);
        }};
    }
    macro_rules! measure_async {
        ($name:literal, $body:block) => {{
            let mut s = LatencySample::new($name);
            for _ in 0..N {
                let t = Instant::now();
                let r = $body;
                s.record_us(t.elapsed().as_micros() as u64);
                let _ = r;
            }
            samples.push(s);
        }};
    }

    measure_sync!("vision.pixel", {
        vision::pixel::pixel_at(&p, 100, 100).expect("pixel")
    });
    measure_async!("vision.region.classify", {
        vision::region_classify::classify(
            &p,
            Bbox {
                x: 64,
                y: 64,
                w: 256,
                h: 192,
            },
        )
        .await
        .expect("classify")
    });
    measure_sync!("vision.color.palette(k=4)", {
        vision::palette::palette(
            &p,
            Some(Bbox {
                x: 64,
                y: 64,
                w: 128,
                h: 96,
            }),
            4,
        )
        .expect("palette")
    });
    measure_sync!("vision.layout.segments", {
        vision::layout::segments(&p).expect("layout")
    });
    measure_sync!("vision.icon.recognize", {
        vision::icon::recognize(
            &p,
            Bbox {
                x: 0,
                y: 0,
                w: 32,
                h: 32,
            },
        )
        .expect("icon")
    });
    measure_sync!("vision.scrollbar.position", {
        vision::scrollbar::scrollbar_position(&p, None).expect("sb")
    });
    measure_sync!("vision.loading.detect", { vision::loading::detect(&p) });
    measure_sync!("vision.tooltip.detect", {
        vision::overlay::tooltip(&p).expect("tt")
    });
    measure_sync!("vision.modal.detect", {
        vision::overlay::modal(&p).expect("modal")
    });
    measure_sync!("vision.changed_since", { p.changed_since(0) });
    measure_sync!("vision.stability", { p.stability_now() });
    measure_async!("vision.diff.semantic(VLM=off)", {
        let prev_frame = p.decoded_frame_by_seq(1).expect("prev frame");
        let next_frame = p.decoded_frame_by_seq(2).expect("next frame");
        vision::semantic_diff::semantic_diff(
            &p,
            1,
            prev_frame,
            2,
            next_frame,
            ActionContext {
                action: "page.click".into(),
                element_ref: None,
                element_text: None,
                note: None,
            },
        )
        .await
        .expect("diff")
    });
    measure_sync!("vision.animation.frames(500ms)", {
        vision::animation::animation_frames(&p, 500).expect("anim")
    });
    measure_sync!("vision.qr_barcode", {
        vision::barcode::scan(&p, None).expect("scan")
    });

    println!("\n========== U10 + deeper-V4 p99 latency table ==========");
    println!("{:32} {:>10} {:>10} {:>10}", "tool", "p50", "p99", "p999");
    println!("{}", "-".repeat(64));
    for s in &samples {
        println!(
            "{:32} {:>8}us {:>8}us {:>8}us",
            s.name,
            s.p(0.50),
            s.p(0.99),
            s.p(0.999)
        );
    }
    println!();

    // SLO assertions — the targets the team-lead summary will quote.
    let pixel_p99 = samples
        .iter()
        .find(|s| s.name == "vision.pixel")
        .unwrap()
        .p(0.99);
    assert!(
        pixel_p99 < 1_000,
        "vision.pixel p99={pixel_p99}us > 1ms target"
    );
    let stab_p99 = samples
        .iter()
        .find(|s| s.name == "vision.stability")
        .unwrap()
        .p(0.99);
    assert!(
        stab_p99 < 1_000,
        "vision.stability p99={stab_p99}us > 1ms target"
    );
    let icon_p99 = samples
        .iter()
        .find(|s| s.name == "vision.icon.recognize")
        .unwrap()
        .p(0.99);
    assert!(
        icon_p99 < 20_000,
        "vision.icon.recognize p99={icon_p99}us > 20ms target"
    );
}
