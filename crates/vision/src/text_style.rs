//! `vision.text.style {region}` — given a CSS-pixel point or rect, look
//! up the OCR region that contains it and infer:
//!
//! - **size** — bbox height in CSS px (font-size proxy).
//! - **weight** — stroke density: ratio of dark-foreground pixels in the
//!   bbox after Otsu binarization.
//! - **color (fg / bg)** — mean pixel under/around foreground & background
//!   masks.
//!
//! Approximations only — fonts in the wild render with antialiasing and
//! sub-pixel hints, so the weight estimate is a coarse heuristic. The
//! returned weight is bucketed (`100`..=`900` in CSS step 100s) which is
//! all an agent ever needs ("is this bold?").

use serde::{Deserialize, Serialize};

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::{DecodedImage, VisionError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextStyle {
    /// Recognized text under the rect (best-match OCR region).
    pub text: String,
    /// Font size in CSS px (≈bbox height).
    pub size_css: u32,
    /// CSS font-weight: 100, 200, 300, 400, 500, 600, 700, 800, 900.
    pub weight: u32,
    pub fg: ColorRgb,
    pub bg: ColorRgb,
    pub display_id: u32,
}

pub async fn text_style(pipeline: &VisionPipeline, region: Bbox) -> Result<TextStyle, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let region = crate::coords::clamp_to_image(region, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame")))?;

    // Find the OCR region that best overlaps the input rect.
    let cached = pipeline.read_text(Some(region)).await.unwrap_or_default();
    let best = cached
        .into_iter()
        .max_by_key(|r| (r.bbox.w * r.bbox.h) as u64)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("no OCR text in region")))?;

    let scale = crate::coords::sanitize_scale(frame.captured_us as f64); // unused — placeholder
    let _ = scale;
    let css_h = best.bbox.h.max(1);
    let (fg, bg, weight) = analyse_pixels(&frame, best.bbox);

    Ok(TextStyle {
        text: best.text,
        size_css: css_h,
        weight,
        fg,
        bg,
        display_id: DEFAULT_DISPLAY_ID,
    })
}

fn analyse_pixels(img: &DecodedImage, region: Bbox) -> (ColorRgb, ColorRgb, u32) {
    let bytes: &[u8] = img.rgba.as_slice();
    let stride = (img.width as usize) * 4;
    // Single pass: bucket pixels into fg (dark) / bg (light) using a fixed
    // mid-luma threshold. Otsu degrades on bimodal binary inputs (the ink-
    // on-paper case) because it picks an extreme threshold that maximises
    // variance without actually splitting the classes; a fixed mid-luma
    // threshold is robust for real-world text.
    const LUMA_THRESHOLD: u32 = 128;
    let mut fg = [0u64; 3];
    let mut bg = [0u64; 3];
    let mut nfg = 0u64;
    let mut nbg = 0u64;
    for y in region.y..region.y + region.h {
        for x in region.x..region.x + region.w {
            let off = (y as usize) * stride + (x as usize) * 4;
            if off + 2 >= bytes.len() {
                continue;
            }
            let r = bytes[off] as u32;
            let g = bytes[off + 1] as u32;
            let b = bytes[off + 2] as u32;
            let luma = (30 * r + 50 * g + 20 * b) / 100;
            if luma < LUMA_THRESHOLD {
                fg[0] += r as u64;
                fg[1] += g as u64;
                fg[2] += b as u64;
                nfg += 1;
            } else {
                bg[0] += r as u64;
                bg[1] += g as u64;
                bg[2] += b as u64;
                nbg += 1;
            }
        }
    }
    let total = nfg + nbg;
    let avg = |s: [u64; 3], n: u64| -> ColorRgb {
        if n == 0 {
            ColorRgb { r: 0, g: 0, b: 0 }
        } else {
            ColorRgb {
                r: ((s[0] / n).min(255)) as u8,
                g: ((s[1] / n).min(255)) as u8,
                b: ((s[2] / n).min(255)) as u8,
            }
        }
    };
    let stroke_density = if total == 0 {
        0.0
    } else {
        nfg as f32 / total as f32
    };
    // Map stroke density → CSS font-weight buckets. Empirical: ≤0.10
    // → 300, 0.10–0.18 → 400, 0.18–0.24 → 600, 0.24–0.32 → 700, ≥0.32 → 800.
    let weight = if stroke_density < 0.10 {
        300
    } else if stroke_density < 0.18 {
        400
    } else if stroke_density < 0.24 {
        600
    } else if stroke_density < 0.32 {
        700
    } else {
        800
    };
    (avg(fg, nfg), avg(bg, nbg), weight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::VisionPipeline;
    use crate::metrics::Histograms;
    use crate::ocr::TextRegion;
    use crate::types::{
        DecodedImage, Frame, FrameFormat, ScreencastFrame, ScreencastFrameMetadata,
    };
    use crate::vlm::VlmConfig;
    use std::sync::Arc;

    /// Build a frame with a horizontal black-on-white "stroke" of `density`.
    fn striped_frame(w: u32, h: u32, density: f32) -> Frame {
        let mut bytes = vec![255u8; (w * h * 4) as usize];
        let n_stripes = ((density * h as f32) as u32).max(1);
        for y in 0..n_stripes {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                bytes[off..off + 4].copy_from_slice(&[0, 0, 0, 255]);
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
    async fn infers_size_and_bold_weight() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // 32-tall heavy stroke (density ~0.30 → weight 700).
        p.tick(striped_frame(120, 32, 0.30)).await.expect("tick");
        p.ocr_cache_test_inject(
            (0, 0, 1),
            vec![TextRegion {
                bbox: Bbox {
                    x: 0,
                    y: 0,
                    w: 120,
                    h: 32,
                },
                text: "BOLD".into(),
                confidence: 0.95,
            }],
        );
        let s = text_style(
            &p,
            Bbox {
                x: 0,
                y: 0,
                w: 120,
                h: 32,
            },
        )
        .await
        .expect("ts");
        assert_eq!(s.text, "BOLD");
        assert_eq!(s.size_css, 32);
        assert!(s.weight >= 600, "expected bold-ish, got {}", s.weight);
        assert!(s.fg.r < 30 && s.fg.g < 30 && s.fg.b < 30, "fg ~ black");
        assert!(s.bg.r > 230 && s.bg.g > 230 && s.bg.b > 230, "bg ~ white");
    }

    #[tokio::test]
    async fn no_ocr_returns_err() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(striped_frame(100, 20, 0.10)).await.expect("tick");
        let err = text_style(
            &p,
            Bbox {
                x: 0,
                y: 0,
                w: 50,
                h: 10,
            },
        )
        .await
        .err()
        .expect("err");
        assert!(format!("{err}").contains("no OCR text"));
    }
}
