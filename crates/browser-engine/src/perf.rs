//! SPEC §12 U4 — Browser performance + introspection surface.
//!
//! Free async fns over [`crate::Page`]. Each fn maps to one CDP domain
//! method (or a small choreography of them) and returns a JSON value
//! shaped for direct serialization through `page.*` router arms.
//!
//! ## Streaming choreography
//!
//! `Tracing.tracingComplete` and `HeapProfiler.addHeapSnapshotChunk` are
//! events, not command responses. Both fire DURING the corresponding
//! command's await window:
//!
//! - `HeapProfiler.takeHeapSnapshot` resolves only after Chromium has
//!   emitted every `addHeapSnapshotChunk` event for the snapshot. We
//!   subscribe to the per-session event broadcast BEFORE issuing the
//!   command, drain chunks straight to a temp file as they arrive, and
//!   stop draining when the command future completes.
//!
//! - `Tracing.start { transferMode: "ReturnAsStream" }` causes Chromium
//!   to emit one `Tracing.tracingComplete` event at flush time carrying
//!   an `IO.StreamHandle`. We wait for that event AFTER issuing
//!   `Tracing.end`, then drain the stream via `IO.read` until `eof`,
//!   then `IO.close`. Stream mode means the CDP broadcaster cannot
//!   overflow with `Tracing.dataCollected` events for a giant trace.
//!
//! All CDP method names are compile-time enforced through typed
//! [`cdp_client::Command`] impls. Errors propagate via `anyhow::Result`
//! with method-name `.context(...)` — matching the existing convention
//! in `network.rs`, `actions.rs`, and `page.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use base64::Engine as _;
use cdp_client::generated::domains::{
    css as cdp_css, heap_profiler as cdp_heap, io as cdp_io, overlay as cdp_overlay,
    page as cdp_page, performance as cdp_perf, profiler as cdp_profiler, tracing as cdp_tracing,
};
use cdp_client::generated::CdpEvent;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, warn};

use crate::page::Page;

/// Default tracing categories — DevTools' "Performance" preset, lightly
/// trimmed. Comma-separated, no whitespace.
pub const DEFAULT_TIMELINE_CATEGORIES: &str = concat!(
    "devtools.timeline,",
    "v8,",
    "v8.execute,",
    "blink,",
    "blink.user_timing,",
    "loading,",
    "navigation,",
    "rail,",
    "disabled-by-default-devtools.timeline,",
    "disabled-by-default-devtools.timeline.frame,",
    "disabled-by-default-devtools.timeline.stack,",
    "disabled-by-default-v8.cpu_profiler"
);

// =====================================================================
// performance_timeline_start / _stop
// =====================================================================

/// Result of a successful [`performance_timeline_stop`] call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineResult {
    pub trace_path: PathBuf,
    pub bytes: u64,
    pub data_loss: bool,
}

/// SPEC §12 U4 — start a tracing session.
///
/// Always uses `transferMode: "ReturnAsStream"` so trace bytes come
/// through `Tracing.tracingComplete`'s stream handle rather than via
/// `Tracing.dataCollected` broadcast events. This keeps the CDP event
/// channel from overflowing on multi-hundred-MiB traces.
pub async fn performance_timeline_start(page: &Page, categories: Option<&str>) -> Result<()> {
    let cats = categories
        .filter(|c| !c.is_empty())
        .unwrap_or(DEFAULT_TIMELINE_CATEGORIES);
    page.cdp_send(cdp_tracing::StartParams {
        categories: Some(cats.to_owned()),
        transfer_mode: Some("ReturnAsStream".to_owned()),
        stream_format: Some(Value::String("json".to_owned())),
        stream_compression: Some(Value::String("none".to_owned())),
        ..Default::default()
    })
    .await
    .context("Tracing.start")?;
    Ok(())
}

/// SPEC §12 U4 — stop tracing and drain the stream to disk.
pub async fn performance_timeline_stop(page: &Page, out_dir: &Path) -> Result<TimelineResult> {
    let started = Instant::now();
    let mut events = page.cdp_session().events();

    page.cdp_send(cdp_tracing::EndParams::default())
        .await
        .context("Tracing.end")?;

    let (handle, data_loss) = loop {
        let recv = tokio::time::timeout(Duration::from_secs(60), events.recv()).await;
        match recv {
            Ok(Ok(CdpEvent::TracingTracingComplete(c))) => {
                let h = c
                    .stream
                    .ok_or_else(|| anyhow!("Tracing.tracingComplete missing stream handle"))?;
                break (h, c.data_loss_occurred);
            }
            Ok(Ok(_)) => continue,
            Ok(Err(RecvError::Closed)) => {
                return Err(anyhow!("CDP event channel closed before tracingComplete"));
            }
            Ok(Err(RecvError::Lagged(n))) => {
                warn!(skipped = n, "tracing event broadcast lagged");
                continue;
            }
            Err(_) => return Err(anyhow!("Tracing.tracingComplete timed out")),
        }
    };

    fs::create_dir_all(out_dir)
        .await
        .with_context(|| format!("create out_dir {}", out_dir.display()))?;
    let seq = monotonic_seq();
    let final_path = out_dir.join(format!("trace_{}.json", seq));
    let tmp_path = out_dir.join(format!("trace_{}.json.partial", seq));
    let bytes = drain_io_stream(page, handle, &tmp_path)
        .await
        .context("drain Tracing.tracingComplete stream")?;
    fs::rename(&tmp_path, &final_path)
        .await
        .with_context(|| format!("rename {} → {}", tmp_path.display(), final_path.display()))?;

    observability::metrics::perf_metrics()
        .record_trace(started.elapsed().as_millis() as u64, bytes);

    Ok(TimelineResult {
        trace_path: final_path,
        bytes,
        data_loss,
    })
}

// =====================================================================
// performance_metrics
// =====================================================================

/// SPEC §12 U4 — `Performance.getMetrics`.
pub async fn performance_metrics(page: &Page) -> Result<Value> {
    page.cdp_send(cdp_perf::EnableParams::default())
        .await
        .context("Performance.enable")?;
    let res = page
        .cdp_send(cdp_perf::GetMetricsParams::default())
        .await
        .context("Performance.getMetrics")?;
    Ok(json!({ "metrics": res.metrics }))
}

// =====================================================================
// coverage_js_start / coverage_js_take
// =====================================================================

pub async fn coverage_js_start(
    page: &Page,
    call_count: Option<bool>,
    detailed: Option<bool>,
) -> Result<()> {
    page.cdp_send(cdp_profiler::EnableParams::default())
        .await
        .context("Profiler.enable")?;
    page.cdp_send(cdp_profiler::StartPreciseCoverageParams {
        call_count: Some(call_count.unwrap_or(true)),
        detailed: Some(detailed.unwrap_or(false)),
        allow_triggered_updates: Some(false),
    })
    .await
    .context("Profiler.startPreciseCoverage")?;
    Ok(())
}

pub async fn coverage_js_take(page: &Page) -> Result<Value> {
    let started = Instant::now();
    let res = page
        .cdp_send(cdp_profiler::TakePreciseCoverageParams::default())
        .await
        .context("Profiler.takePreciseCoverage")?;
    observability::metrics::perf_metrics()
        .record_coverage_take(started.elapsed().as_millis() as u64);
    Ok(json!({
        "result": res.result,
        "timestamp": res.timestamp,
    }))
}

// =====================================================================
// coverage_css_start / coverage_css_take
// =====================================================================

pub async fn coverage_css_start(page: &Page) -> Result<()> {
    page.cdp_send(cdp_css::EnableParams::default())
        .await
        .context("CSS.enable")?;
    page.cdp_send(cdp_css::StartRuleUsageTrackingParams::default())
        .await
        .context("CSS.startRuleUsageTracking")?;
    Ok(())
}

pub async fn coverage_css_take(page: &Page) -> Result<Value> {
    let started = Instant::now();
    let res = page
        .cdp_send(cdp_css::TakeCoverageDeltaParams::default())
        .await
        .context("CSS.takeCoverageDelta")?;
    observability::metrics::perf_metrics()
        .record_coverage_take(started.elapsed().as_millis() as u64);
    Ok(json!({
        "coverage": res.coverage,
        "timestamp": res.timestamp,
    }))
}

// =====================================================================
// heap_snapshot
// =====================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeapSnapshotResult {
    pub snapshot_path: PathBuf,
    pub bytes: u64,
}

/// SPEC §12 U4 — `HeapProfiler.takeHeapSnapshot` with streamed chunk
/// reassembly. Subscribes BEFORE issuing the command (events fire
/// during the command's await window), drains
/// `HeapProfiler.addHeapSnapshotChunk` to a tempfile, then renames to
/// `<out_dir>/heap_<seq>.heapsnapshot`.
pub async fn heap_snapshot(page: &Page, out_dir: &Path) -> Result<HeapSnapshotResult> {
    let started = Instant::now();
    fs::create_dir_all(out_dir)
        .await
        .with_context(|| format!("create out_dir {}", out_dir.display()))?;
    let seq = monotonic_seq();
    let tmp_path = out_dir.join(format!("heap_{}.heapsnapshot.partial", seq));
    let final_path = out_dir.join(format!("heap_{}.heapsnapshot", seq));

    page.cdp_send(cdp_heap::EnableParams::default())
        .await
        .context("HeapProfiler.enable")?;

    let mut events = page.cdp_session().events();

    let tmp_path_clone = tmp_path.clone();
    let drain = tokio::spawn(async move {
        let mut file = match fs::File::create(&tmp_path_clone).await {
            Ok(f) => f,
            Err(e) => return Err(anyhow!("create heap tmp file: {e}")),
        };
        let mut bytes_written: u64 = 0;
        loop {
            match events.recv().await {
                Ok(CdpEvent::HeapProfilerAddHeapSnapshotChunk(c)) => {
                    let buf = c.chunk.as_bytes();
                    if let Err(e) = file.write_all(buf).await {
                        return Err(anyhow!("write heap chunk: {e}"));
                    }
                    bytes_written += buf.len() as u64;
                }
                Ok(CdpEvent::HeapProfilerReportHeapSnapshotProgress(p)) => {
                    debug!(done = p.done, total = p.total, "heap snapshot progress");
                }
                Ok(_) => {}
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(n)) => {
                    warn!(skipped = n, "heap snapshot event broadcast lagged");
                }
            }
        }
        if let Err(e) = file.flush().await {
            return Err(anyhow!("flush heap snapshot file: {e}"));
        }
        Ok::<u64, anyhow::Error>(bytes_written)
    });

    let take_result = page
        .cdp_send(cdp_heap::TakeHeapSnapshotParams {
            report_progress: Some(true),
            treat_global_objects_as_roots: Some(true),
            capture_numeric_value: Some(true),
            expose_internals: Some(false),
        })
        .await
        .context("HeapProfiler.takeHeapSnapshot");

    // Grace window for any final chunks already buffered.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain.abort();

    let bytes_via_drain: Option<u64> = match drain.await {
        Ok(Ok(n)) => Some(n),
        Ok(Err(e)) => {
            observability::metrics::perf_metrics().record_failure();
            let _ = fs::remove_file(&tmp_path).await;
            take_result?;
            return Err(e);
        }
        Err(join_err) if join_err.is_cancelled() => None,
        Err(join_err) => {
            observability::metrics::perf_metrics().record_failure();
            let _ = fs::remove_file(&tmp_path).await;
            return Err(anyhow!("heap drain join error: {join_err}"));
        }
    };

    if let Err(e) = take_result {
        observability::metrics::perf_metrics().record_failure();
        let _ = fs::remove_file(&tmp_path).await;
        return Err(e);
    }

    let bytes = match bytes_via_drain {
        Some(n) => n,
        None => fs::metadata(&tmp_path)
            .await
            .with_context(|| format!("stat heap tmp {}", tmp_path.display()))?
            .len(),
    };

    fs::rename(&tmp_path, &final_path)
        .await
        .with_context(|| format!("rename {} → {}", tmp_path.display(), final_path.display()))?;

    observability::metrics::perf_metrics()
        .record_heap_snapshot(started.elapsed().as_millis() as u64, bytes);

    Ok(HeapSnapshotResult {
        snapshot_path: final_path,
        bytes,
    })
}

// =====================================================================
// heap_sample_alloc
// =====================================================================

pub async fn heap_sample_alloc(
    page: &Page,
    duration_ms: u64,
    sampling_interval_bytes: Option<u64>,
) -> Result<Value> {
    let started = Instant::now();
    page.cdp_send(cdp_heap::EnableParams::default())
        .await
        .context("HeapProfiler.enable")?;
    page.cdp_send(cdp_heap::StartSamplingParams {
        sampling_interval: sampling_interval_bytes.map(|v| v as f64),
        ..Default::default()
    })
    .await
    .context("HeapProfiler.startSampling")?;
    tokio::time::sleep(Duration::from_millis(duration_ms)).await;
    let res = page
        .cdp_send(cdp_heap::StopSamplingParams::default())
        .await
        .context("HeapProfiler.stopSampling")?;
    observability::metrics::perf_metrics().record_heap_sample(started.elapsed().as_millis() as u64);
    Ok(json!({ "profile": res.profile }))
}

// =====================================================================
// cpu_profile
// =====================================================================

pub async fn cpu_profile(page: &Page, duration_ms: u64) -> Result<Value> {
    let started = Instant::now();
    page.cdp_send(cdp_profiler::EnableParams::default())
        .await
        .context("Profiler.enable")?;
    page.cdp_send(cdp_profiler::StartParams::default())
        .await
        .context("Profiler.start")?;
    tokio::time::sleep(Duration::from_millis(duration_ms)).await;
    let res = page
        .cdp_send(cdp_profiler::StopParams::default())
        .await
        .context("Profiler.stop")?;
    observability::metrics::perf_metrics().record_cpu_profile(started.elapsed().as_millis() as u64);
    Ok(json!({ "profile": res.profile }))
}

// =====================================================================
// layout_metrics
// =====================================================================

pub async fn layout_metrics(page: &Page) -> Result<Value> {
    let res = page
        .cdp_send(cdp_page::GetLayoutMetricsParams::default())
        .await
        .context("Page.getLayoutMetrics")?;
    Ok(json!({
        "layout_viewport": res.layout_viewport,
        "visual_viewport": res.visual_viewport,
        "content_size": res.content_size,
        "css_layout_viewport": res.css_layout_viewport,
        "css_visual_viewport": res.css_visual_viewport,
        "css_content_size": res.css_content_size,
    }))
}

// =====================================================================
// paint_flash
// =====================================================================

pub async fn paint_flash(page: &Page, enable: bool) -> Result<()> {
    page.cdp_send(cdp_overlay::EnableParams::default())
        .await
        .context("Overlay.enable")?;
    page.cdp_send(cdp_overlay::SetShowPaintRectsParams { result: enable })
        .await
        .context("Overlay.setShowPaintRects")?;
    Ok(())
}

// =====================================================================
// Helpers
// =====================================================================

/// Drain an `IO.StreamHandle` to disk. Always calls `IO.close` at end,
/// even on read error. Base64-decodes chunks when the agent reports
/// `base64Encoded: true`.
pub(crate) async fn drain_io_stream(page: &Page, handle: Value, out_path: &Path) -> Result<u64> {
    let mut file = fs::File::create(out_path)
        .await
        .with_context(|| format!("create {}", out_path.display()))?;
    let mut total: u64 = 0;
    let result = drain_io_stream_into(page, handle.clone(), &mut file, &mut total).await;
    let _ = file.flush().await;
    let close_res = page
        .cdp_send(cdp_io::CloseParams { handle })
        .await
        .context("IO.close");
    match (result, close_res) {
        (Ok(()), _) => Ok(total),
        (Err(e), _) => Err(e),
    }
}

async fn drain_io_stream_into(
    page: &Page,
    handle: Value,
    file: &mut fs::File,
    total: &mut u64,
) -> Result<()> {
    use observability::caps::TRACING_IO_READ_CHUNK_BYTES;
    loop {
        let res = page
            .cdp_send(cdp_io::ReadParams {
                handle: handle.clone(),
                offset: None,
                size: Some(TRACING_IO_READ_CHUNK_BYTES),
            })
            .await
            .context("IO.read")?;
        let chunk = if res.base64_encoded.unwrap_or(false) {
            base64::engine::general_purpose::STANDARD
                .decode(res.data.as_bytes())
                .map_err(|e| anyhow!("IO.read base64 decode failed: {e}"))?
        } else {
            res.data.into_bytes()
        };
        if !chunk.is_empty() {
            file.write_all(&chunk).await.context("write IO chunk")?;
            *total += chunk.len() as u64;
        }
        if res.eof {
            return Ok(());
        }
    }
}

fn monotonic_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_seq_advances() {
        let a = monotonic_seq();
        let b = monotonic_seq();
        assert!(b > a);
    }

    #[test]
    fn default_categories_are_comma_separated() {
        assert!(DEFAULT_TIMELINE_CATEGORIES.contains("devtools.timeline"));
        assert!(DEFAULT_TIMELINE_CATEGORIES.contains("v8"));
        assert!(!DEFAULT_TIMELINE_CATEGORIES.contains(' '));
    }

    #[test]
    fn timeline_result_serializes() {
        let r = TimelineResult {
            trace_path: PathBuf::from("/tmp/trace.json"),
            bytes: 12345,
            data_loss: false,
        };
        let v = serde_json::to_value(&r).expect("serialize TimelineResult");
        assert_eq!(v.get("bytes").and_then(Value::as_u64), Some(12345));
        assert_eq!(v.get("data_loss").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn heap_snapshot_result_serializes() {
        let r = HeapSnapshotResult {
            snapshot_path: PathBuf::from("/tmp/heap.heapsnapshot"),
            bytes: 999,
        };
        let v = serde_json::to_value(&r).expect("serialize HeapSnapshotResult");
        assert_eq!(v.get("bytes").and_then(Value::as_u64), Some(999));
    }
}
