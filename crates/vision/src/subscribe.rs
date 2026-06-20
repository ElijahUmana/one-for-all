//! Pipeline subscriber. Drains the diff/OCR result stream and emits
//! `vision.frame` notifications to the bound sink. The broker may choose a
//! JSON `event/notify` envelope or the V5 binary fast path; frame payloads
//! carry the shm path + slot seq, NOT the bytes themselves.

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use crate::diff::{Bbox, TileChange};
use crate::frame_ring::FrameHandle;
use crate::metrics::Histograms;
use crate::ocr::TextRegion;
use crate::types::{Frame, ScreencastFrameMetadata};

/// Wire shape of a `vision.frame` notification (used in tests + diary).
#[derive(Debug, Clone, Serialize)]
pub struct VisionFrameEvent {
    pub topic: &'static str,
    pub session_id: String,
    pub tab_id: String,
    pub seq: u64,
    pub captured_us: u64,
    pub viewport: ScreencastFrameMetadata,
    pub frame: FrameHandle,
    pub changed_tiles: Vec<TileChange>,
    pub ocr_delta: Vec<TextRegion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stability: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::api::StabilityState>,
}

impl VisionFrameEvent {
    pub fn to_json_value(&self) -> serde_json::Value {
        let mut payload = json!({
            "topic": self.topic,
            "session_id": self.session_id,
            "tab_id": self.tab_id,
            "seq": self.seq,
            "captured_us": self.captured_us,
            "viewport": self.viewport,
            "frame": self.frame,
            "changed_tiles": self.changed_tiles,
            "ocr_delta": self.ocr_delta,
        });
        if let Some(s) = self.stability {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("stability".into(), json!(s));
            }
        }
        if let Some(state) = self.state {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "state".into(),
                    serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
                );
            }
        }
        payload
    }
}

/// Trait so the broker can plug its session writer into the vision pipeline
/// without `vision` depending on broker types.
pub trait NotificationSink: Send + Sync + 'static {
    fn push_vision_frame(&self, event: VisionFrameEvent);
}

/// Subscriber driver: consumes pipeline output and dispatches it through a
/// [`NotificationSink`].
pub struct VisionSubscriber {
    sink: Arc<dyn NotificationSink>,
    session_id: String,
    tab_id: String,
    metrics: Histograms,
}

impl VisionSubscriber {
    pub fn new(
        sink: Arc<dyn NotificationSink>,
        session_id: String,
        tab_id: String,
        metrics: Histograms,
    ) -> Self {
        Self {
            sink,
            session_id,
            tab_id,
            metrics,
        }
    }

    /// Build + dispatch one `vision.frame` event. Records the
    /// `pipeline_total_ms` histogram from the original frame timestamp to
    /// dispatch time so the SLO is enforced end-to-end.
    pub fn emit(
        &self,
        frame: &Frame,
        handle: FrameHandle,
        changed_tiles: Vec<TileChange>,
        ocr_delta: Vec<TextRegion>,
        stability: Option<f32>,
        stability_state: Option<crate::api::StabilityState>,
        pipeline_started: Instant,
    ) {
        let ev = VisionFrameEvent {
            topic: "vision.frame",
            session_id: self.session_id.clone(),
            tab_id: self.tab_id.clone(),
            seq: frame.seq,
            captured_us: frame.raw.captured_us,
            viewport: frame.raw.metadata.clone(),
            frame: handle,
            changed_tiles,
            ocr_delta,
            stability,
            state: stability_state,
        };
        self.sink.push_vision_frame(ev);
        let elapsed_ms = pipeline_started.elapsed().as_millis() as u64;
        self.metrics.pipeline_total_ms().record(elapsed_ms);
    }
}

/// Helper used by `vision.find_text` callers — checks if a `Bbox` is
/// in the requested region, defaulting to "any" when no region is given.
pub fn region_filter(rect: Bbox, filter: Option<Bbox>) -> bool {
    match filter {
        Some(f) => rect.intersects(&f),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    struct CaptureSink {
        seen: Arc<Mutex<Vec<VisionFrameEvent>>>,
    }
    impl NotificationSink for CaptureSink {
        fn push_vision_frame(&self, event: VisionFrameEvent) {
            self.seen.lock().push(event);
        }
    }

    fn fake_frame(seq: u64) -> Frame {
        use crate::types::{FrameFormat, ScreencastFrame};
        Frame {
            seq,
            raw: ScreencastFrame {
                bytes: Arc::new(vec![0; 4]),
                format: FrameFormat::Jpeg,
                metadata: ScreencastFrameMetadata {
                    offset_top: 0.0,
                    page_scale_factor: 1.0,
                    device_width: 1.0,
                    device_height: 1.0,
                    scroll_offset_x: 0.0,
                    scroll_offset_y: 0.0,
                    timestamp: 0.0,
                },
                session_id: String::from("s"),
                captured_us: 42,
            },
            decoded: None,
        }
    }

    #[test]
    fn emits_event_notify_with_shm_handle() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(CaptureSink {
            seen: Arc::clone(&seen),
        });
        let sub = VisionSubscriber::new(sink, "sess-1".into(), "tab-1".into(), Histograms::new());
        let handle = FrameHandle {
            shm_path: std::path::PathBuf::from("/tmp/ofa-frames-x"),
            slot_seq: 7,
            slot_index: 3,
            offset: 1024,
            len: 256,
            ts_us: 42,
        };
        sub.emit(
            &fake_frame(1),
            handle,
            vec![],
            vec![],
            None,
            None,
            Instant::now(),
        );
        let got = seen.lock();
        assert_eq!(got.len(), 1);
        let v = got[0].to_json_value();
        assert_eq!(v["topic"], "vision.frame");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["tab_id"], "tab-1");
        // The handle is embedded — frame bytes are NOT inlined.
        assert_eq!(v["frame"]["slot_seq"], 7);
        assert_eq!(v["frame"]["len"], 256);
        assert!(v.get("data").is_none(), "frame bytes must not be inlined");
    }
}
