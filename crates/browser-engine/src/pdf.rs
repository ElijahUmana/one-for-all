//! SPEC §12 U5 — Browser print + PDF surface.
//!
//! Two free async fns over [`crate::Page`]:
//!
//! - [`pdf`] — `Page.printToPDF` with the full DevTools options surface.
//!   Auto-switches to `transferMode: "ReturnAsStream"` when the inline
//!   base64 payload exceeds [`STREAM_THRESHOLD_BYTES`] or whenever the
//!   caller explicitly requests stream mode. Streamed payloads are
//!   drained to disk via the same `IO.read` loop perf.rs uses for
//!   tracing; small docs return inline base64.
//!
//! - [`print_preview`] — emulates `@media print` via
//!   `Emulation.setEmulatedMedia`, captures a screenshot, then ALWAYS
//!   restores the original media setting (even on screenshot error).
//!   Drop guards aren't usable because `Drop::drop` cannot `await`; the
//!   function uses an explicit match-with-restore-arm pattern.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context as _, Result};
use base64::Engine as _;
use cdp_client::generated::domains::{emulation as cdp_emulation, page as cdp_page};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;

use crate::page::Page;
use crate::perf::drain_io_stream;

/// SPEC §12 U5 — auto-stream threshold in encoded base64 bytes.
/// Anything bigger than this gets re-issued in stream mode to keep
/// CDP messages below the broker's per-message budget.
pub const STREAM_THRESHOLD_BYTES: usize = 8 * 1024 * 1024;

/// SPEC §12 U5 — full Page.printToPDF options surface. All fields are
/// optional; Chromium uses sensible defaults for any `None`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PdfOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landscape: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_header_footer: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paper_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paper_height: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_top: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_bottom: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_left: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_right: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_ranges: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_css_page_size: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_tagged_pdf: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_document_outline: Option<bool>,
    /// Force stream mode regardless of estimated size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PdfResult {
    Inline {
        format: String,
        data_base64: String,
        bytes: u64,
    },
    OnDisk {
        format: String,
        pdf_path: PathBuf,
        bytes: u64,
    },
}

/// SPEC §12 U5 — `Page.printToPDF` with the full options surface.
pub async fn pdf(page: &Page, options: PdfOptions, out_dir: &Path) -> Result<PdfResult> {
    pdf_inner(page, options, out_dir, false).await
}

async fn pdf_inner(
    page: &Page,
    options: PdfOptions,
    out_dir: &Path,
    is_retry_in_stream_mode: bool,
) -> Result<PdfResult> {
    let started = Instant::now();
    let force_stream = options.force_stream.unwrap_or(false) || is_retry_in_stream_mode;

    let params = cdp_page::PrintToPdfParams {
        landscape: options.landscape,
        display_header_footer: options.display_header_footer,
        print_background: options.print_background,
        scale: options.scale,
        paper_width: options.paper_width,
        paper_height: options.paper_height,
        margin_top: options.margin_top,
        margin_bottom: options.margin_bottom,
        margin_left: options.margin_left,
        margin_right: options.margin_right,
        page_ranges: options.page_ranges.clone(),
        header_template: options.header_template.clone(),
        footer_template: options.footer_template.clone(),
        prefer_css_page_size: options.prefer_css_page_size,
        transfer_mode: Some(
            if force_stream {
                "ReturnAsStream"
            } else {
                "ReturnAsBase64"
            }
            .to_owned(),
        ),
        generate_tagged_pdf: options.generate_tagged_pdf,
        generate_document_outline: options.generate_document_outline,
    };
    let res = page.cdp_send(params).await.context("Page.printToPDF")?;

    if let Some(handle) = res.stream {
        fs::create_dir_all(out_dir)
            .await
            .with_context(|| format!("create out_dir {}", out_dir.display()))?;
        let seq = monotonic_seq();
        let final_path = out_dir.join(format!("doc_{}.pdf", seq));
        let tmp_path = out_dir.join(format!("doc_{}.pdf.partial", seq));
        let bytes = drain_io_stream(page, handle, &tmp_path)
            .await
            .context("drain Page.printToPDF stream")?;
        fs::rename(&tmp_path, &final_path)
            .await
            .with_context(|| format!("rename {} → {}", tmp_path.display(), final_path.display()))?;
        observability::metrics::perf_metrics()
            .record_pdf(started.elapsed().as_millis() as u64, bytes);
        return Ok(PdfResult::OnDisk {
            format: "pdf".to_owned(),
            pdf_path: final_path,
            bytes,
        });
    }

    // Inline path. If the encoded payload looks too large AND we
    // haven't already retried, redo in stream mode.
    if !force_stream && res.data.len() >= STREAM_THRESHOLD_BYTES {
        let mut next = options;
        next.force_stream = Some(true);
        return Box::pin(pdf_inner(page, next, out_dir, true)).await;
    }

    // Decode just to compute the precise byte count for the caller.
    let decoded_bytes = base64::engine::general_purpose::STANDARD
        .decode(res.data.as_bytes())
        .map_err(|e| anyhow!("Page.printToPDF inline base64 decode: {e}"))?;
    let bytes = decoded_bytes.len() as u64;
    observability::metrics::perf_metrics().record_pdf(started.elapsed().as_millis() as u64, bytes);
    Ok(PdfResult::Inline {
        format: "pdf".to_owned(),
        data_base64: res.data,
        bytes,
    })
}

/// SPEC §12 U5 — `print_preview`. Captures a screenshot of the page
/// rendered as if `@media print` were active, then ALWAYS restores
/// the emulated media setting (even on screenshot error).
pub async fn print_preview(
    page: &Page,
    format: &str,
    capture_beyond_viewport: bool,
) -> Result<Value> {
    let started = Instant::now();
    page.cdp_send(cdp_emulation::SetEmulatedMediaParams {
        media: Some("print".to_owned()),
        features: None,
    })
    .await
    .context("Emulation.setEmulatedMedia(print)")?;

    let shot_res = page
        .cdp_send(cdp_page::CaptureScreenshotParams {
            format: Some(format.to_owned()),
            quality: None,
            capture_beyond_viewport: Some(capture_beyond_viewport),
            ..Default::default()
        })
        .await;

    // Always attempt to restore — even if the screenshot failed.
    let restore_res = page
        .cdp_send(cdp_emulation::SetEmulatedMediaParams {
            media: Some(String::new()),
            features: None,
        })
        .await;

    let shot = shot_res.context("Page.captureScreenshot (print preview)")?;
    if let Err(e) = restore_res {
        tracing::warn!(error = %e, "failed to restore emulated media after print preview");
    }

    observability::metrics::perf_metrics()
        .record_print_preview(started.elapsed().as_millis() as u64);

    Ok(serde_json::json!({
        "format": format,
        "data_base64": shot.data,
    }))
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
    fn pdf_options_default_serializes_empty() {
        let opts = PdfOptions::default();
        let v = serde_json::to_value(&opts).expect("serialize PdfOptions");
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn pdf_options_round_trip_full() {
        let json = serde_json::json!({
            "landscape": true,
            "display_header_footer": true,
            "print_background": false,
            "scale": 0.75,
            "paper_width": 8.5,
            "paper_height": 11.0,
            "margin_top": 0.4,
            "margin_bottom": 0.4,
            "margin_left": 0.4,
            "margin_right": 0.4,
            "page_ranges": "1-3,5",
            "header_template": "<span>hi</span>",
            "footer_template": "<span>bye</span>",
            "prefer_css_page_size": true,
            "generate_tagged_pdf": true,
            "generate_document_outline": false,
            "force_stream": false,
        });
        let opts: PdfOptions = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(opts.landscape, Some(true));
        assert_eq!(opts.scale, Some(0.75));
        assert_eq!(opts.page_ranges.as_deref(), Some("1-3,5"));
        assert_eq!(opts.force_stream, Some(false));
    }

    #[test]
    fn pdf_result_inline_serializes() {
        let r = PdfResult::Inline {
            format: "pdf".to_owned(),
            data_base64: "JVBERi0=".to_owned(),
            bytes: 4,
        };
        let v = serde_json::to_value(&r).expect("serialize PdfResult");
        assert_eq!(
            v.get("data_base64").and_then(Value::as_str),
            Some("JVBERi0=")
        );
    }

    #[test]
    fn stream_threshold_is_at_least_a_megabyte() {
        assert!(STREAM_THRESHOLD_BYTES >= 1_000_000);
    }
}
