//! `vision.qr_barcode {region?}` — barcode + QR detection.
//!
//! macOS: `VNRecognizeBarcodesRequest` (Apple Vision) behind the
//! `macos-vision` feature, returning detected payloads + bounding boxes.
//!
//! Other platforms (or when the feature is off): a deterministic stub
//! that returns an empty result. The router still surfaces the tool —
//! it just always reports zero matches off-Apple. This matches how
//! [`crate::ocr`] handles its `NoopOcr` fallback.

use serde::Serialize;

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::diff::Bbox;
use crate::types::VisionError;

#[derive(Debug, Clone, Serialize)]
pub struct Barcode {
    pub kind: String,
    pub payload: String,
    pub bbox: Bbox,
    pub display_id: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BarcodeScan {
    pub matches: Vec<Barcode>,
    pub display_id: u32,
}

pub fn scan(pipeline: &VisionPipeline, region: Option<Bbox>) -> Result<BarcodeScan, VisionError> {
    let frame = pipeline.last_decoded().ok_or(VisionError::NotEnabled)?;
    let rect = region.unwrap_or(Bbox {
        x: 0,
        y: 0,
        w: frame.width,
        h: frame.height,
    });
    let _rect = crate::coords::clamp_to_image(rect, frame.width, frame.height)
        .ok_or_else(|| VisionError::Other(anyhow::anyhow!("region outside frame")))?;
    #[cfg(all(target_os = "macos", feature = "macos-vision"))]
    {
        return platform::scan_apple_vision(&frame, _rect);
    }
    #[cfg(not(all(target_os = "macos", feature = "macos-vision")))]
    {
        Ok(BarcodeScan {
            matches: Vec::new(),
            display_id: DEFAULT_DISPLAY_ID,
        })
    }
}

#[cfg(all(target_os = "macos", feature = "macos-vision"))]
mod platform {
    use super::*;
    use crate::types::DecodedImage;

    pub(super) fn scan_apple_vision(
        _img: &DecodedImage,
        _rect: Bbox,
    ) -> Result<BarcodeScan, VisionError> {
        // Real implementation builds a CIImage from the RGBA buffer and
        // dispatches a VNRecognizeBarcodesRequest synchronously. The
        // observation list is mapped into `Barcode` entries with
        // `kind = symbology` (e.g. "QR", "Code128") and `payload =
        // payloadStringValue`. Bbox is `boundingBox` mapped from
        // normalized to pixel coordinates and flipped vertically.
        //
        // We're scaffolding the surface here; the objc2 bindings + CIImage
        // bridging are intentionally deferred to the same follow-up that
        // turns the OCR scaffolding into a real Apple Vision call. Until
        // then this returns an empty match set so the router contract
        // stays stable.
        Ok(BarcodeScan {
            matches: Vec::new(),
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

    fn solid_frame() -> Frame {
        let w = 64u32;
        let h = 64u32;
        let bytes = vec![255u8; (w * h * 4) as usize];
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
    async fn scan_returns_empty_off_apple_vision() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(solid_frame()).await.expect("tick");
        let r = scan(&p, None).expect("scan");
        assert!(r.matches.is_empty());
    }
}
