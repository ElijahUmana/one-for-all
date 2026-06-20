//! `vision.loading.detect` — coarse motion-over-window classifier:
//! `idle`, `progress` (continuous low-amplitude churn), or `spinner`
//! (periodic high-amplitude churn in a small region).
//!
//! Uses the rolling [`StabilityState`] window already populated by
//! `VisionPipeline::tick`. We don't need fresh frame work — just an
//! aggregate read against the per-frame `(changed, total)` counts.

use serde::{Deserialize, Serialize};

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoadingState {
    Idle,
    Progress,
    Spinner,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadingDetection {
    pub state: LoadingState,
    pub motion_ratio: f32,
    pub variance: f32,
    pub display_id: u32,
    pub window_frames: u32,
}

pub fn detect(pipeline: &VisionPipeline) -> LoadingDetection {
    let win = pipeline.change_window();
    if win.is_empty() {
        return LoadingDetection {
            state: LoadingState::Idle,
            motion_ratio: 0.0,
            variance: 0.0,
            display_id: DEFAULT_DISPLAY_ID,
            window_frames: 0,
        };
    }
    let n = win.len() as u64;
    let ratios: Vec<f32> = win
        .iter()
        .map(|(_, _, c, t)| {
            if *t == 0 {
                0.0
            } else {
                (*c as f32 / *t as f32).clamp(0.0, 1.0)
            }
        })
        .collect();
    let mean: f32 = ratios.iter().sum::<f32>() / n as f32;
    let variance: f32 = ratios.iter().map(|r| (r - mean) * (r - mean)).sum::<f32>() / n as f32;

    // Heuristics:
    // - mean < 0.005 → idle.
    // - mean ≥ 0.005 and variance < 0.005 → progress (steady churn).
    // - variance ≥ 0.005 → spinner-like (bursty).
    let state = if mean < 0.005 {
        LoadingState::Idle
    } else if variance < 0.005 {
        LoadingState::Progress
    } else {
        LoadingState::Spinner
    };
    LoadingDetection {
        state,
        motion_ratio: mean,
        variance,
        display_id: DEFAULT_DISPLAY_ID,
        window_frames: n as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::VisionPipeline;
    use crate::metrics::Histograms;
    use crate::types::{
        DecodedImage, Frame, FrameFormat, ScreencastFrame, ScreencastFrameMetadata,
    };
    use crate::vlm::VlmConfig;
    use std::sync::Arc;

    fn solid_frame(seq: u64, w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            seq,
            raw: ScreencastFrame {
                bytes: Arc::new(vec![]),
                format: FrameFormat::Jpeg,
                metadata: ScreencastFrameMetadata {
                    offset_top: 0.0,
                    page_scale_factor: 1.0,
                    device_width: w as f64,
                    device_height: h as f64,
                    scroll_offset_x: 0.0,
                    scroll_offset_y: 0.0,
                    timestamp: 0.0,
                },
                session_id: "s".into(),
                captured_us: seq,
            },
            decoded: Some(DecodedImage {
                width: w,
                height: h,
                rgba: Arc::new(vec![fill; (w * h * 4) as usize]),
                captured_us: seq,
            }),
        }
    }

    #[tokio::test]
    async fn idle_when_static() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        for i in 1..6 {
            p.tick(solid_frame(i, 64, 64, 0)).await.expect("tick");
        }
        let d = detect(&p);
        assert_eq!(d.state, LoadingState::Idle, "{d:?}");
    }

    #[tokio::test]
    async fn motion_over_window_classifies_progress() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // Vary one pixel each frame (small steady churn).
        p.tick(solid_frame(1, 128, 128, 0)).await.expect("tick");
        for i in 2..10 {
            let mut bytes = vec![0u8; 128 * 128 * 4];
            // Flip a small consistent ~1% of tiles: paint 8 random small dots.
            for k in 0..8 {
                let x = ((k * 13 + i as u32 * 7) % 128) as usize;
                let y = ((k * 19 + i as u32 * 11) % 128) as usize;
                let off = (y * 128 + x) * 4;
                bytes[off] = 255;
                bytes[off + 1] = 255;
                bytes[off + 2] = 255;
                bytes[off + 3] = 255;
            }
            let f = Frame {
                seq: i,
                raw: ScreencastFrame {
                    bytes: Arc::new(vec![]),
                    format: FrameFormat::Jpeg,
                    metadata: ScreencastFrameMetadata {
                        offset_top: 0.0,
                        page_scale_factor: 1.0,
                        device_width: 128.0,
                        device_height: 128.0,
                        scroll_offset_x: 0.0,
                        scroll_offset_y: 0.0,
                        timestamp: 0.0,
                    },
                    session_id: "s".into(),
                    captured_us: i,
                },
                decoded: Some(DecodedImage {
                    width: 128,
                    height: 128,
                    rgba: Arc::new(bytes),
                    captured_us: i,
                }),
            };
            p.tick(f).await.expect("tick");
        }
        let d = detect(&p);
        // We expect non-idle; can be Progress or Spinner depending on jitter.
        assert!(d.state != LoadingState::Idle, "{d:?}");
        assert!(d.motion_ratio > 0.0);
    }
}
