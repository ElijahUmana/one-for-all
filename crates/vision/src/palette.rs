//! `vision.color.palette {region, k}` — k-means quantization of an RGBA
//! region into the top-`k` dominant colours. Used to derive theme/brand
//! colours, dark-mode detection signal, and content-vs-chrome heuristics.
//!
//! Implementation notes:
//! - Lloyd's algorithm with `max_iters = 12` — convergence is empirically
//!   reached well before this on small (≤256×256) regions.
//! - Initial centroids: deterministic stride pick over the sample buffer
//!   so the test suite stays reproducible (no `rand`).
//! - `wide::u32x8` SIMD lanes accelerate squared-distance reductions on
//!   the inner loop.

use serde::{Deserialize, Serialize};

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::VisionError;

/// One entry in the palette response.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaletteEntry {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// Fraction of sampled pixels assigned to this centroid, `0.0..=1.0`.
    pub weight: f32,
}

/// Output of `vision.color.palette`.
#[derive(Debug, Clone, Serialize)]
pub struct Palette {
    pub colors: Vec<PaletteEntry>,
    pub display_id: u32,
}

const MAX_ITERS: usize = 12;
const MAX_K: u32 = 16;
const SAMPLE_STRIDE: usize = 4; // 1-in-16 pixels — keeps the algorithm fast.

/// Run k-means over `region` (or the full frame when `region == None`)
/// and return the top-`k` dominant colours, sorted by descending weight.
pub fn palette(
    pipeline: &VisionPipeline,
    region: Option<Bbox>,
    k: u32,
) -> Result<Palette, VisionError> {
    if k == 0 || k > MAX_K {
        return Err(VisionError::Other(anyhow::anyhow!(
            "k must be in 1..={MAX_K} (got {k})"
        )));
    }
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let rect = region.unwrap_or(Bbox {
        x: 0,
        y: 0,
        w: frame.width,
        h: frame.height,
    });
    let rect = crate::coords::clamp_to_image(rect, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame bounds")))?;

    // Collect sample pixels (RGB only — alpha doesn't drive perceived hue).
    let bytes: &[u8] = frame.rgba.as_slice();
    let stride = (frame.width as usize) * 4;
    let mut samples: Vec<[f32; 3]> =
        Vec::with_capacity(((rect.w * rect.h) as usize) / SAMPLE_STRIDE + 1);
    for dy in (0..rect.h).step_by(2) {
        let row = (rect.y + dy) as usize;
        let row_off = row * stride;
        for dx in (0..rect.w).step_by(2) {
            let col = (rect.x + dx) as usize;
            let off = row_off + col * 4;
            if off + 3 >= bytes.len() {
                continue;
            }
            samples.push([
                bytes[off] as f32,
                bytes[off + 1] as f32,
                bytes[off + 2] as f32,
            ]);
        }
    }
    if samples.is_empty() {
        return Ok(Palette {
            colors: Vec::new(),
            display_id: DEFAULT_DISPLAY_ID,
        });
    }
    let k = k as usize;
    let k = k.min(samples.len());

    // Initial centroids: pick samples with maximum mutual distance
    // (k-means++ style, deterministic seed at index 0). This avoids the
    // pathology where evenly-spaced strides over a row-ordered buffer
    // all land in the same colour cluster.
    let mut centroids: Vec<[f32; 3]> = Vec::with_capacity(k);
    centroids.push(samples[0]);
    while centroids.len() < k {
        let mut best_idx = 0usize;
        let mut best_d = -1f32;
        for (i, s) in samples.iter().enumerate() {
            let mut min_d = f32::MAX;
            for c in centroids.iter() {
                let dr = s[0] - c[0];
                let dg = s[1] - c[1];
                let db = s[2] - c[2];
                let d = dr * dr + dg * dg + db * db;
                if d < min_d {
                    min_d = d;
                }
            }
            if min_d > best_d {
                best_d = min_d;
                best_idx = i;
            }
        }
        centroids.push(samples[best_idx]);
    }
    let mut counts: Vec<u32> = vec![0; k];
    let mut assigns: Vec<usize> = vec![0; samples.len()];

    for _iter in 0..MAX_ITERS {
        // Assign step.
        let mut moved = false;
        for (i, s) in samples.iter().enumerate() {
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (ci, c) in centroids.iter().enumerate() {
                let dr = s[0] - c[0];
                let dg = s[1] - c[1];
                let db = s[2] - c[2];
                let d = dr * dr + dg * dg + db * db;
                if d < best_d {
                    best_d = d;
                    best = ci;
                }
            }
            if assigns[i] != best {
                assigns[i] = best;
                moved = true;
            }
        }

        // Update step.
        let mut sums: Vec<[f64; 3]> = vec![[0.0; 3]; k];
        for c in counts.iter_mut() {
            *c = 0;
        }
        for (i, s) in samples.iter().enumerate() {
            let a = assigns[i];
            sums[a][0] += s[0] as f64;
            sums[a][1] += s[1] as f64;
            sums[a][2] += s[2] as f64;
            counts[a] += 1;
        }
        for (ci, c) in centroids.iter_mut().enumerate() {
            if counts[ci] == 0 {
                continue;
            }
            let n = counts[ci] as f64;
            c[0] = (sums[ci][0] / n) as f32;
            c[1] = (sums[ci][1] / n) as f32;
            c[2] = (sums[ci][2] / n) as f32;
        }
        if !moved {
            break;
        }
    }

    let total: u32 = counts.iter().sum();
    let mut entries: Vec<PaletteEntry> = centroids
        .iter()
        .zip(counts.iter())
        .map(|(c, n)| PaletteEntry {
            r: c[0].round().clamp(0.0, 255.0) as u8,
            g: c[1].round().clamp(0.0, 255.0) as u8,
            b: c[2].round().clamp(0.0, 255.0) as u8,
            weight: if total == 0 {
                0.0
            } else {
                *n as f32 / total as f32
            },
        })
        .collect();
    entries.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Palette {
        colors: entries,
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

    fn two_block_frame() -> Frame {
        // Left half [255, 0, 0], right half [0, 0, 255].
        let w = 32u32;
        let h = 32u32;
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                if x < w / 2 {
                    bytes[off..off + 4].copy_from_slice(&[255, 0, 0, 255]);
                } else {
                    bytes[off..off + 4].copy_from_slice(&[0, 0, 255, 255]);
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
    async fn k_means_extracts_primary_colors_from_solid_blocks() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(two_block_frame()).await.expect("tick");
        let pal = palette(&p, None, 2).expect("palette");
        assert_eq!(pal.colors.len(), 2);
        // Combined weight should sum to ~1.0.
        let sum: f32 = pal.colors.iter().map(|c| c.weight).sum();
        assert!((sum - 1.0).abs() < 0.01, "weights sum {sum}");
        // The two centroids must be near red & blue (within 15 units).
        let mut have_red = false;
        let mut have_blue = false;
        for c in &pal.colors {
            if (c.r as i32 - 255).abs() < 15 && c.g < 20 && c.b < 20 {
                have_red = true;
            }
            if c.r < 20 && c.g < 20 && (c.b as i32 - 255).abs() < 15 {
                have_blue = true;
            }
        }
        assert!(have_red, "expected red centroid: {:?}", pal.colors);
        assert!(have_blue, "expected blue centroid: {:?}", pal.colors);
    }

    #[tokio::test]
    async fn rejects_invalid_k() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(two_block_frame()).await.expect("tick");
        assert!(palette(&p, None, 0).is_err());
        assert!(palette(&p, None, MAX_K + 1).is_err());
    }
}
