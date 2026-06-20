//! CDP `Page.startScreencast` capture — one task per page. Decodes the
//! base64 frame, writes it into the per-tab [`crate::FrameRing`], emits a
//! [`Frame`] downstream, and acks every frame to keep CDP flowing.
//!
//! Idle/active boost: subscribers can flip a [`tokio::sync::watch`] sender
//! between `false` (idle) and `true` (action in flight); the capture task
//! reconfigures `Page.startScreencast` with a higher `every_nth_frame`
//! when idle and a lower one when active. CDP supports re-entry — calling
//! `Page.startScreencast` again replaces the active config.
//!
//! ## SPEC §11 V4 latency budget
//!
//! Capture-stage histogram is recorded per frame, exported via
//! [`crate::Histograms::capture_ms`].

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use browser_engine::page::{Page, ScreencastFramePayload};
use observability::trace::{TraceEvent, TraceSink};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::frame_ring::FrameRing;
use crate::metrics::Histograms;
use crate::types::{
    DecodedImage, Frame, FrameFormat, ScreencastFrame as VisionScreencastFrame,
    ScreencastFrameMetadata, VisionError,
};

/// Configurable capture parameters per page.
#[derive(Clone)]
pub struct CaptureConfig {
    pub format: FrameFormat,
    pub quality: u8,
    pub max_fps: u32,
    pub idle_fps: u32,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    /// SPEC §10 M10 — when both `trace=true` and `vision=continuous`, every
    /// screencast frame is doubled-stored to the trace dir. The frame ring
    /// stays ephemeral (mmap, fixed slots, overwritten on wrap-around); the
    /// trace gets the durable copy via `TraceSink::save_screencast_frame`
    /// + a `Screenshot` record. `None` (default) is a no-op.
    pub trace_sink: Option<Arc<dyn TraceSink>>,
    /// SPEC §10 M10 — `tab_id` to embed in the doubled-stored screenshot
    /// trace records. Optional because it isn't strictly part of the
    /// screencast capture path; the broker passes it when wiring.
    pub trace_tab_id: Option<String>,
    /// SPEC §10 M10 — `session_id` to embed in the doubled-stored screenshot
    /// trace records. Optional for the same reason.
    pub trace_session_id: Option<String>,
}

impl std::fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("format", &self.format)
            .field("quality", &self.quality)
            .field("max_fps", &self.max_fps)
            .field("idle_fps", &self.idle_fps)
            .field("max_width", &self.max_width)
            .field("max_height", &self.max_height)
            .field("trace_sink", &self.trace_sink.is_some())
            .field("trace_tab_id", &self.trace_tab_id)
            .field("trace_session_id", &self.trace_session_id)
            .finish()
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            format: FrameFormat::Jpeg,
            quality: 60,
            max_fps: 30,
            idle_fps: 5,
            max_width: None,
            max_height: None,
            trace_sink: None,
            trace_tab_id: None,
            trace_session_id: None,
        }
    }
}

/// Owned by the broker per (session, tab). Drop stops the capture task
/// and tells CDP to stop screencasting. Frames flow out via the
/// `mpsc::Receiver` returned alongside this handle from
/// [`enable_screencast`].
pub struct CaptureHandle {
    pub action_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    page: Arc<Page>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

impl CaptureHandle {
    /// Signal an action is starting (boost FPS).
    pub fn boost(&self) {
        let _ = self.action_tx.send(true);
    }

    /// Signal idle (drop back to `idle_fps`).
    pub fn relax(&self) {
        let _ = self.action_tx.send(false);
    }

    /// Stop the capture loop and tell CDP to stop screencasting.
    pub async fn stop(mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.page.stop_screencast().await;
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        // Best-effort shutdown if `stop()` wasn't awaited.
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// Spawn a capture task. Returns the handle (for fps boost / stop) plus
/// a frame receiver the caller drives through the pipeline stages.
pub async fn enable_screencast(
    page: Arc<Page>,
    ring: Arc<FrameRing>,
    metrics: Histograms,
    cfg: CaptureConfig,
) -> Result<(CaptureHandle, mpsc::Receiver<Frame>), VisionError> {
    let (frames_tx, frames_rx) = mpsc::channel::<Frame>(64);
    let (action_tx, mut action_rx) = watch::channel(false);
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped_task = Arc::clone(&stopped);

    // Subscribe BEFORE starting screencast so we don't miss the first frame.
    let mut frame_rx = page.screencast_subscribe();

    // Start screencast at idle FPS — boost() can crank it up.
    let idle_nth = fps_to_nth(cfg.idle_fps);
    page.start_screencast(
        cfg.format.cdp_name(),
        cfg.quality,
        idle_nth,
        cfg.max_width,
        cfg.max_height,
    )
    .await
    .map_err(|e| VisionError::Cdp(e.to_string()))?;

    let page_for_task = Arc::clone(&page);
    let trace_sink = cfg.trace_sink.clone();
    let trace_tab_id = cfg.trace_tab_id.clone();
    let trace_session_id = cfg.trace_session_id.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = action_rx.changed() => {
                    if changed.is_err() { break; }
                    let active = *action_rx.borrow();
                    let nth = fps_to_nth(if active { cfg.max_fps } else { cfg.idle_fps });
                    if let Err(e) = page_for_task
                        .start_screencast(cfg.format.cdp_name(), cfg.quality, nth, cfg.max_width, cfg.max_height)
                        .await
                    {
                        warn!(error = %e, "screencast re-arm failed");
                    }
                }
                msg = frame_rx.recv() => {
                    let payload = match msg {
                        Ok(p) => p,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!(n = n, "screencast lagged");
                            continue;
                        }
                        Err(_) => break,
                    };
                    let start = Instant::now();
                    let res = handle_one_frame(
                        &page_for_task,
                        &ring,
                        &frames_tx,
                        cfg.format,
                        payload,
                        trace_sink.as_ref(),
                        trace_tab_id.as_deref(),
                        trace_session_id.as_deref(),
                    ).await;
                    metrics.capture_ms().record(start.elapsed().as_millis() as u64);
                    if let Err(e) = res {
                        warn!(error = %e, "frame capture failed");
                    }
                    if stopped_task.load(std::sync::atomic::Ordering::Acquire) { break; }
                }
            }
        }
        debug!("capture task exited");
    });

    Ok((
        CaptureHandle {
            action_tx,
            task: Some(task),
            page,
            stopped,
        },
        frames_rx,
    ))
}

/// Convert FPS → CDP `every_nth_frame`. CDP samples at ~60Hz, so
/// `nth = max(1, 60 / fps)` is a useful approximation.
fn fps_to_nth(fps: u32) -> u32 {
    if fps == 0 {
        return 60;
    }
    (60u32 / fps).max(1)
}

/// Per-frame work: ack to CDP, decode base64, decode image, write to ring,
/// emit downstream.
/// Per-frame work: ack to CDP, decode base64, decode image, write to ring,
/// emit downstream. When a [`TraceSink`] is plumbed (the broker provides it
/// when both `trace=true` and `vision=continuous`), the encoded bytes also
/// get persisted to the trace dir's `screenshots/` subdir as a durable copy
/// of the otherwise-ephemeral frame ring slot.
#[allow(clippy::too_many_arguments)]
async fn handle_one_frame(
    page: &Arc<Page>,
    ring: &Arc<FrameRing>,
    tx: &mpsc::Sender<Frame>,
    format: FrameFormat,
    payload: ScreencastFramePayload,
    trace_sink: Option<&Arc<dyn TraceSink>>,
    trace_tab_id: Option<&str>,
    trace_session_id: Option<&str>,
) -> Result<(), VisionError> {
    // Ack first so CDP keeps streaming.
    let _ = page
        .screencast_frame_ack(payload.cdp_frame_session_id)
        .await;

    let bytes = base64::engine::general_purpose::STANDARD.decode(payload.data_b64.as_bytes())?;
    let bytes = Arc::new(bytes);

    // SPEC §10 M10 — doubled-storage. Persist the encoded frame to the
    // trace directory BEFORE we hand the buffer off downstream so a slow
    // sink can never block the ring or the diff stage.
    if let Some(sink) = trace_sink {
        let ext = match format {
            FrameFormat::Jpeg => "jpg",
            FrameFormat::Png => "png",
        };
        match sink.save_screencast_frame(ext, bytes.as_slice()) {
            Ok(rel_path) => {
                let session_id = trace_session_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| page.cdp_session_id().as_str().to_owned());
                let tab_id = trace_tab_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| page.tab_id().0.clone());
                sink.record(TraceEvent::Screenshot {
                    ts_ms: sink.now_ms(),
                    session_id,
                    tab_id,
                    after_action: "vision.continuous".into(),
                    png_path: rel_path,
                });
            }
            Err(e) => {
                debug!(error = %e, "vision frame doubled-storage write failed");
            }
        }
    }

    // Stuff the encoded bytes into the ring.
    let captured_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let len = bytes.len() as u32;
    if len > ring.slot_bytes() {
        return Err(VisionError::FrameTooLarge {
            len: len as usize,
            cap: ring.slot_bytes() as usize,
        });
    }
    let mut g = ring.acquire_write(len, captured_us)?;
    g.write(&bytes)?;
    let seq = g.commit();

    // Decode metadata into our normalized struct.
    let meta = ScreencastFrameMetadata {
        offset_top: payload
            .metadata
            .get("offsetTop")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        page_scale_factor: payload
            .metadata
            .get("pageScaleFactor")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        device_width: payload
            .metadata
            .get("deviceWidth")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        device_height: payload
            .metadata
            .get("deviceHeight")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        scroll_offset_x: payload
            .metadata
            .get("scrollOffsetX")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        scroll_offset_y: payload
            .metadata
            .get("scrollOffsetY")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        timestamp: payload
            .metadata
            .get("timestamp")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
    };

    let raw = VisionScreencastFrame {
        bytes: Arc::clone(&bytes),
        format,
        metadata: meta,
        session_id: page.cdp_session_id().as_str().to_owned(),
        captured_us,
    };

    // Decode into RGBA so downstream stages have pixel access.
    let decoded = decode_to_rgba(&raw)?;

    let frame = Frame {
        seq,
        raw,
        decoded: Some(decoded),
    };
    tx.send(frame)
        .await
        .map_err(|_| VisionError::Other(anyhow::anyhow!("downstream pipeline closed")))?;
    Ok(())
}

/// Decode encoded bytes into an RGBA8 buffer.
pub fn decode_to_rgba(raw: &VisionScreencastFrame) -> Result<DecodedImage, VisionError> {
    let format = match raw.format {
        FrameFormat::Png => image::ImageFormat::Png,
        FrameFormat::Jpeg => image::ImageFormat::Jpeg,
    };
    let img = image::load_from_memory_with_format(&raw.bytes, format)
        .map_err(|e| VisionError::Image(e.to_string()))?;
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: Arc::new(rgba.into_raw()),
        captured_us: raw.captured_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_mapping() {
        assert_eq!(fps_to_nth(30), 2);
        assert_eq!(fps_to_nth(15), 4);
        assert_eq!(fps_to_nth(5), 12);
        assert_eq!(fps_to_nth(60), 1);
        assert_eq!(fps_to_nth(0), 60);
        // Anything > 60 gets clamped to nth=1 (max FPS).
        assert_eq!(fps_to_nth(120), 1);
    }

    #[test]
    fn decode_rgba_round_trip() {
        // 2×1 PNG, two pixels: red and blue.
        let mut bytes: Vec<u8> = Vec::new();
        let enc = image::codecs::png::PngEncoder::new(&mut bytes);
        let pixels: Vec<u8> = vec![255, 0, 0, 255, 0, 0, 255, 255];
        use image::ImageEncoder;
        enc.write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
            .expect("encode");
        let raw = VisionScreencastFrame {
            bytes: Arc::new(bytes),
            format: FrameFormat::Png,
            metadata: ScreencastFrameMetadata {
                offset_top: 0.0,
                page_scale_factor: 1.0,
                device_width: 2.0,
                device_height: 1.0,
                scroll_offset_x: 0.0,
                scroll_offset_y: 0.0,
                timestamp: 0.0,
            },
            session_id: String::new(),
            captured_us: 0,
        };
        let img = decode_to_rgba(&raw).expect("decode");
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba.as_slice()[..4], [255, 0, 0, 255]);
        assert_eq!(img.rgba.as_slice()[4..8], [0, 0, 255, 255]);
    }
}
