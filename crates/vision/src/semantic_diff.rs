//! `vision.diff.semantic {prev, next, action_context}` — VLM-driven
//! semantic diff classifier.
//!
//! Categorizes the change between two frames as one of: `no_op`,
//! `progress`, `failure`, `success`. Used by post-action verification:
//! after the agent dispatches `page.click` (etc.), we hand the prior
//! frame + the now-frame + the action context to a VLM and let it
//! decide whether the action did what was intended.
//!
//! Backends:
//! - VLM `Off`  — returns a heuristic verdict computed from
//!                tile-change ratio (`progress`/`no_op`).
//! - Anthropic / LocalLlama — see [`crate::vlm`].

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::types::{DecodedImage, VisionError};
use crate::vlm::ActionContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SemanticDiffKind {
    NoOp,
    Progress,
    Failure,
    Success,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SemanticDiffVerdict {
    pub kind: SemanticDiffKind,
    pub confidence: f32,
    pub concern: Option<String>,
    pub display_id: u32,
    pub prev_seq: u64,
    pub next_seq: u64,
}

/// Diff `prev_frame` against `next_frame` semantically.
///
/// The prev/next images travel as `Arc<DecodedImage>` so the VLM
/// backend can fan them out without a deep copy.
pub async fn semantic_diff(
    pipeline: &VisionPipeline,
    prev_seq: u64,
    prev_frame: Arc<DecodedImage>,
    next_seq: u64,
    next_frame: Arc<DecodedImage>,
    action: ActionContext,
) -> Result<SemanticDiffVerdict, VisionError> {
    if prev_frame.width != next_frame.width || prev_frame.height != next_frame.height {
        return Err(VisionError::DimensionsMismatch {
            prev: (prev_frame.width, prev_frame.height),
            next: (next_frame.width, next_frame.height),
        });
    }
    // Cheap baseline signal: pixel-bucket churn on a downsampled grid.
    let churn = pixel_churn(&prev_frame, &next_frame);
    // Hand the *next* frame to the VLM verifier so the semantic-diff contract
    // actually evaluates the requested historical frame pair rather than the
    // pipeline's latest decoded frame.
    let vlm_verdict = pipeline
        .verify_action_on_frame(Arc::clone(&next_frame), action.clone())
        .await?;

    let kind = if vlm_verdict.skipped {
        // No VLM — derive from churn.
        if churn < 0.005 {
            SemanticDiffKind::NoOp
        } else if churn < 0.15 {
            SemanticDiffKind::Progress
        } else {
            // Big change without a VLM — we can't tell success vs failure.
            SemanticDiffKind::Progress
        }
    } else if let Some(concern) = vlm_verdict.concern.as_ref() {
        if concern.to_lowercase().contains("fail") {
            SemanticDiffKind::Failure
        } else {
            SemanticDiffKind::Progress
        }
    } else if vlm_verdict.confidence > 0.8 {
        SemanticDiffKind::Success
    } else if churn < 0.005 {
        SemanticDiffKind::NoOp
    } else {
        SemanticDiffKind::Progress
    };

    Ok(SemanticDiffVerdict {
        kind,
        confidence: vlm_verdict.confidence,
        concern: vlm_verdict.concern,
        display_id: DEFAULT_DISPLAY_ID,
        prev_seq,
        next_seq,
    })
}

fn pixel_churn(a: &DecodedImage, b: &DecodedImage) -> f32 {
    let stride_a = (a.width as usize) * 4;
    let stride_b = (b.width as usize) * 4;
    let bytes_a: &[u8] = a.rgba.as_slice();
    let bytes_b: &[u8] = b.rgba.as_slice();
    let mut diff = 0u64;
    let mut total = 0u64;
    let step = 8u32;
    for y in (0..a.height).step_by(step as usize) {
        for x in (0..a.width).step_by(step as usize) {
            let oa = (y as usize) * stride_a + (x as usize) * 4;
            let ob = (y as usize) * stride_b + (x as usize) * 4;
            if oa + 2 >= bytes_a.len() || ob + 2 >= bytes_b.len() {
                continue;
            }
            let d = (bytes_a[oa] as i32 - bytes_b[ob] as i32).abs()
                + (bytes_a[oa + 1] as i32 - bytes_b[ob + 1] as i32).abs()
                + (bytes_a[oa + 2] as i32 - bytes_b[ob + 2] as i32).abs();
            if d > 30 {
                diff += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        diff as f32 / total as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::VisionPipeline;
    use crate::metrics::Histograms;
    use crate::types::DecodedImage;
    use crate::vlm::VlmConfig;
    use std::sync::Arc;

    fn solid(w: u32, h: u32, fill: u8) -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: w,
            height: h,
            rgba: Arc::new(vec![fill; (w * h * 4) as usize]),
            captured_us: 0,
        })
    }

    #[tokio::test]
    async fn vlm_off_returns_skipped_verdict() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        // Need a frame in the pipeline so pre_action_verify has an image.
        crate::pixel::pixel_at(&p, 0, 0)
            .err()
            .expect("no frame yet");
        // Inject one via tick.
        p.tick(crate::types::Frame {
            seq: 1,
            raw: crate::types::ScreencastFrame {
                bytes: Arc::new(vec![]),
                format: crate::types::FrameFormat::Jpeg,
                metadata: crate::types::ScreencastFrameMetadata {
                    offset_top: 0.0,
                    page_scale_factor: 1.0,
                    device_width: 32.0,
                    device_height: 32.0,
                    scroll_offset_x: 0.0,
                    scroll_offset_y: 0.0,
                    timestamp: 0.0,
                },
                session_id: "s".into(),
                captured_us: 0,
            },
            decoded: Some((*solid(32, 32, 50)).clone()),
        })
        .await
        .expect("tick");
        let prev = solid(32, 32, 50);
        let next = solid(32, 32, 50);
        let v = semantic_diff(
            &p,
            1,
            prev,
            1,
            next,
            ActionContext {
                action: "page.click".into(),
                element_ref: None,
                element_text: None,
                note: None,
            },
        )
        .await
        .expect("diff");
        // With VLM off and zero churn → no_op.
        assert_eq!(v.kind, SemanticDiffKind::NoOp);
        assert_eq!(v.prev_seq, 1);
        assert_eq!(v.next_seq, 1);
    }

    #[tokio::test]
    async fn dimensions_mismatch_errors() {
        let p = VisionPipeline::new("s", "t", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(crate::types::Frame {
            seq: 1,
            raw: crate::types::ScreencastFrame {
                bytes: Arc::new(vec![]),
                format: crate::types::FrameFormat::Jpeg,
                metadata: crate::types::ScreencastFrameMetadata {
                    offset_top: 0.0,
                    page_scale_factor: 1.0,
                    device_width: 32.0,
                    device_height: 32.0,
                    scroll_offset_x: 0.0,
                    scroll_offset_y: 0.0,
                    timestamp: 0.0,
                },
                session_id: "s".into(),
                captured_us: 0,
            },
            decoded: Some((*solid(32, 32, 0)).clone()),
        })
        .await
        .expect("tick");
        let r = semantic_diff(
            &p,
            1,
            solid(32, 32, 0),
            2,
            solid(64, 32, 0),
            ActionContext {
                action: "page.click".into(),
                element_ref: None,
                element_text: None,
                note: None,
            },
        )
        .await;
        assert!(matches!(r, Err(VisionError::DimensionsMismatch { .. })));
    }
}
