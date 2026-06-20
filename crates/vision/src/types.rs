//! Shared concrete types — `Frame`, `DecodedImage`, error enum.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::diff::Bbox;

/// Errors surfaced by every module in this crate. Maps onto JSON-RPC error
/// codes in the broker layer when applicable.
#[derive(Debug, Error)]
pub enum VisionError {
    #[error("vision pipeline not enabled for this tab")]
    NotEnabled,
    #[error("frame ring exhausted (no free slots and all readers are lagging)")]
    RingExhausted,
    #[error("frame too large for ring slot ({len} bytes > {cap})")]
    FrameTooLarge { len: usize, cap: usize },
    #[error("frame dimensions mismatch (prev {prev:?} vs next {next:?})")]
    DimensionsMismatch { prev: (u32, u32), next: (u32, u32) },
    #[error("invalid screencast format {0}")]
    InvalidFormat(String),
    #[error("vlm backend unavailable: {0}")]
    VlmUnavailable(String),
    #[error("ocr backend error: {0}")]
    Ocr(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("image decode error: {0}")]
    Image(String),
    #[error("cdp error: {0}")]
    Cdp(String),
    #[error("other error: {0}")]
    Other(#[from] anyhow::Error),
}

/// CDP screencast frame format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    Png,
    Jpeg,
}

impl FrameFormat {
    pub fn cdp_name(self) -> &'static str {
        match self {
            FrameFormat::Png => "png",
            FrameFormat::Jpeg => "jpeg",
        }
    }
    pub fn parse(s: &str) -> Result<FrameFormat, VisionError> {
        match s {
            "png" => Ok(FrameFormat::Png),
            "jpeg" => Ok(FrameFormat::Jpeg),
            other => Err(VisionError::InvalidFormat(other.to_owned())),
        }
    }
}

/// CDP `Page.screencastFrameMetadata`, normalized into a tight struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreencastFrameMetadata {
    pub offset_top: f64,
    pub page_scale_factor: f64,
    pub device_width: f64,
    pub device_height: f64,
    pub scroll_offset_x: f64,
    pub scroll_offset_y: f64,
    pub timestamp: f64,
}

/// Raw screencast frame off the wire, before decoding.
#[derive(Debug, Clone)]
pub struct ScreencastFrame {
    /// Encoded image bytes (decoded from CDP base64).
    pub bytes: Arc<Vec<u8>>,
    pub format: FrameFormat,
    pub metadata: ScreencastFrameMetadata,
    /// CDP session id this frame originated on (for the ack).
    pub session_id: String,
    /// Producer-side monotonic timestamp, microseconds since epoch.
    pub captured_us: u64,
}

/// A decoded RGBA frame held in shared memory.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tight RGBA8 buffer, length = width * height * 4.
    pub rgba: Arc<Vec<u8>>,
    pub captured_us: u64,
}

impl DecodedImage {
    pub fn pixel_count(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    pub fn byte_len(&self) -> usize {
        self.pixel_count() * 4
    }
}

/// A `Frame` is the unit of work flowing through the pipeline. It carries
/// either encoded bytes (capture stage) or a decoded image (diff/OCR
/// stages), plus a stable seq number assigned by the [`crate::FrameRing`].
#[derive(Debug, Clone)]
pub struct Frame {
    pub seq: u64,
    pub raw: ScreencastFrame,
    pub decoded: Option<DecodedImage>,
}

/// Free-form query for [`crate::api::VisionPipeline::find_text`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextQuery {
    pub query: String,
    #[serde(default)]
    pub is_regex: bool,
    /// Optional region to restrict the search.
    #[serde(default)]
    pub region: Option<Bbox>,
}

/// One match returned by `vision.find_text`.
#[derive(Debug, Clone, Serialize)]
pub struct TextMatch {
    pub region: Bbox,
    pub text: String,
    pub score: f32,
}
