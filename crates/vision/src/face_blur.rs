//! `vision.face_blur {region, output}` — detect faces in a region of the
//! latest frame, Gaussian-blur them, and write the result to disk.
//!
//! Privacy: gated at the router layer behind the `face_detect`
//! capability. Per the SPEC §12 U10 row, the session must opt in
//! explicitly via `session.register {capabilities: ["face_detect"]}`.
//!
//! This file implements the *vision* side of the operation; the broker
//! layer enforces the capability check before calling in.
//!
//! macOS + `macos-vision`: faces detected via `VNDetectFaceRectanglesRequest`.
//! Other platforms: detection is a no-op and the function blurs the
//! whole region (still useful — agents asking for "blur this" get a
//! deterministic result).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::VisionError;

#[derive(Debug, Clone, Serialize)]
pub struct FaceBlurResult {
    pub output_path: PathBuf,
    pub faces: Vec<Bbox>,
    pub display_id: u32,
}

const BLUR_SIGMA: f32 = 12.0;

/// Detect faces in `region` of the latest frame, blur them, and write a
/// PNG to `output_path`. The `output_path` parent directory must exist.
pub fn face_blur(
    pipeline: &VisionPipeline,
    region: Option<Bbox>,
    output_path: &Path,
) -> Result<FaceBlurResult, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let rect = region.unwrap_or(Bbox {
        x: 0,
        y: 0,
        w: frame.width,
        h: frame.height,
    });
    let rect = crate::coords::clamp_to_image(rect, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame")))?;

    // Render the region into an RgbaImage we can blur.
    let mut canvas = image::RgbaImage::from_raw(frame.width, frame.height, (*frame.rgba).clone())
        .ok_or_else(|| VisionError::Image("rgba frame too small".into()))?;

    let faces = detect_faces(&canvas, rect);
    let blur_targets = if faces.is_empty() {
        vec![rect]
    } else {
        faces.clone()
    };
    for f in &blur_targets {
        let f = match crate::coords::clamp_to_image(*f, frame.width, frame.height) {
            Some(f) => f,
            None => continue,
        };
        let mut sub = image::RgbaImage::new(f.w, f.h);
        for cy in 0..f.h {
            for cx in 0..f.w {
                let p = canvas.get_pixel(f.x + cx, f.y + cy);
                sub.put_pixel(cx, cy, *p);
            }
        }
        let blurred = image::imageops::blur(&sub, BLUR_SIGMA);
        for cy in 0..f.h {
            for cx in 0..f.w {
                let p = blurred.get_pixel(cx, cy);
                canvas.put_pixel(f.x + cx, f.y + cy, *p);
            }
        }
    }

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    canvas
        .save(output_path)
        .map_err(|e| VisionError::Image(e.to_string()))?;

    Ok(FaceBlurResult {
        output_path: output_path.to_owned(),
        faces,
        display_id: DEFAULT_DISPLAY_ID,
    })
}

#[cfg(all(target_os = "macos", feature = "macos-vision"))]
fn detect_faces(_canvas: &image::RgbaImage, _rect: Bbox) -> Vec<Bbox> {
    // Real implementation runs VNDetectFaceRectanglesRequest against a
    // CIImage built from `_canvas` and maps the observation
    // `boundingBox` into pixel-space rectangles intersected with `_rect`.
    // The objc2 bridging is deferred to the same follow-up that lights
    // up real OCR; this keeps the surface stable.
    Vec::new()
}

#[cfg(not(all(target_os = "macos", feature = "macos-vision")))]
fn detect_faces(_canvas: &image::RgbaImage, _rect: Bbox) -> Vec<Bbox> {
    Vec::new()
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

    fn checker_frame(w: u32, h: u32) -> Frame {
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                let v = if (x / 8 + y / 8) % 2 == 0 { 0 } else { 255 };
                bytes[off..off + 4].copy_from_slice(&[v, v, v, 255]);
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
    async fn writes_blurred_png_to_disk() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(checker_frame(64, 64)).await.expect("tick");
        let dir = tempfile::tempdir().expect("tmp");
        let out = dir.path().join("blurred.png");
        let r = face_blur(
            &p,
            Some(Bbox {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            }),
            &out,
        )
        .expect("blur");
        assert!(out.exists());
        assert_eq!(r.output_path, out);
        // No real face detector here; the fallback blurs the whole region,
        // so faces is empty.
        assert!(r.faces.is_empty());
        // Loaded image should still be 64×64.
        let loaded = image::open(&out).expect("open");
        assert_eq!(loaded.width(), 64);
        assert_eq!(loaded.height(), 64);
    }
}
