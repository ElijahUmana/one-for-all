//! `vision.scrollbar.position {region?}` — locate a vertical scrollbar
//! thumb and report its position as a 0..=1 float ratio.
//!
//! Heuristic:
//! 1. Scan a 16-wide vertical strip at the right edge of the frame (or
//!    the right edge of `region` when supplied) for pixels with low edge
//!    density and uniform colour — the scrollbar track.
//! 2. Within the strip, find the longest run of pixels whose colour
//!    diverges from the track baseline by more than `THUMB_DELTA` —
//!    that's the thumb.
//! 3. Position = `thumb_top / (track_h - thumb_h)`.

use serde::Serialize;

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::VisionError;

const STRIP_WIDTH: u32 = 16;
const THUMB_DELTA: u32 = 30;

#[derive(Debug, Clone, Serialize)]
pub struct ScrollbarPosition {
    pub present: bool,
    pub position: f32,
    pub thumb_bbox: Option<Bbox>,
    pub display_id: u32,
}

pub fn scrollbar_position(
    pipeline: &VisionPipeline,
    region: Option<Bbox>,
) -> Result<ScrollbarPosition, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let rect = region.unwrap_or(Bbox {
        x: 0,
        y: 0,
        w: frame.width,
        h: frame.height,
    });
    let rect = crate::coords::clamp_to_image(rect, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame")))?;
    if rect.w < STRIP_WIDTH || rect.h < 8 {
        return Ok(ScrollbarPosition {
            present: false,
            position: 0.0,
            thumb_bbox: None,
            display_id: DEFAULT_DISPLAY_ID,
        });
    }
    let strip_x0 = rect.x + rect.w - STRIP_WIDTH;
    let strip_x1 = rect.x + rect.w;

    // Per-row mean luma in the strip.
    let bytes: &[u8] = frame.rgba.as_slice();
    let stride = (frame.width as usize) * 4;
    let mut row_luma = vec![0u32; rect.h as usize];
    for dy in 0..rect.h {
        let y = rect.y + dy;
        let mut sum = 0u32;
        let mut n = 0u32;
        for x in strip_x0..strip_x1 {
            let off = (y as usize) * stride + (x as usize) * 4;
            if off + 2 >= bytes.len() {
                continue;
            }
            let l = ((bytes[off] as u32) * 30
                + (bytes[off + 1] as u32) * 50
                + (bytes[off + 2] as u32) * 20)
                / 100;
            sum += l;
            n += 1;
        }
        row_luma[dy as usize] = if n == 0 { 0 } else { sum / n };
    }
    // Track baseline = median luma.
    let mut sorted = row_luma.clone();
    sorted.sort_unstable();
    let baseline = sorted[sorted.len() / 2] as i32;

    // Walk for runs that diverge from baseline > THUMB_DELTA.
    let mut best: Option<(u32, u32)> = None;
    let mut start: Option<u32> = None;
    for (i, &v) in row_luma.iter().enumerate() {
        let diverges = (v as i32 - baseline).unsigned_abs() > THUMB_DELTA;
        if diverges {
            if start.is_none() {
                start = Some(i as u32);
            }
        } else if let Some(s) = start {
            let len = i as u32 - s;
            if best.map(|(_, l)| l < len).unwrap_or(true) {
                best = Some((s, len));
            }
            start = None;
        }
    }
    if let Some(s) = start {
        let len = row_luma.len() as u32 - s;
        if best.map(|(_, l)| l < len).unwrap_or(true) {
            best = Some((s, len));
        }
    }

    if let Some((thumb_top_rel, thumb_h)) = best {
        if thumb_h < 4 || thumb_h >= rect.h {
            return Ok(ScrollbarPosition {
                present: false,
                position: 0.0,
                thumb_bbox: None,
                display_id: DEFAULT_DISPLAY_ID,
            });
        }
        let denom = (rect.h - thumb_h).max(1) as f32;
        let pos = (thumb_top_rel as f32 / denom).clamp(0.0, 1.0);
        Ok(ScrollbarPosition {
            present: true,
            position: pos,
            thumb_bbox: Some(Bbox {
                x: strip_x0,
                y: rect.y + thumb_top_rel,
                w: STRIP_WIDTH,
                h: thumb_h,
            }),
            display_id: DEFAULT_DISPLAY_ID,
        })
    } else {
        Ok(ScrollbarPosition {
            present: false,
            position: 0.0,
            thumb_bbox: None,
            display_id: DEFAULT_DISPLAY_ID,
        })
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

    fn frame_with_thumb(thumb_top: u32, thumb_h: u32) -> Frame {
        let w = 256u32;
        let h = 200u32;
        // Light grey track, dark grey thumb, white interior.
        let mut bytes = vec![255u8; (w * h * 4) as usize];
        // Track strip on the right edge.
        for y in 0..h {
            for x in (w - STRIP_WIDTH)..w {
                let off = ((y * w + x) * 4) as usize;
                bytes[off..off + 4].copy_from_slice(&[230, 230, 230, 255]);
            }
        }
        // Thumb.
        for y in thumb_top..(thumb_top + thumb_h) {
            for x in (w - STRIP_WIDTH)..w {
                let off = ((y * w + x) * 4) as usize;
                bytes[off..off + 4].copy_from_slice(&[120, 120, 120, 255]);
            }
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
    async fn detects_thumb_position_from_synthesized_strip() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // Thumb at y=80, height 30; track 200, denom=170 → pos = 80/170 ≈ 0.47.
        p.tick(frame_with_thumb(80, 30)).await.expect("tick");
        let s = scrollbar_position(&p, None).expect("sb");
        assert!(s.present);
        assert!(
            (s.position - 80.0 / 170.0).abs() < 0.05,
            "got {}",
            s.position
        );
        let bb = s.thumb_bbox.expect("bbox");
        assert_eq!(bb.h, 30);
    }

    #[tokio::test]
    async fn no_thumb_means_not_present() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // All-uniform strip — no thumb.
        let w = 256u32;
        let h = 200u32;
        let bytes = vec![230u8; (w * h * 4) as usize];
        let f = Frame {
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
        };
        p.tick(f).await.expect("tick");
        let s = scrollbar_position(&p, None).expect("sb");
        assert!(!s.present);
    }
}
