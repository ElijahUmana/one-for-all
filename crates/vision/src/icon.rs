//! `vision.icon.recognize` — small-icon semantic labelling.
//!
//! Local backend: a deterministic 16×16 luma + average-hash signature
//! against a built-in library of canonical icons (close, search, menu,
//! arrow, check, cross, gear, …). When a hash is within Hamming distance
//! `LOCAL_HAM_MAX` of a library entry, that label is returned with a
//! confidence proportional to closeness. Otherwise the call returns
//! `unknown` and downstream callers can fall back to the VLM tool
//! (`vision.diff.semantic` or a future `vision.icon.recognize` VLM mode).
//!
//! The library is hand-crafted with synthesized 16×16 glyphs so the test
//! suite is hermetic. Real-world deployments swap in a richer embedding
//! model, which is why the signature is hash-based and not a CNN; we
//! gain compactness, determinism, and zero-allocation on the hot path.

use serde::Serialize;

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::{DecodedImage, VisionError};

const ICON_DIM: u32 = 16;
const LOCAL_HAM_MAX: u32 = 32;

/// One match returned by [`recognize`].
#[derive(Debug, Clone, Serialize)]
pub struct IconRecognition {
    pub label: String,
    pub confidence: f32,
    pub display_id: u32,
}

/// Recognize the icon in `region`. Returns `unknown` (with confidence 0)
/// when no library entry matches.
pub fn recognize(pipeline: &VisionPipeline, region: Bbox) -> Result<IconRecognition, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let region = crate::coords::clamp_to_image(region, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame")))?;
    let sig = compute_signature(&frame, region);
    let mut best = ("unknown", u32::MAX);
    for (label, ref_sig) in LIBRARY.iter() {
        let d = hamming(sig, *ref_sig);
        if d < best.1 {
            best = (*label, d);
        }
    }
    let confidence = if best.1 > LOCAL_HAM_MAX {
        0.0
    } else {
        let max_bits = (ICON_DIM * ICON_DIM) as u32;
        1.0 - (best.1 as f32 / max_bits as f32)
    };
    Ok(IconRecognition {
        label: if confidence > 0.0 {
            best.0.to_owned()
        } else {
            "unknown".to_owned()
        },
        confidence,
        display_id: DEFAULT_DISPLAY_ID,
    })
}

/// Resample the `region` to 16×16 luma, threshold at the region mean →
/// 256-bit signature stored as 4 × u64.
fn compute_signature(img: &DecodedImage, region: Bbox) -> [u64; 4] {
    let bytes: &[u8] = img.rgba.as_slice();
    let stride = (img.width as usize) * 4;
    let mut grid = [0u8; (ICON_DIM * ICON_DIM) as usize];
    let mut sum = 0u32;
    for cy in 0..ICON_DIM {
        for cx in 0..ICON_DIM {
            let sx = region.x + (cx * region.w / ICON_DIM);
            let sy = region.y + (cy * region.h / ICON_DIM);
            let off = (sy as usize) * stride + (sx as usize) * 4;
            let l = if off + 2 < bytes.len() {
                ((bytes[off] as u32) * 30
                    + (bytes[off + 1] as u32) * 50
                    + (bytes[off + 2] as u32) * 20)
                    / 100
            } else {
                0
            };
            grid[(cy * ICON_DIM + cx) as usize] = l.min(255) as u8;
            sum += l;
        }
    }
    let mean = (sum / (ICON_DIM * ICON_DIM)) as u8;
    let mut out = [0u64; 4];
    for i in 0..(ICON_DIM * ICON_DIM) as usize {
        if grid[i] >= mean {
            let word = i / 64;
            let bit = i % 64;
            out[word] |= 1u64 << bit;
        }
    }
    out
}

fn hamming(a: [u64; 4], b: [u64; 4]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum()
}

/// Hand-crafted icon library. Each entry is a 16×16 binary glyph encoded
/// as 4× u64. Built once at module-load by the canonical glyph painter
/// below so the values match `compute_signature` byte-for-byte.
static LIBRARY: once_cell::sync::Lazy<Vec<(&'static str, [u64; 4])>> =
    once_cell::sync::Lazy::new(|| {
        vec![
            ("close", make_signature(close_glyph)),
            ("check", make_signature(check_glyph)),
            ("search", make_signature(search_glyph)),
            ("menu", make_signature(menu_glyph)),
            ("arrow_right", make_signature(arrow_right_glyph)),
            ("arrow_down", make_signature(arrow_down_glyph)),
            ("plus", make_signature(plus_glyph)),
            ("minus", make_signature(minus_glyph)),
            ("gear", make_signature(gear_glyph)),
        ]
    });

fn make_signature(painter: fn(u32, u32) -> bool) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut sum = 0u32;
    let mut grid = [0u8; (ICON_DIM * ICON_DIM) as usize];
    for y in 0..ICON_DIM {
        for x in 0..ICON_DIM {
            let on = painter(x, y);
            let v = if on { 0 } else { 255 }; // Glyph dark on light.
            grid[(y * ICON_DIM + x) as usize] = v;
            sum += v as u32;
        }
    }
    let mean = (sum / (ICON_DIM * ICON_DIM)) as u8;
    for i in 0..(ICON_DIM * ICON_DIM) as usize {
        if grid[i] >= mean {
            out[i / 64] |= 1u64 << (i % 64);
        }
    }
    out
}

fn close_glyph(x: u32, y: u32) -> bool {
    x == y || x + y == ICON_DIM - 1
}
fn check_glyph(x: u32, y: u32) -> bool {
    (x >= 2 && x < 7 && y == x + 5) || (x >= 6 && x < 14 && y as i32 == 14 - x as i32)
}
fn search_glyph(x: u32, y: u32) -> bool {
    let cx = 6;
    let cy = 6;
    let r2 = ((x as i32 - cx).pow(2) + (y as i32 - cy).pow(2)) as u32;
    r2 == 16 || (x >= 11 && x <= 14 && y == x + 1)
}
fn menu_glyph(_x: u32, y: u32) -> bool {
    y == 3 || y == 8 || y == 12
}
fn arrow_right_glyph(x: u32, y: u32) -> bool {
    (y == 8 && x >= 2 && x <= 13)
        || (x >= 9 && x <= 13 && (y as i32 - 8).abs() == (x as i32 - 13).abs())
}
fn arrow_down_glyph(x: u32, y: u32) -> bool {
    (x == 8 && y >= 2 && y <= 13)
        || (y >= 9 && y <= 13 && (x as i32 - 8).abs() == (y as i32 - 13).abs())
}
fn plus_glyph(x: u32, y: u32) -> bool {
    (x == 8 && y >= 2 && y <= 13) || (y == 8 && x >= 2 && x <= 13)
}
fn minus_glyph(_x: u32, y: u32) -> bool {
    y == 8
}
fn gear_glyph(x: u32, y: u32) -> bool {
    let cx = 8;
    let cy = 8;
    let r2 = ((x as i32 - cx).pow(2) + (y as i32 - cy).pow(2)) as u32;
    r2 == 16 || r2 == 25 || r2 == 36
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

    fn frame_with_glyph(painter: fn(u32, u32) -> bool, w: u32, h: u32) -> Frame {
        let mut bytes = vec![255u8; (w * h * 4) as usize];
        // Render the painter into the centre 16×16 region.
        let ox = (w - ICON_DIM) / 2;
        let oy = (h - ICON_DIM) / 2;
        for gy in 0..ICON_DIM {
            for gx in 0..ICON_DIM {
                if painter(gx, gy) {
                    let x = ox + gx;
                    let y = oy + gy;
                    let off = ((y * w + x) * 4) as usize;
                    bytes[off..off + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
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
    async fn recognizes_known_icon_via_hash_table() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(frame_with_glyph(close_glyph, 32, 32))
            .await
            .expect("tick");
        let r = recognize(
            &p,
            Bbox {
                x: 8,
                y: 8,
                w: ICON_DIM,
                h: ICON_DIM,
            },
        )
        .expect("recog");
        assert_eq!(
            r.label, "close",
            "got {:?} (conf {})",
            r.label, r.confidence
        );
        assert!(r.confidence > 0.5);
    }

    #[tokio::test]
    async fn unknown_when_far_from_library() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // Random checkerboard — far from any glyph.
        let w = 32u32;
        let h = 32u32;
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                let v = if ((x ^ y) & 0b101) == 0 { 0 } else { 255 };
                bytes[off..off + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
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
        let r = recognize(
            &p,
            Bbox {
                x: 8,
                y: 8,
                w: ICON_DIM,
                h: ICON_DIM,
            },
        )
        .expect("recog");
        // Either confidence=0 or a low-confidence label; either is acceptable.
        if r.label != "unknown" {
            assert!(r.confidence < 0.85, "noise should not match strongly");
        }
    }
}
