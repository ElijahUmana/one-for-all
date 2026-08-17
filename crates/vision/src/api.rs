//! Public per-tab orchestrator. Owns capture, frame ring, diff, OCR cache,
//! and subscriber. Exposes the tool-surface entry points used by the
//! broker for the `vision.*` JSON-RPC methods.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bumpalo::Bump;
use parking_lot::{Mutex, RwLock};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::capture::{decode_to_rgba, enable_screencast, CaptureConfig, CaptureHandle};
use crate::diff::{compute_hash_grid, diff, Bbox, TileChange, DEFAULT_TILE_SIZE};
use crate::frame_ring::{FrameHandle, FrameRing, DEFAULT_SLOT_BYTES, DEFAULT_SLOT_COUNT};
use crate::metrics::Histograms;
use crate::ocr::{default_backend, OcrCache, TextRegion};
use crate::subscribe::{NotificationSink, VisionSubscriber};
use crate::types::{Frame, FrameFormat, TextMatch, TextQuery, VisionError};
use crate::vlm::{ActionContext, VlmBackend, VlmConfig, VlmVerdict};

/// Vision-pipeline mode for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VisionMode {
    Off,
    OnDemand,
    Continuous,
}

impl Default for VisionMode {
    fn default() -> Self {
        VisionMode::Off
    }
}

/// Per-session vision configuration.
#[derive(Debug, Clone, Default)]
pub struct VisionConfig {
    pub mode: VisionMode,
    pub max_fps: u32,
    pub idle_fps: u32,
    pub format: Option<FrameFormat>,
    pub vlm: VlmConfig,
}

impl VisionConfig {
    pub fn continuous(max_fps: u32) -> Self {
        Self {
            mode: VisionMode::Continuous,
            max_fps,
            idle_fps: 5,
            format: Some(FrameFormat::Jpeg),
            vlm: VlmConfig::Off,
        }
    }
}

/// One vision pipeline per (session, tab). Cheap to clone (`Arc` inside).
pub struct VisionPipeline {
    inner: Arc<PipelineInner>,
}

impl Clone for VisionPipeline {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct PipelineInner {
    session_id: String,
    tab_id: String,
    ring: Arc<FrameRing>,
    metrics: Histograms,
    ocr: OcrCache,
    vlm: Box<dyn VlmBackend>,
    state: RwLock<PipelineState>,
    capture: Mutex<Option<CaptureHandle>>,
    pump_task: Mutex<Option<JoinHandle<()>>>,
    fps_watch: watch::Sender<(u32, u32)>,
    last_frame: RwLock<Option<Frame>>,
}

struct PipelineState {
    /// `(width, height)` of the most recent decoded frame.
    last_dims: Option<(u32, u32)>,
    /// Per-tile hash grid for the most recent frame.
    last_hashes: Option<Vec<u64>>,
    /// Ring of (seq, captured_us, changed_tile_count) for the last
    /// `STABILITY_WINDOW` frames; powers the stability score.
    stability_window: VecDeque<(u64, u64, u32, u32)>,
    /// Ring of (seq, captured_us, Vec<TileChange>) for the last
    /// `CHANGES_HISTORY` frames; powers `vision.changed_since`. Capped so
    /// memory stays bounded.
    changes_history: VecDeque<(u64, u64, Vec<TileChange>)>,
    /// Ring of decoded images for recent frames, keyed by seq. Powers
    /// `vision.diff.semantic {prev,next,...}` without having to infer the
    /// encoded-image format of historical frame-ring slots.
    decoded_history: VecDeque<(u64, crate::types::DecodedImage)>,
}

const STABILITY_WINDOW: usize = 8;
const CHANGES_HISTORY: usize = 64;

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            last_dims: None,
            last_hashes: None,
            stability_window: VecDeque::with_capacity(STABILITY_WINDOW),
            changes_history: VecDeque::with_capacity(CHANGES_HISTORY),
            decoded_history: VecDeque::with_capacity(CHANGES_HISTORY),
        }
    }
}

/// Stability classification surfaced on every frame and via
/// `vision.stability`. Driven by the rolling tile-change ratio over the
/// last [`STABILITY_WINDOW`] frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StabilityState {
    Loading,
    Settling,
    Stable,
}

impl StabilityState {
    pub fn from_score(score: f32) -> Self {
        if score >= 0.95 {
            StabilityState::Stable
        } else if score >= 0.80 {
            StabilityState::Settling
        } else {
            StabilityState::Loading
        }
    }
}

/// Reported by `vision.stability` and stamped on every emitted frame.
#[derive(Debug, Clone, Serialize)]
pub struct StabilityReport {
    pub seq: u64,
    pub score: f32,
    pub state: StabilityState,
    pub window_frames: u32,
}

/// Output of a single pipeline tick. Mostly used by tests.
#[derive(Debug, Clone)]
pub struct PipelineTick {
    pub seq: u64,
    pub changed_tiles: Vec<TileChange>,
    pub ocr_delta: Vec<TextRegion>,
    /// SPEC §11 V4 — `% tiles unchanged from previous N frames`. 1.0 =
    /// pixel-static; <0.80 = page actively repainting.
    pub stability: f32,
    /// Discretized version of `stability` (loading / settling / stable).
    pub state: StabilityState,
}

impl VisionPipeline {
    /// Build a new pipeline for `(session_id, tab_id)`. The frame ring is
    /// created at the conventional `${TMPDIR}/ofa-frames-<sess>-<tab>` path.
    pub fn new(
        session_id: impl Into<String>,
        tab_id: impl Into<String>,
        metrics: Histograms,
        vlm: VlmConfig,
    ) -> Result<Self, VisionError> {
        let session_id = session_id.into();
        let tab_id = tab_id.into();
        let tmp = std::env::temp_dir();
        // Disambiguate the shm path with a per-process monotonic counter
        // so concurrent test threads (and concurrent sessions sharing the
        // same `(session_id, tab_id)` pair, e.g. test fixtures) don't
        // collide on the same backing file. Production callers always
        // supply distinct ids; the counter is purely defensive.
        static SHM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SHM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let path = tmp.join(format!("ofa-frames-{session_id}-{tab_id}-{pid}-{seq}"));
        let ring = FrameRing::create(path, DEFAULT_SLOT_BYTES, DEFAULT_SLOT_COUNT)?;
        let ocr = OcrCache::new(default_backend(), metrics.clone(), 256);
        let vlm_backend = crate::vlm::build_backend(&vlm);
        let (fps_watch, _) = watch::channel((30u32, 5u32));
        Ok(Self {
            inner: Arc::new(PipelineInner {
                session_id,
                tab_id,
                ring,
                metrics,
                ocr,
                vlm: vlm_backend,
                state: RwLock::new(PipelineState::default()),
                capture: Mutex::new(None),
                pump_task: Mutex::new(None),
                fps_watch,
                last_frame: RwLock::new(None),
            }),
        })
    }

    pub fn ring(&self) -> &Arc<FrameRing> {
        &self.inner.ring
    }

    pub fn metrics(&self) -> &Histograms {
        &self.inner.metrics
    }

    pub fn shm_path(&self) -> &Path {
        self.inner.ring.path()
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn tab_id(&self) -> &str {
        &self.inner.tab_id
    }

    /// Boost FPS to the configured `max_fps` (action in flight).
    pub fn boost(&self) {
        if let Some(c) = self.inner.capture.lock().as_ref() {
            c.boost();
        }
    }

    /// Drop FPS to `idle_fps`.
    pub fn relax(&self) {
        if let Some(c) = self.inner.capture.lock().as_ref() {
            c.relax();
        }
    }

    /// Replace the active FPS pair.
    pub fn set_fps(&self, max_fps: u32, idle_fps: u32) -> Result<(), VisionError> {
        if max_fps == 0 || idle_fps == 0 {
            return Err(VisionError::Other(anyhow::anyhow!("fps must be > 0")));
        }
        self.inner
            .fps_watch
            .send((max_fps, idle_fps))
            .map_err(|_| VisionError::Other(anyhow::anyhow!("fps watch closed")))?;
        // Best-effort live re-arm via boost flag toggle (the capture task
        // will re-arm with the new nth on the next watch tick).
        self.boost();
        self.relax();
        Ok(())
    }

    /// Start continuous capture on a Page. Idempotent — calling twice is
    /// a no-op. The pipeline pump task takes the capture's frame stream
    /// and runs each frame through diff → OCR → subscriber.
    pub async fn start_continuous(
        self: &Arc<Self>,
        page: Arc<browser_engine::Page>,
        cfg: CaptureConfig,
        sink: Arc<dyn NotificationSink>,
    ) -> Result<(), VisionError> {
        if self.inner.capture.lock().is_some() {
            return Ok(());
        }
        let (cap, frames_rx_owned) = enable_screencast(
            page.clone(),
            Arc::clone(&self.inner.ring),
            self.inner.metrics.clone(),
            cfg.clone(),
        )
        .await?;
        *self.inner.capture.lock() = Some(cap);
        let mut frames_rx = frames_rx_owned;

        let pipeline = Arc::clone(self);
        let subscriber = VisionSubscriber::new(
            Arc::clone(&sink),
            self.inner.session_id.clone(),
            self.inner.tab_id.clone(),
            self.inner.metrics.clone(),
        );

        let task = tokio::spawn(async move {
            while let Some(frame) = frames_rx.recv().await {
                let started = Instant::now();
                match pipeline
                    .tick_with_subscriber(frame, &subscriber, started)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "vision pipeline tick failed"),
                }
            }
            debug!("vision pump task exiting");
        });
        *self.inner.pump_task.lock() = Some(task);
        Ok(())
    }

    /// Stop continuous capture.
    pub async fn stop(&self) {
        if let Some(t) = self.inner.pump_task.lock().take() {
            t.abort();
        }
        if let Some(c) = self.inner.capture.lock().take() {
            c.stop().await;
        }
    }

    /// Run one frame through diff + OCR. Used by the pump and tests.
    pub async fn tick(&self, frame: Frame) -> Result<PipelineTick, VisionError> {
        let arena = Bump::new();
        let decoded = frame
            .decoded
            .clone()
            .ok_or_else(|| VisionError::Other(anyhow::anyhow!("frame missing decoded image")))?;

        // Diff (or first-frame seed).
        let diff_started = Instant::now();
        let (changes_owned, total_tiles) = {
            let mut state = self.inner.state.write();
            match (state.last_dims, &state.last_hashes) {
                (Some(dims), Some(prev)) if dims == (decoded.width, decoded.height) => {
                    let res = diff(&decoded, prev.as_slice(), dims, DEFAULT_TILE_SIZE, &arena)?;
                    let changes_owned: Vec<TileChange> = res.changes.iter().copied().collect();
                    let total = res.grid_w * res.grid_h;
                    state.last_hashes = Some(res.hashes);
                    state.last_dims = Some((res.width, res.height));
                    (changes_owned, total)
                }
                _ => {
                    let hashes = compute_hash_grid(&decoded, DEFAULT_TILE_SIZE)?;
                    let total = ((decoded.width + DEFAULT_TILE_SIZE - 1) / DEFAULT_TILE_SIZE)
                        * ((decoded.height + DEFAULT_TILE_SIZE - 1) / DEFAULT_TILE_SIZE);
                    state.last_hashes = Some(hashes);
                    state.last_dims = Some((decoded.width, decoded.height));
                    (Vec::new(), total)
                }
            }
        };
        self.inner
            .metrics
            .diff_ms()
            .record(diff_started.elapsed().as_millis() as u64);

        // Stability: rolling fraction of tiles unchanged across the window.
        let (stability, state_kind) = {
            let mut s = self.inner.state.write();
            let changed = changes_owned.len() as u32;
            s.stability_window
                .push_back((frame.seq, frame.raw.captured_us, changed, total_tiles));
            while s.stability_window.len() > STABILITY_WINDOW {
                s.stability_window.pop_front();
            }
            // Score = 1 - (sum_changed / sum_total) clamped to [0, 1].
            let mut sum_changed = 0u64;
            let mut sum_total = 0u64;
            for (_, _, c, t) in s.stability_window.iter() {
                sum_changed += *c as u64;
                sum_total += *t as u64;
            }
            let score = if sum_total == 0 {
                1.0
            } else {
                1.0 - (sum_changed as f32 / sum_total as f32).clamp(0.0, 1.0)
            };
            (score, StabilityState::from_score(score))
        };

        // Push the frame's changes into the bounded history ring.
        {
            let mut s = self.inner.state.write();
            s.changes_history
                .push_back((frame.seq, frame.raw.captured_us, changes_owned.clone()));
            while s.changes_history.len() > CHANGES_HISTORY {
                s.changes_history.pop_front();
            }
            s.decoded_history.push_back((frame.seq, decoded.clone()));
            while s.decoded_history.len() > CHANGES_HISTORY {
                s.decoded_history.pop_front();
            }
        }

        // OCR on changed tiles. On the seed frame we OCR the full frame.
        let ocr_input: Vec<TileChange> = if changes_owned.is_empty() {
            vec![TileChange {
                tile_x: 0,
                tile_y: 0,
                bbox: Bbox {
                    x: 0,
                    y: 0,
                    w: decoded.width,
                    h: decoded.height,
                },
                prev_hash: 0,
                next_hash: 0,
            }]
        } else {
            changes_owned.clone()
        };
        let ocr_delta = self.inner.ocr.recognize_tiles(&decoded, &ocr_input).await?;

        *self.inner.last_frame.write() = Some(frame.clone());

        Ok(PipelineTick {
            seq: frame.seq,
            changed_tiles: changes_owned,
            ocr_delta,
            stability,
            state: state_kind,
        })
    }

    /// Ingest one encoded screencast/screenshot frame into the shared-memory
    /// ring, decode it to RGBA, and run the normal diff/OCR pipeline. Used by
    /// on-demand priming paths that want V4/U10 cache semantics without
    /// standing up continuous capture.
    pub async fn ingest_raw_frame(
        &self,
        raw: crate::types::ScreencastFrame,
    ) -> Result<PipelineTick, VisionError> {
        let len = raw.bytes.len() as u32;
        let mut guard = self.inner.ring.acquire_write(len, raw.captured_us)?;
        guard.write(raw.bytes.as_slice())?;
        let seq = guard.commit();
        let decoded = decode_to_rgba(&raw)?;
        let frame = Frame {
            seq,
            raw,
            decoded: Some(decoded),
        };
        self.tick(frame).await
    }

    /// SPEC §11 V4 — return the union of changed tiles since the given
    /// timestamp. Beats re-running diff on the agent side.
    pub fn changed_since(&self, since_ts_us: u64) -> Vec<TileChange> {
        let s = self.inner.state.read();
        let mut out = Vec::with_capacity(16);
        for (_, ts, tiles) in s.changes_history.iter() {
            if *ts > since_ts_us {
                out.extend_from_slice(tiles);
            }
        }
        out
    }

    /// SPEC §11 V4 — current stability snapshot (score + discretized state +
    /// the seq of the most recently observed frame).
    pub fn stability_now(&self) -> StabilityReport {
        let s = self.inner.state.read();
        let mut sum_changed = 0u64;
        let mut sum_total = 0u64;
        let mut latest_seq = 0u64;
        for (seq, _, c, t) in s.stability_window.iter() {
            sum_changed += *c as u64;
            sum_total += *t as u64;
            if *seq > latest_seq {
                latest_seq = *seq;
            }
        }
        let score = if sum_total == 0 {
            1.0
        } else {
            1.0 - (sum_changed as f32 / sum_total as f32).clamp(0.0, 1.0)
        };
        StabilityReport {
            seq: latest_seq,
            score,
            state: StabilityState::from_score(score),
            window_frames: s.stability_window.len() as u32,
        }
    }

    async fn tick_with_subscriber(
        self: &Arc<Self>,
        frame: Frame,
        subscriber: &VisionSubscriber,
        started: Instant,
    ) -> Result<(), VisionError> {
        let tick = self.tick(frame.clone()).await?;
        let handle = self.inner.ring.handle_for_seq(frame.seq).ok_or_else(|| {
            VisionError::Other(anyhow::anyhow!("frame ring lost seq {}", frame.seq))
        })?;
        subscriber.emit(
            &frame,
            handle,
            tick.changed_tiles,
            tick.ocr_delta,
            Some(tick.stability),
            Some(tick.state),
            started,
        );
        Ok(())
    }

    /// SPEC §7 — `vision.read_text`. Returns the cached text regions
    /// intersecting `region` (or all if region is `None`).
    pub async fn read_text(&self, region: Option<Bbox>) -> Result<Vec<TextRegion>, VisionError> {
        let start = Instant::now();
        let out = self.inner.ocr.snapshot_regions(region);
        self.inner
            .metrics
            .find_text_ms()
            .record(start.elapsed().as_millis() as u64);
        Ok(out)
    }

    /// SPEC §7 — `vision.find_text`. Substring or regex match against the
    /// cached OCR regions. Records `vision.find_text_ms` for SLO check.
    pub async fn find_text(&self, query: TextQuery) -> Result<Vec<TextMatch>, VisionError> {
        let start = Instant::now();
        let regions = self.inner.ocr.snapshot_regions(query.region);
        let mut out = Vec::with_capacity(8);
        if query.is_regex {
            let re = RegexBuilder::new(&query.query)
                .case_insensitive(true)
                .build()?;
            for r in regions {
                if let Some(m) = re.find(&r.text) {
                    let ratio = (m.end() - m.start()) as f32 / r.text.len().max(1) as f32;
                    out.push(TextMatch {
                        region: r.bbox,
                        text: r.text,
                        score: r.confidence * (0.5 + 0.5 * ratio),
                    });
                }
            }
        } else {
            let needle = query.query.to_lowercase();
            for r in regions {
                let hay = r.text.to_lowercase();
                if hay.contains(&needle) {
                    let ratio = needle.len() as f32 / hay.len().max(1) as f32;
                    out.push(TextMatch {
                        region: r.bbox,
                        text: r.text,
                        score: r.confidence * (0.5 + 0.5 * ratio),
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.inner
            .metrics
            .find_text_ms()
            .record(start.elapsed().as_millis() as u64);
        Ok(out)
    }

    /// SPEC §7 — `vision.compare`. 32×32 average-hash perceptual compare
    /// against a reference image on disk. Returns `1.0` for identical and
    /// approaches `0.0` for fully different.
    pub async fn compare(&self, ref_path: &Path) -> Result<f32, VisionError> {
        let last = self
            .inner
            .last_frame
            .read()
            .clone()
            .ok_or_else(|| VisionError::Other(anyhow::anyhow!("no captured frame yet")))?;
        let last_decoded = last
            .decoded
            .clone()
            .ok_or_else(|| VisionError::Other(anyhow::anyhow!("frame not decoded")))?;
        let ref_img = image::open(ref_path).map_err(|e| VisionError::Image(e.to_string()))?;
        let ref_gray = ref_img.to_luma8();
        let (rw, rh) = ref_gray.dimensions();
        let _ = (rw, rh);
        let ref_resized =
            image::imageops::resize(&ref_gray, 32, 32, image::imageops::FilterType::Triangle);
        let last_rgba = image::RgbaImage::from_raw(
            last_decoded.width,
            last_decoded.height,
            (*last_decoded.rgba).clone(),
        )
        .ok_or_else(|| VisionError::Image("rgba frame too small".into()))?;
        let last_gray = image::imageops::grayscale(&last_rgba);
        let last_resized =
            image::imageops::resize(&last_gray, 32, 32, image::imageops::FilterType::Triangle);
        // Compare 1024 pixels.
        let mut diff_acc: u32 = 0;
        for (a, b) in ref_resized.pixels().zip(last_resized.pixels()) {
            let d = (a.0[0] as i32 - b.0[0] as i32).abs() as u32;
            diff_acc += d;
        }
        // Max diff per pixel = 255; total = 1024 × 255.
        let max = 1024u32 * 255;
        Ok(1.0 - (diff_acc as f32 / max as f32))
    }

    /// SPEC §11 V4 — pre-action VLM verification hook. Always returns a
    /// verdict (`skipped` when no backend configured / no frame yet).
    /// Snapshot the most recently decoded frame's pixel buffer. Returns
    /// `None` if no frame has flowed through `tick` yet. Used by the
    /// sub-granularity tools (`vision.pixel`, `vision.color.palette`,
    /// `vision.region.classify`, …) which need direct RGBA access.
    pub fn last_decoded(&self) -> Option<Arc<crate::types::DecodedImage>> {
        let f = self.inner.last_frame.read().clone()?;
        f.decoded.map(|d| Arc::new(d))
    }

    /// `page_scale_factor` of the most recent frame's CDP metadata. Used
    /// by the CSS-px coordinate conversion helpers.
    pub fn last_page_scale(&self) -> Option<f64> {
        self.inner
            .last_frame
            .read()
            .as_ref()
            .map(|f| f.raw.metadata.page_scale_factor)
    }

    /// Test-only helper exposing the OCR cache for direct injection.
    /// Used by sub-granularity-tool unit tests that need OCR overlap signal
    /// without standing up a real backend.
    #[doc(hidden)]
    pub fn ocr_cache_test_inject(
        &self,
        key: (u32, u32, u64),
        regions: Vec<crate::ocr::TextRegion>,
    ) {
        self.inner.ocr.cache_test_inject(key, regions);
    }

    /// Look up the first cached `OcrEntry` rectangle that contains the
    /// requested point. Used by `vision.text.style` to find which OCR
    /// bbox a CSS-px coordinate sits inside.
    pub fn ocr_region_at(&self, x: u32, y: u32) -> Option<crate::ocr::TextRegion> {
        let regions = self.inner.ocr.snapshot_regions(None);
        regions.into_iter().find(|r| {
            x >= r.bbox.x && y >= r.bbox.y && x < r.bbox.x + r.bbox.w && y < r.bbox.y + r.bbox.h
        })
    }

    /// Snapshot the rolling tile-change history (used by
    /// `vision.loading.detect`). Returns `(seq, captured_us, changed_count,
    /// total_tiles)` quadruples in arrival order.
    pub fn change_window(&self) -> Vec<(u64, u64, u32, u32)> {
        let s = self.inner.state.read();
        s.stability_window.iter().copied().collect()
    }

    /// Look up a recent decoded frame by its `vision.frame` sequence number.
    /// Used by `vision.diff.semantic {prev,next,...}` to compare two real
    /// historical frames instead of reusing the latest image twice.
    pub fn decoded_frame_by_seq(&self, seq: u64) -> Option<Arc<crate::types::DecodedImage>> {
        let s = self.inner.state.read();
        s.decoded_history
            .iter()
            .find(|(frame_seq, _)| *frame_seq == seq)
            .map(|(_, frame)| Arc::new(frame.clone()))
    }

    pub async fn verify_action_on_frame(
        &self,
        image: Arc<crate::types::DecodedImage>,
        action: ActionContext,
    ) -> Result<VlmVerdict, VisionError> {
        let started = Instant::now();
        let res = self.inner.vlm.verify(image, &action).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.inner.metrics.vlm_ms().record(elapsed_ms);
        res
    }

    pub async fn pre_action_verify(
        &self,
        action: ActionContext,
    ) -> Result<VlmVerdict, VisionError> {
        let last = self.inner.last_frame.read().clone().and_then(|f| f.decoded);
        let Some(image) = last else {
            return Ok(VlmVerdict::skipped());
        };
        self.verify_action_on_frame(Arc::new(image), action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FrameFormat, ScreencastFrame, ScreencastFrameMetadata};
    use std::sync::Arc;

    fn synthesize_frame(seq: u64, w: u32, h: u32, fill: u8) -> Frame {
        Frame {
            seq,
            raw: ScreencastFrame {
                bytes: Arc::new(vec![0; 4]),
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
            decoded: Some(crate::types::DecodedImage {
                width: w,
                height: h,
                rgba: Arc::new(vec![fill; (w * h * 4) as usize]),
                captured_us: 0,
            }),
        }
    }

    #[tokio::test]
    async fn first_frame_seeds_and_emits_empty_diff() {
        let p = VisionPipeline::new("sess", "tab", Histograms::new(), VlmConfig::Off).expect("new");
        let tick = p
            .tick(synthesize_frame(1, 128, 128, 0))
            .await
            .expect("tick");
        assert_eq!(tick.seq, 1);
        // First frame: empty changes, but OCR ran across full frame.
        assert!(tick.changed_tiles.is_empty());
    }

    #[tokio::test]
    async fn second_frame_with_change_yields_changed_tiles() {
        let p = VisionPipeline::new("sess", "tab", Histograms::new(), VlmConfig::Off).expect("new");
        p.tick(synthesize_frame(1, 128, 128, 0))
            .await
            .expect("seed");
        // Mutate one pixel by rebuilding the frame with one tile dirty.
        let mut bytes = vec![0u8; 128 * 128 * 4];
        bytes[64 * 128 * 4 + 65 * 4] = 255; // pixel at (65, 64)
        let f = Frame {
            seq: 2,
            raw: synthesize_frame(2, 128, 128, 0).raw,
            decoded: Some(crate::types::DecodedImage {
                width: 128,
                height: 128,
                rgba: Arc::new(bytes),
                captured_us: 0,
            }),
        };
        let tick = p.tick(f).await.expect("tick");
        assert_eq!(tick.changed_tiles.len(), 1);
    }

    #[tokio::test]
    async fn find_text_substring_and_regex() {
        let p = VisionPipeline::new("sess", "tab", Histograms::new(), VlmConfig::Off).expect("new");
        // Manually inject OCR cache state.
        p.inner.ocr.cache_test_inject(
            (0, 0, 1),
            vec![TextRegion {
                bbox: Bbox {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 20,
                },
                text: "Welcome to one-for-all".into(),
                confidence: 0.99,
            }],
        );
        p.inner.ocr.cache_test_inject(
            (0, 1, 2),
            vec![TextRegion {
                bbox: Bbox {
                    x: 0,
                    y: 20,
                    w: 100,
                    h: 20,
                },
                text: "Hello world".into(),
                confidence: 0.9,
            }],
        );
        let r = p
            .find_text(TextQuery {
                query: "one-for-all".into(),
                is_regex: false,
                region: None,
            })
            .await
            .expect("find");
        assert_eq!(r.len(), 1);
        let r = p
            .find_text(TextQuery {
                query: r"hel+o\s+\w+".into(),
                is_regex: true,
                region: None,
            })
            .await
            .expect("regex");
        assert_eq!(r.len(), 1);
        assert!(r[0].text.contains("Hello"));
    }

    #[tokio::test]
    async fn find_text_p99_under_10ms() {
        let p = VisionPipeline::new("sess", "tab", Histograms::new(), VlmConfig::Off).expect("new");
        for i in 0..500u32 {
            p.inner.ocr.cache_test_inject(
                (i, i, i as u64),
                vec![TextRegion {
                    bbox: Bbox {
                        x: i,
                        y: i,
                        w: 1,
                        h: 1,
                    },
                    text: format!("Region {i}"),
                    confidence: 0.8,
                }],
            );
        }
        // Warm up — first call may pay for arena/regex setup.
        for _ in 0..5 {
            let _ = p
                .find_text(TextQuery {
                    query: "Region 7".into(),
                    is_regex: false,
                    region: None,
                })
                .await;
        }
        for _ in 0..1000 {
            let _ = p
                .find_text(TextQuery {
                    query: "Region 7".into(),
                    is_regex: false,
                    region: None,
                })
                .await
                .expect("find");
        }
        let p99 = p.metrics().find_text_ms().percentile(0.99);
        assert!(p99 < 10, "p99 = {p99}ms — must be < 10ms (SPEC §11 V4)");
    }
}
