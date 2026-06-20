//! `vision.pixel {tab_id, x, y}` — direct mmap RGBA read of the latest
//! decoded frame at the requested CSS-pixel coordinate.
//!
//! Hot-path discipline: the latest decoded frame is held behind an
//! `RwLock<Option<Frame>>` in `api::PipelineInner`; we hold the read guard
//! only long enough to clone the `Arc<DecodedImage>`, so the actual byte
//! load runs lock-free.

use serde::{Deserialize, Serialize};

use crate::api::VisionPipeline;
use crate::coords::{sanitize_scale, DEFAULT_DISPLAY_ID};
use crate::types::VisionError;

/// One RGBA8 pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PixelRgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
    pub display_id: u32,
}

/// Read the RGBA8 pixel at `(x, y)` in CSS-px from the latest captured
/// frame. Returns `Err(NotEnabled)` when no frame has been captured yet.
pub fn pixel_at(pipeline: &VisionPipeline, x: u32, y: u32) -> Result<PixelRgba, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let scale = sanitize_scale(frame.width as f64 / frame.width.max(1) as f64); // 1.0 — we don't have meta here.
                                                                                // The scale we want is page_scale_factor — pulled separately when known.
    let scale = sanitize_scale(pipeline.last_page_scale().unwrap_or(scale));
    let dx = (x as f64 * scale).round() as u32;
    let dy = (y as f64 * scale).round() as u32;
    if dx >= frame.width || dy >= frame.height {
        return Err(VisionError::Other(anyhow::anyhow!(
            "pixel ({x},{y}) -> ({dx},{dy}) out of bounds for {}x{}",
            frame.width,
            frame.height
        )));
    }
    let idx = ((dy as usize) * (frame.width as usize) + dx as usize) * 4;
    let bytes: &[u8] = frame.rgba.as_slice();
    if idx + 4 > bytes.len() {
        return Err(VisionError::Other(anyhow::anyhow!("rgba buffer truncated")));
    }
    Ok(PixelRgba {
        r: bytes[idx],
        g: bytes[idx + 1],
        b: bytes[idx + 2],
        a: bytes[idx + 3],
        display_id: DEFAULT_DISPLAY_ID,
    })
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

    fn rgba_frame(w: u32, h: u32, fill: [u8; 4]) -> Frame {
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for px in bytes.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        Frame {
            seq: 1,
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
                captured_us: 0,
            },
            decoded: Some(DecodedImage {
                width: w,
                height: h,
                rgba: Arc::new(bytes),
                captured_us: 0,
            }),
        }
    }

    #[tokio::test]
    async fn reads_rgba_at_xy_from_synthesized_frame() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // Inject a frame.
        p.tick(rgba_frame(64, 64, [10, 20, 30, 255]))
            .await
            .expect("tick");
        let px = pixel_at(&p, 10, 5).expect("pixel");
        assert_eq!(px.r, 10);
        assert_eq!(px.g, 20);
        assert_eq!(px.b, 30);
        assert_eq!(px.a, 255);
    }

    #[tokio::test]
    async fn out_of_bounds_returns_err() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(rgba_frame(8, 8, [0, 0, 0, 0])).await.expect("tick");
        assert!(pixel_at(&p, 100, 100).is_err());
    }

    #[tokio::test]
    async fn no_frame_yet_returns_not_enabled() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        let e = pixel_at(&p, 0, 0).err().expect("err");
        assert!(matches!(e, VisionError::NotEnabled));
    }
}
