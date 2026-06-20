//! `vision` — SPEC §11 V4 continuous vision pipeline.
//!
//! ## What this crate does
//!
//! Per page, on `vision = continuous`:
//!
//! ```text
//! CDP Page.startScreencast ──▶ capture ──▶ frame_ring (mmap) ──┬──▶ diff ──▶ ocr ──┐
//!                                                              │                    ├──▶ subscribe ──▶ event/notify {topic: "vision.frame"}
//!                                                              └────── meta ────────┘
//! ```
//!
//! ## SLOs (SPEC §11 V4 latency budget)
//!
//! | Path                                          | Target  |
//! |-----------------------------------------------|---------|
//! | Frame capture → diff → `vision.frame` event   | < 50ms p99 |
//! | `vision.find_text` query                       | < 10ms p99 |
//!
//! The crate exports per-stage histograms via [`metrics`] for SLO enforcement.
//!
//! ## Allocation discipline
//!
//! Hot paths (`diff`, OCR dispatch, notification build) accept a
//! [`bumpalo::Bump`] scratch arena. Inner loops avoid `Vec::new()` /
//! `String::new()` / `Box::new()`. Verified by clippy
//! `disallowed_methods` in the broker integration.
//!
//! ## Bounded channels (SPEC §1 D16)
//!
//! Every internal channel is bounded. `Page::screencast_subscribe()`
//! broadcaster: 32. Capture → pipeline `mpsc`: 64. OCR work-queue: 32.

#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod animation;
pub mod api;
pub mod barcode;
pub mod capture;
pub mod coords;
pub mod diff;
pub mod face_blur;
pub mod frame_ring;
pub mod icon;
pub mod layout;
pub mod loading;
pub mod metrics;
pub mod ocr;
pub mod overlay;
pub mod palette;
pub mod pixel;
pub mod region_classify;
pub mod scrollbar;
pub mod semantic_diff;
pub mod subscribe;
pub mod text_style;
pub mod vlm;

mod types;

pub use api::{StabilityReport, StabilityState, VisionConfig, VisionMode, VisionPipeline};
pub use coords::{to_css_bbox, to_device_bbox, CssBbox, DEFAULT_DISPLAY_ID};
pub use diff::{Bbox, DiffResult, TileChange};
pub use frame_ring::{FrameHandle, FrameRing, ReadGuard, WriteGuard};
pub use metrics::Histograms;
pub use ocr::{OcrBackend, OcrCache, TextRegion};
pub use subscribe::{VisionFrameEvent, VisionSubscriber};
pub use types::{
    DecodedImage, Frame, FrameFormat, ScreencastFrame, ScreencastFrameMetadata, TextMatch,
    TextQuery, VisionError,
};
pub use vlm::{ActionContext, VlmBackend, VlmConfig, VlmVerdict};
