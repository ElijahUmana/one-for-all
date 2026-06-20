//! `vision.animation.frames {tab_id, duration_ms}` — capture a burst of
//! frames at the pipeline's current peak FPS and return their handles.
//!
//! This does not change the long-running FPS configuration; it walks the
//! ring backwards from `head_seq` collecting frames whose `captured_us`
//! falls inside the requested window. Callers wanting *future* frames
//! over a window should call `vision.fps` to bump capture rate first.
//!
//! Returned handles point at the same shm ring slot the consumer can
//! mmap zero-copy. We do NOT inline frame bytes — the `vision.frame`
//! event contract is preserved.

use serde::Serialize;

use crate::api::VisionPipeline;
use crate::coords::DEFAULT_DISPLAY_ID;
use crate::frame_ring::FrameHandle;
use crate::types::VisionError;

#[derive(Debug, Clone, Serialize)]
pub struct AnimationFrames {
    pub frames: Vec<FrameHandle>,
    pub duration_ms: u32,
    pub display_id: u32,
}

const MAX_DURATION_MS: u32 = 5_000;

pub fn animation_frames(
    pipeline: &VisionPipeline,
    duration_ms: u32,
) -> Result<AnimationFrames, VisionError> {
    if duration_ms == 0 || duration_ms > MAX_DURATION_MS {
        return Err(VisionError::Other(anyhow::anyhow!(
            "duration_ms must be in 1..={MAX_DURATION_MS} (got {duration_ms})"
        )));
    }
    let ring = pipeline.ring();
    let head = ring.head_seq();
    if head == 0 {
        return Ok(AnimationFrames {
            frames: Vec::new(),
            duration_ms,
            display_id: DEFAULT_DISPLAY_ID,
        });
    }
    // Newest first; stop when we've walked past the window.
    let head_ts = ring.slot_ts_us(head).unwrap_or(0);
    let cutoff = head_ts.saturating_sub((duration_ms as u64) * 1_000);
    let mut out = Vec::new();
    let mut seq = head;
    while seq > 0 {
        if let Some(handle) = ring.handle_for_seq(seq) {
            if handle.ts_us < cutoff {
                break;
            }
            out.push(handle);
            seq -= 1;
        } else {
            break;
        }
        if out.len() >= 256 {
            // Hard cap — defends against runaway windows on a fast ring.
            break;
        }
    }
    out.reverse();
    Ok(AnimationFrames {
        frames: out,
        duration_ms,
        display_id: DEFAULT_DISPLAY_ID,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::VisionPipeline;
    use crate::metrics::Histograms;
    use crate::vlm::VlmConfig;

    #[tokio::test]
    async fn empty_when_ring_empty() {
        let p = VisionPipeline::new(
            format!("anim-empty-{}", std::process::id()),
            "t",
            Histograms::new(),
            VlmConfig::Off,
        )
        .expect("new");
        let r = animation_frames(&p, 1000).expect("anim");
        assert!(r.frames.is_empty());
        assert_eq!(r.duration_ms, 1000);
    }

    #[tokio::test]
    async fn rejects_zero_or_huge_duration() {
        let p = VisionPipeline::new(
            format!("anim-bad-{}", std::process::id()),
            "t",
            Histograms::new(),
            VlmConfig::Off,
        )
        .expect("new");
        assert!(animation_frames(&p, 0).is_err());
        assert!(animation_frames(&p, MAX_DURATION_MS + 1).is_err());
    }
}
