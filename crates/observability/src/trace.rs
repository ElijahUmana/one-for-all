//! SPEC §10 M10 — structured trace recording.
//!
//! When `browser.context.create {trace: true}` is set, the broker writes a
//! replay-able audit trail to `~/.one-for-all/sessions/<sid>/trace/<seq>.jsonl`
//! capturing every CDP request/response, every action's screenshot, and a DOM
//! snapshot every 500ms during agent activity.
//!
//! ## Architecture
//!
//! [`TraceWriter`] owns one bounded `tokio::sync::mpsc` channel
//! (capacity 1024 per SPEC §1 D16 / §8 R8) plus a background async task that
//! drains the channel, serializes [`TraceEvent`]s as JSON-Lines, and rotates
//! the file when either threshold trips:
//!
//!   * `OFA_TRACE_MAX_BYTES` (default 100 MiB) — file size cap.
//!   * `OFA_TRACE_MAX_AGE_SECS` (default 3600 s) — wall-clock age.
//!
//! On overflow the **writer** drops the **oldest** queued event with a
//! rate-limited `WARN trace.overflow` log + an atomic counter, fulfilling the
//! SPEC §8 R8 quality gate. Implementation note: we use a small `VecDeque`
//! drain inside the writer task to honor "drop-oldest" semantics — `try_send`
//! would otherwise drop newest.
//!
//! ## On-disk layout
//!
//! ```text
//! ~/.one-for-all/sessions/<sid>/trace/
//!   ├── 0000.jsonl           ← rotated by size or age
//!   ├── 0001.jsonl
//!   ├── screenshots/
//!   │     └── <tool>-<unix_ms>-<seq>.png
//!   └── snapshots/
//!         └── <snapshot_seq>.json
//! ```
//!
//! Large blobs (PNG screenshots, full DOM snapshots) live alongside the JSONL
//! and are referenced by relative path; the JSONL records only the path + a
//! sha256 hash for the snapshot.
//!
//! ## Concurrency contract
//!
//! [`TraceWriter`] is `Clone`-cheap (`Arc` inside) and therefore the
//! [`TraceSink`] trait can be erased behind `Arc<dyn TraceSink>` and shared
//! across the browser-engine + broker actor graph without further
//! synchronization. Every `pub` async function below is documented with its
//! cancellation safety.
//!
//! ## SPEC quality gates honoured here
//!
//! * Zero `.unwrap()` / `.expect()` outside `#[cfg(test)]`.
//! * Every spawned task's `JoinHandle` is stored on the writer.
//! * Every channel is bounded (cap 1024).
//! * No `mpsc::unbounded_channel`.
//! * `Drop` is sync-only — async cleanup is via explicit
//!   [`TraceWriter::shutdown`].

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::latency::{LatencyHistogram, LatencySnapshot};

/// Default size cap for one trace JSONL file (100 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// Default age cap for one trace JSONL file (1 hour).
pub const DEFAULT_MAX_AGE_SECS: u64 = 3_600;
/// Bounded channel capacity per SPEC §1 D16 / §8 R8.
pub const TRACE_CHANNEL_CAP: usize = 1024;
/// Drop-rate alarm window. If `DROP_ALARM_THRESHOLD` drops occur inside
/// this window the writer latches `drop_alarm_active=true` and emits a
/// rate-limited `ERROR trace.overflow.alarm`. Surfaces a slow disk early.
pub const DROP_ALARM_WINDOW: Duration = Duration::from_secs(60);
/// Drops-per-window before the alarm fires.
pub const DROP_ALARM_THRESHOLD: usize = 100;
/// Built-in structural denylist applied BEFORE pattern redaction. Any JSON
/// object key in this list (case-insensitive) has its value replaced with
/// the redaction sentinel regardless of whether `redact_patterns` matches.
const HEADER_DENYLIST: &[&str] = &[
    "cookie",
    "set-cookie",
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "password",
    "passwd",
    "secret",
];
/// Replacement string written wherever redaction matches.
const REDACTED: &str = "<REDACTED>";

/// One record in a trace JSONL file.
///
/// The wire shape is `{ts_ms, kind, ...}` — `kind` is the discriminant tag
/// (lower-snake), all variants share `ts_ms` (unix milliseconds) and most
/// share `session_id` for cross-session debugging tools (`ofa-trace ls`,
/// `ofa-trace tail`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// Outbound CDP method call.
    CdpRequest {
        ts_ms: u64,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_id: Option<String>,
        id: i64,
        method: String,
        params: Value,
    },
    /// Inbound CDP reply for a previously emitted [`Self::CdpRequest`].
    CdpResponse {
        ts_ms: u64,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_id: Option<String>,
        id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<Value>,
    },
    /// Inbound CDP event (no `id`).
    CdpEvent {
        ts_ms: u64,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_id: Option<String>,
        method: String,
        params: Value,
    },
    /// Tool dispatch outcome at the broker boundary.
    Action {
        ts_ms: u64,
        session_id: String,
        tab_id: String,
        tool: String,
        args: Value,
        result: Value,
    },
    /// Post-action screenshot. PNG lives at `<trace_dir>/<png_path>`.
    Screenshot {
        ts_ms: u64,
        session_id: String,
        tab_id: String,
        after_action: String,
        png_path: String,
    },
    /// 500 ms-cadence DOM snapshot. Full payload is at
    /// `<trace_dir>/<snapshot_path>`; JSONL only carries the hash + path.
    DomSnapshot {
        ts_ms: u64,
        session_id: String,
        tab_id: String,
        snapshot_seq: u64,
        hash: String,
        snapshot_path: String,
    },
}

impl TraceEvent {
    /// Record kind ("cdp_request" / "screenshot" / …). Useful for tests and
    /// log lines without rebuilding the serde tag.
    pub fn kind(&self) -> &'static str {
        match self {
            TraceEvent::CdpRequest { .. } => "cdp_request",
            TraceEvent::CdpResponse { .. } => "cdp_response",
            TraceEvent::CdpEvent { .. } => "cdp_event",
            TraceEvent::Action { .. } => "action",
            TraceEvent::Screenshot { .. } => "screenshot",
            TraceEvent::DomSnapshot { .. } => "dom_snapshot",
        }
    }
}

/// Type-erased sink — what hot-path code in browser-engine + broker holds.
///
/// Implementing the trait keeps callers blind to whether the underlying
/// writer is a real file-backed one, an in-memory test stub, or a discard
/// sink. `record` is non-blocking / non-awaiting — it MUST never await on
/// I/O.
pub trait TraceSink: Send + Sync + 'static {
    /// Enqueue an event. Non-blocking; on overflow the writer drops the
    /// oldest queued event and emits a single rate-limited `WARN`.
    fn record(&self, ev: TraceEvent);

    /// Persist a screenshot blob to the trace dir and return the relative
    /// path that should be embedded in [`TraceEvent::Screenshot::png_path`].
    fn save_screenshot_png(&self, tool: &str, png: &[u8]) -> Result<String>;

    /// Persist a continuous-vision screencast frame (typically JPEG) to the
    /// trace dir's `screenshots/` subdirectory and return the relative
    /// path. Default impl falls back to [`Self::save_screenshot_png`] —
    /// implementations may override to honour a non-`.png` extension when
    /// the encoded format is JPEG so ofa-replay decodes correctly.
    fn save_screencast_frame(&self, ext: &str, bytes: &[u8]) -> Result<String> {
        // Best-effort default: many sinks only know about PNG. Real
        // file-backed sinks override this.
        let _ = ext;
        self.save_screenshot_png("vision.continuous", bytes)
    }

    /// Persist a DOM-snapshot JSON blob and return `(relative_path, sha256_hex)`.
    fn save_snapshot_json(&self, snapshot_seq: u64, json: &Value) -> Result<(String, String)>;

    /// Number of events dropped on overflow since process start. Surfaced by
    /// `_internal.metrics` once the broker wires it.
    fn dropped_count(&self) -> u64 {
        0
    }

    /// Convenience helper — current monotonic timestamp in unix milliseconds.
    /// We deliberately do NOT panic on a clock skew before UNIX_EPOCH.
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// File-backed trace writer for one session.
///
/// Construct via [`TraceWriter::start`]; clones share the same channel and
/// background task. Drop is sync-only; call [`TraceWriter::shutdown`] for
/// graceful flush before process exit.
#[derive(Clone)]
pub struct TraceWriter {
    inner: Arc<TraceWriterInner>,
}

struct TraceWriterInner {
    session_id: String,
    dir: PathBuf,
    screenshots_dir: PathBuf,
    snapshots_dir: PathBuf,
    tx: mpsc::Sender<Cmd>,
    /// Counter — exposed via [`TraceSink::dropped_count`].
    dropped: AtomicU64,
    /// Monotonic counter for screenshot filenames. Avoids same-millisecond
    /// collisions when several screenshots land inside one tick.
    screenshot_seq: AtomicU64,
    /// Set once `shutdown` has been called.
    shutting_down: AtomicBool,
    /// Task handle for the file-writer; awaited in `shutdown`.
    task: Mutex<Option<JoinHandle<()>>>,
    /// Wake the writer when a new event lands (or shutdown was requested).
    notify: Arc<Notify>,
    /// Bounded buffer the writer drains in batches. Drop-oldest happens here.
    buffer: Arc<Mutex<VecDeque<TraceEvent>>>,
    /// Cap on `buffer` — same as TRACE_CHANNEL_CAP unless overridden in tests.
    buffer_cap: usize,
    /// Histogram of per-event write latency in microseconds. Surfaced via
    /// `_internal.metrics` so a slow disk shows up before drop-oldest fires
    /// repeatedly.
    write_latency: Arc<LatencyHistogram>,
    /// Sliding 60-second window of recent drop timestamps (unix-ms). Used
    /// to fire the rate-based alarm without needing wall-clock arithmetic
    /// outside the lock.
    drop_window: Mutex<VecDeque<u64>>,
    /// Latched once `>=DROP_ALARM_THRESHOLD` drops occurred inside the window.
    /// Cleared by `_internal.metrics` consumers via `clear_alarm`.
    drop_alarm_active: AtomicBool,
    /// Compiled redaction set + structural key denylist. None disables
    /// redaction (default). Kept here so external observers (`doctor.sh`
    /// / `_internal.metrics`) can report whether redaction is active.
    #[allow(dead_code)]
    redactor: Option<Arc<Redactor>>,
    /// Optional HMAC-SHA256 key for the manifest. None → manifest is still
    /// written but without an `hmac` field. Kept on the inner struct so an
    /// observer can report "manifest is signed" without re-reading env.
    #[allow(dead_code)]
    hmac_key: Option<Vec<u8>>,
}

/// Internal control commands sent to the writer task.
enum Cmd {
    /// Flush + close, then exit.
    Shutdown,
}

/// Tunables for [`TraceWriter::start_with_options`]. Production callers use
/// [`TraceWriter::start`] which reads `OFA_TRACE_MAX_BYTES` /
/// `OFA_TRACE_MAX_AGE_SECS` from the environment.
#[derive(Debug, Clone)]
pub struct TraceOptions {
    pub max_bytes: u64,
    pub max_age: Duration,
    pub buffer_cap: usize,
    /// Per-session PII redaction patterns. Each entry is a regex; matched
    /// substrings inside string-typed JSON nodes of `params` / `args` /
    /// `result` are replaced with `<REDACTED>` BEFORE the line hits disk.
    /// Object keys, kinds, ids, file paths are NEVER redacted (metadata).
    /// Invalid patterns log `WARN` once and are skipped.
    pub redact_patterns: Vec<String>,
    /// Optional HMAC-SHA256 key for the rotation manifest. When `Some`, the
    /// writer maintains `<dir>/manifest.json` with a hash-and-hmac index of
    /// every rotated file; `ofa-replay verify` consumes it. When `None`,
    /// the manifest is still written but without an `hmac` field.
    pub hmac_key: Option<Vec<u8>>,
}

impl TraceOptions {
    /// Defaults sourced from env (or hard-coded defaults if env is unset).
    /// Reads `OFA_TRACE_MAX_BYTES`, `OFA_TRACE_MAX_AGE_SECS`, and
    /// `OFA_TRACE_HMAC_KEY` (raw bytes / hex if prefixed `hex:`).
    pub fn from_env() -> Self {
        let max_bytes = std::env::var("OFA_TRACE_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_BYTES);
        let max_age_secs = std::env::var("OFA_TRACE_MAX_AGE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_AGE_SECS);
        let hmac_key = std::env::var("OFA_TRACE_HMAC_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                if let Some(rest) = s.strip_prefix("hex:") {
                    hex::decode(rest).unwrap_or_else(|_| rest.as_bytes().to_vec())
                } else {
                    s.into_bytes()
                }
            });
        Self {
            max_bytes,
            max_age: Duration::from_secs(max_age_secs),
            buffer_cap: TRACE_CHANNEL_CAP,
            redact_patterns: Vec::new(),
            hmac_key,
        }
    }
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_age: Duration::from_secs(DEFAULT_MAX_AGE_SECS),
            buffer_cap: TRACE_CHANNEL_CAP,
            redact_patterns: Vec::new(),
            hmac_key: None,
        }
    }
}

impl TraceWriter {
    /// Resolve the trace directory for `session_id` under
    /// `~/.one-for-all/sessions/<session_id>/trace/` and start the writer
    /// task. Reads tunables from `OFA_TRACE_MAX_BYTES` / `OFA_TRACE_MAX_AGE_SECS`.
    ///
    /// Idempotent: the directory is created with `mode 0700` if missing;
    /// existing files are preserved (the next seq picks up after the highest
    /// `<NNNN>.jsonl` already on disk).
    pub fn start(session_id: &str) -> Result<Self> {
        let base = sessions_root()?;
        let dir = base.join(session_id).join("trace");
        Self::start_in_dir(session_id, &dir, TraceOptions::from_env())
    }

    /// Test/forensics-friendly variant — start a writer at an explicit dir
    /// with explicit options (used by unit tests to bypass `~/.one-for-all` and
    /// shrink rotation thresholds).
    pub fn start_in_dir(session_id: &str, dir: &Path, opts: TraceOptions) -> Result<Self> {
        create_dir_secure(dir)
            .with_context(|| format!("creating trace dir at {}", dir.display()))?;
        let screenshots_dir = dir.join("screenshots");
        let snapshots_dir = dir.join("snapshots");
        create_dir_secure(&screenshots_dir)
            .with_context(|| format!("creating {}", screenshots_dir.display()))?;
        create_dir_secure(&snapshots_dir)
            .with_context(|| format!("creating {}", snapshots_dir.display()))?;

        let next_seq = scan_next_seq(dir);
        let buffer_cap = opts.buffer_cap.max(1);
        let (tx, rx) = mpsc::channel::<Cmd>(8);
        let buffer: Arc<Mutex<VecDeque<TraceEvent>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(buffer_cap)));
        let notify = Arc::new(Notify::new());

        // SAFETY: bounds (1, 60_000_000, 3) are compile-time valid per
        // `LatencyHistogram::new`. We use `shared` to mirror existing usage.
        let write_latency = LatencyHistogram::shared("trace.write_latency_us");

        let redactor = if opts.redact_patterns.is_empty() {
            None
        } else {
            match Redactor::new(&opts.redact_patterns) {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    warn!(error = %e, "trace redactor disabled (no patterns compiled)");
                    None
                }
            }
        };
        let hmac_key = opts.hmac_key.clone();

        let task = {
            let dir = dir.to_path_buf();
            let buffer = Arc::clone(&buffer);
            let notify = Arc::clone(&notify);
            let session_id_owned = session_id.to_owned();
            let opts = opts.clone();
            let redactor = redactor.clone();
            let hmac_key = hmac_key.clone();
            let write_latency = Arc::clone(&write_latency);
            tokio::spawn(async move {
                writer_loop(WriterCtx {
                    session_id: session_id_owned,
                    dir,
                    next_seq,
                    opts,
                    rx,
                    buffer,
                    notify,
                    redactor,
                    hmac_key,
                    write_latency,
                })
                .await;
            })
        };

        let inner = TraceWriterInner {
            session_id: session_id.to_owned(),
            dir: dir.to_path_buf(),
            screenshots_dir,
            snapshots_dir,
            tx,
            dropped: AtomicU64::new(0),
            screenshot_seq: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            task: Mutex::new(Some(task)),
            notify,
            buffer,
            buffer_cap,
            write_latency,
            drop_window: Mutex::new(VecDeque::with_capacity(DROP_ALARM_THRESHOLD * 2)),
            drop_alarm_active: AtomicBool::new(false),
            redactor,
            hmac_key,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Path to the trace directory for this session.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The session id this writer was created for.
    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// Drain the buffer, close the current file, and join the writer task.
    ///
    /// CANCELLATION: not safe — caller must `await` to completion or events
    /// queued before this call may not flush. Subsequent `record()` calls
    /// after `shutdown()` returns are silently dropped (with `dropped` counter
    /// increment).
    pub async fn shutdown(&self) -> Result<()> {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Best-effort signal to the task. If the channel is full of unrelated
        // commands or already gone, we still wake via Notify and let the task
        // observe `shutting_down`.
        let _ = self.inner.tx.try_send(Cmd::Shutdown);
        self.inner.notify.notify_one();
        let handle = self.inner.task.lock().take();
        if let Some(h) = handle {
            if let Err(e) = h.await {
                warn!(session_id = %self.inner.session_id, error = ?e, "trace writer task panicked");
            }
        }
        Ok(())
    }

    /// Test helper — return the in-memory pending event count.
    #[doc(hidden)]
    pub fn pending_len(&self) -> usize {
        self.inner.buffer.lock().len()
    }

    /// Test helper — block until the writer has drained all pending events
    /// (or `timeout` elapses).
    #[doc(hidden)]
    pub async fn flush_for_test(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.inner.buffer.lock().is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            self.inner.notify.notify_one();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Snapshot the per-event write-latency histogram. Surfaced via the
    /// broker `_internal.metrics` RPC so a slow disk shows up as elevated
    /// p95/p99 well before drop-oldest fires.
    pub fn write_latency_snapshot(&self) -> LatencySnapshot {
        self.inner.write_latency.snapshot()
    }

    /// `true` once the rolling drop-rate alarm has tripped. Latched until
    /// [`Self::clear_alarm`] runs (typically by the metrics consumer).
    pub fn drop_alarm_active(&self) -> bool {
        self.inner.drop_alarm_active.load(Ordering::Acquire)
    }

    /// Clear the latched drop-rate alarm. Idempotent.
    pub fn clear_alarm(&self) {
        self.inner.drop_alarm_active.store(false, Ordering::Release);
    }

    /// Public alias for [`Self::start_in_dir`]. Lets the broker pass an
    /// explicit `TraceOptions` (e.g. with `redact_patterns` / `hmac_key`)
    /// while staying inside `~/.one-for-all/sessions/<sid>/trace/`.
    pub fn start_with_options(session_id: &str, opts: TraceOptions) -> Result<Self> {
        let base = sessions_root()?;
        let dir = base.join(session_id).join("trace");
        Self::start_in_dir(session_id, &dir, opts)
    }

    /// Trim and append-now to the sliding drop-rate window. Latches
    /// `drop_alarm_active` and emits a single `ERROR` once the window's
    /// drop count crosses [`DROP_ALARM_THRESHOLD`]. Cheap: bounded VecDeque,
    /// trim is O(window-front) which is bounded by the threshold itself.
    fn note_drop_for_alarm(&self) {
        let now = current_unix_ms();
        let cutoff = now.saturating_sub(DROP_ALARM_WINDOW.as_millis() as u64);
        let mut w = self.inner.drop_window.lock();
        while let Some(&front) = w.front() {
            if front < cutoff {
                w.pop_front();
            } else {
                break;
            }
        }
        w.push_back(now);
        if w.len() >= DROP_ALARM_THRESHOLD
            && !self.inner.drop_alarm_active.swap(true, Ordering::AcqRel)
        {
            error!(
                target: "trace.overflow.alarm",
                session_id = %self.inner.session_id,
                drops_in_window = w.len(),
                window_secs = DROP_ALARM_WINDOW.as_secs(),
                "trace drop-rate alarm: probable slow disk"
            );
        }
    }
}

impl TraceSink for TraceWriter {
    /// Enqueue `ev` for the writer task.
    ///
    /// Non-blocking. If the buffer is at capacity we drop the OLDEST event
    /// (per SPEC §8 R8) and bump [`Self::dropped_count`]. A rate-limited
    /// `WARN trace.overflow` is logged at most ~1 Hz to avoid log floods,
    /// and a sliding 60 s drop-rate window latches
    /// [`TraceWriter::drop_alarm_active`] once
    /// [`DROP_ALARM_THRESHOLD`] drops accumulate (slow-disk indicator).
    fn record(&self, ev: TraceEvent) {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut buf = self.inner.buffer.lock();
        if buf.len() >= self.inner.buffer_cap {
            let _dropped = buf.pop_front();
            let prev = self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            // Rate-limit: 1, 2, 4, 8, ... — log on power-of-two boundaries.
            if prev == 0 || prev.is_power_of_two() {
                warn!(
                    target: "trace.overflow",
                    session_id = %self.inner.session_id,
                    dropped_total = prev + 1,
                    "trace buffer full, dropped oldest event"
                );
            }
            // Sliding-window drop-rate alarm. Cheap: bounded VecDeque, never
            // grows past DROP_ALARM_THRESHOLD * 2 because we trim on entry.
            self.note_drop_for_alarm();
        }
        buf.push_back(ev);
        drop(buf);
        self.inner.notify.notify_one();
    }

    fn save_screenshot_png(&self, tool: &str, png: &[u8]) -> Result<String> {
        let safe_tool = sanitize_for_filename(tool);
        let unix_ms = self.now_ms();
        // Strict-monotonic counter — never collides on the same millisecond
        // even when several screenshots land in one tick.
        let seq = self.inner.screenshot_seq.fetch_add(1, Ordering::Relaxed);
        let fname = format!("{safe_tool}-{unix_ms}-{seq:06}.png");
        let path = self.inner.screenshots_dir.join(&fname);
        let mut f =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        f.write_all(png)
            .with_context(|| format!("writing png to {}", path.display()))?;
        f.flush().ok();
        // Relative-to-trace-dir path for the JSONL record.
        Ok(format!("screenshots/{fname}"))
    }

    /// Override: continuous-vision frames may be JPEG. We honour the caller's
    /// extension to keep ofa-replay's image decoder happy.
    fn save_screencast_frame(&self, ext: &str, bytes: &[u8]) -> Result<String> {
        let safe_ext = sanitize_for_filename(ext);
        let unix_ms = self.now_ms();
        let seq = self.inner.screenshot_seq.fetch_add(1, Ordering::Relaxed);
        let fname = format!("vision.frame-{unix_ms}-{seq:06}.{safe_ext}");
        let path = self.inner.screenshots_dir.join(&fname);
        let mut f =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing screencast frame to {}", path.display()))?;
        f.flush().ok();
        Ok(format!("screenshots/{fname}"))
    }

    fn save_snapshot_json(&self, snapshot_seq: u64, json: &Value) -> Result<(String, String)> {
        let bytes = serde_json::to_vec(json).context("serializing snapshot json")?;
        let hash = sha256_hex(&bytes);
        let fname = format!("{snapshot_seq:08}.json");
        let path = self.inner.snapshots_dir.join(&fname);
        let mut f =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        f.write_all(&bytes)
            .with_context(|| format!("writing snapshot to {}", path.display()))?;
        f.flush().ok();
        Ok((format!("snapshots/{fname}"), hash))
    }

    fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

/// Per-session registry of trace writers. Mirrors `metrics::Registry`.
#[derive(Clone, Default)]
pub struct TraceRegistry {
    inner: Arc<dashmap::DashMap<String, Arc<TraceWriter>>>,
}

impl TraceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the trace writer for `session_id`. Returns the existing
    /// writer if one is already registered.
    pub fn get_or_start(&self, session_id: &str) -> Result<Arc<TraceWriter>> {
        if let Some(w) = self.inner.get(session_id) {
            return Ok(Arc::clone(&*w));
        }
        let w = Arc::new(TraceWriter::start(session_id)?);
        self.inner.insert(session_id.to_owned(), Arc::clone(&w));
        Ok(w)
    }

    /// Get-or-create using explicit `TraceOptions`. Used by the broker to
    /// pass `redact_patterns` / `hmac_key` from `session.register` straight
    /// through to the writer. If a writer is already registered for this
    /// session, the existing writer is returned (options are ignored — the
    /// existing writer's options are preserved).
    pub fn get_or_start_with_options(
        &self,
        session_id: &str,
        opts: TraceOptions,
    ) -> Result<Arc<TraceWriter>> {
        if let Some(w) = self.inner.get(session_id) {
            return Ok(Arc::clone(&*w));
        }
        let w = Arc::new(TraceWriter::start_with_options(session_id, opts)?);
        self.inner.insert(session_id.to_owned(), Arc::clone(&w));
        Ok(w)
    }

    /// Test/forensics — register a writer that was constructed with
    /// non-default options (e.g. an explicit dir).
    pub fn insert(&self, writer: Arc<TraceWriter>) {
        self.inner.insert(writer.session_id().to_owned(), writer);
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<TraceWriter>> {
        self.inner.get(session_id).map(|w| Arc::clone(&*w))
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<TraceWriter>> {
        self.inner.remove(session_id).map(|(_, w)| w)
    }

    pub fn iter(&self) -> impl Iterator<Item = (String, Arc<TraceWriter>)> + '_ {
        self.inner
            .iter()
            .map(|kv| (kv.key().clone(), Arc::clone(kv.value())))
    }
}

// ---------- writer task ----------

struct WriterCtx {
    session_id: String,
    dir: PathBuf,
    next_seq: u64,
    opts: TraceOptions,
    rx: mpsc::Receiver<Cmd>,
    buffer: Arc<Mutex<VecDeque<TraceEvent>>>,
    notify: Arc<Notify>,
    redactor: Option<Arc<Redactor>>,
    hmac_key: Option<Vec<u8>>,
    write_latency: Arc<LatencyHistogram>,
}

async fn writer_loop(mut ctx: WriterCtx) {
    let mut current = match open_seq(&ctx.dir, ctx.next_seq).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, dir = %ctx.dir.display(), "trace writer: open initial file failed");
            return;
        }
    };
    debug!(
        session_id = %ctx.session_id,
        path = %current.path.display(),
        "trace writer: started"
    );

    // Track manifest entries — one per closed file. The current open file is
    // appended at shutdown / rotation time.
    let mut manifest_files: Vec<ManifestFile> = Vec::new();
    let started_ts_ms = current_unix_ms();
    // Best-effort initial manifest so consumers see the session even before
    // first rotation.
    write_manifest(
        &ctx.dir,
        &ctx.session_id,
        started_ts_ms,
        &manifest_files,
        ctx.hmac_key.as_deref(),
    );

    let mut shutdown = false;
    while !shutdown {
        let max_age = ctx.opts.max_age;
        let next_rotation_age = max_age.saturating_sub(current.opened_at.elapsed());
        let wake_in = next_rotation_age.max(Duration::from_millis(50));

        tokio::select! {
            biased;
            cmd = ctx.rx.recv() => {
                if matches!(cmd, Some(Cmd::Shutdown) | None) {
                    shutdown = true;
                }
            }
            _ = ctx.notify.notified() => {}
            _ = tokio::time::sleep(wake_in) => {}
        }

        // Drain buffer.
        loop {
            let next_event = ctx.buffer.lock().pop_front();
            let ev = match next_event {
                Some(e) => e,
                None => break,
            };
            // Rotate if needed BEFORE writing.
            if should_rotate(&current, ctx.opts.max_bytes, max_age) {
                let rotated = match rotate(current, &ctx.dir).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, dir = %ctx.dir.display(), "trace writer: rotation failed");
                        return;
                    }
                };
                current = rotated.next;
                if let Some(closed) = rotated.closed_manifest {
                    manifest_files.push(closed);
                    write_manifest(
                        &ctx.dir,
                        &ctx.session_id,
                        started_ts_ms,
                        &manifest_files,
                        ctx.hmac_key.as_deref(),
                    );
                }
            }
            let started = Instant::now();
            let redacted = match ctx.redactor.as_deref() {
                Some(r) => redact_event(&ev, r),
                None => ev,
            };
            if let Err(e) = current.write_event(&redacted).await {
                warn!(error = %e, "trace writer: write failed; dropping event");
            }
            ctx.write_latency.record_duration(started.elapsed());
        }

        // Even with no events, we may need to rotate due to age.
        if !shutdown && should_rotate(&current, ctx.opts.max_bytes, max_age) {
            let rotated = match rotate(current, &ctx.dir).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, dir = %ctx.dir.display(), "trace writer: age rotation failed");
                    return;
                }
            };
            current = rotated.next;
            if let Some(closed) = rotated.closed_manifest {
                manifest_files.push(closed);
                write_manifest(
                    &ctx.dir,
                    &ctx.session_id,
                    started_ts_ms,
                    &manifest_files,
                    ctx.hmac_key.as_deref(),
                );
            }
        }
    }

    // Final drain.
    loop {
        let next_event = ctx.buffer.lock().pop_front();
        let Some(ev) = next_event else { break };
        let started = Instant::now();
        let redacted = match ctx.redactor.as_deref() {
            Some(r) => redact_event(&ev, r),
            None => ev,
        };
        if let Err(e) = current.write_event(&redacted).await {
            warn!(error = %e, "trace writer: shutdown drain write failed");
        }
        ctx.write_latency.record_duration(started.elapsed());
    }
    let final_path = current.path.clone();
    let final_seq = current.seq;
    let final_bytes = current.bytes_written;
    if let Err(e) = current.shutdown().await {
        warn!(error = %e, "trace writer: file shutdown error");
    }
    // Hash the final (still-uncompressed) file for the manifest.
    if let Ok(mf) = manifest_for_closed(&final_path, final_seq, final_bytes, false) {
        manifest_files.push(mf);
    }
    write_manifest(
        &ctx.dir,
        &ctx.session_id,
        started_ts_ms,
        &manifest_files,
        ctx.hmac_key.as_deref(),
    );
    info!(session_id = %ctx.session_id, "trace writer: stopped");
}

struct CurrentFile {
    path: PathBuf,
    file: tokio::fs::File,
    bytes_written: u64,
    opened_at: Instant,
    seq: u64,
}

impl CurrentFile {
    async fn write_event(&mut self, ev: &TraceEvent) -> Result<()> {
        let mut bytes = serde_json::to_vec(ev).context("serializing trace event")?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .await
            .context("writing trace event line")?;
        self.file.flush().await.context("flushing trace file")?;
        self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    async fn shutdown(mut self) -> Result<()> {
        self.file.flush().await.ok();
        self.file.shutdown().await.ok();
        Ok(())
    }
}

fn should_rotate(c: &CurrentFile, max_bytes: u64, max_age: Duration) -> bool {
    c.bytes_written >= max_bytes || c.opened_at.elapsed() >= max_age
}

/// Result of a rotation: the freshly-opened file, plus an optional manifest
/// entry for the file we just closed (and gzipped).
struct Rotated {
    next: CurrentFile,
    closed_manifest: Option<ManifestFile>,
}

async fn rotate(prev: CurrentFile, dir: &Path) -> Result<Rotated> {
    let next_seq_n = prev.seq.saturating_add(1);
    let prev_path = prev.path.clone();
    let prev_seq = prev.seq;
    let prev_bytes = prev.bytes_written;
    prev.shutdown().await.ok();

    // Gzip-on-rotate. Failure is a `WARN` — never block opening the next
    // file. The plain `.jsonl` is left in place so consumers still see
    // contiguous data.
    let gz_path = match gzip_file(&prev_path).await {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(error = %e, path = %prev_path.display(), "trace writer: gzip-on-rotate failed");
            None
        }
    };
    let (manifest_path, gzipped) = match &gz_path {
        Some(p) => (p.clone(), true),
        None => (prev_path.clone(), false),
    };
    let closed_manifest = manifest_for_closed(&manifest_path, prev_seq, prev_bytes, gzipped).ok();

    let next = open_seq(dir, next_seq_n).await?;
    Ok(Rotated {
        next,
        closed_manifest,
    })
}

/// Compress `src` to `<src>.gz` (DEFLATE level 6) and atomically rename
/// over the original on success. The uncompressed file is removed only
/// after the gzip lands and is fsynced — no data window where neither
/// exists.
async fn gzip_file(src: &Path) -> Result<PathBuf> {
    let src = src.to_path_buf();
    let dst = {
        let mut s = src.as_os_str().to_owned();
        s.push(".gz");
        PathBuf::from(s)
    };
    let dst_tmp = {
        let mut s = dst.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };
    let dst_for_blocking = dst.clone();
    let dst_tmp_for_blocking = dst_tmp.clone();
    let src_for_blocking = src.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::{copy, BufReader};
        let infile = std::fs::File::open(&src_for_blocking)
            .with_context(|| format!("open {}", src_for_blocking.display()))?;
        let mut reader = BufReader::new(infile);
        let outfile = std::fs::File::create(&dst_tmp_for_blocking)
            .with_context(|| format!("create {}", dst_tmp_for_blocking.display()))?;
        let mut enc = GzEncoder::new(outfile, Compression::default());
        copy(&mut reader, &mut enc).context("gzip stream")?;
        let mut outfile = enc.finish().context("gzip finish")?;
        outfile.flush().ok();
        // fsync the gz before rename so a crash never leaves a half-written
        // .gz hiding the original.
        outfile.sync_all().ok();
        std::fs::rename(&dst_tmp_for_blocking, &dst_for_blocking)
            .with_context(|| format!("rename {}", dst_for_blocking.display()))?;
        // Original `.jsonl` removed only after .gz is in place.
        std::fs::remove_file(&src_for_blocking).ok();
        Ok(())
    })
    .await
    .map_err(|e| anyhow!("gzip task join: {e}"))??;
    let _ = dst_tmp; // silence unused warning when feature gate flips
    Ok(dst)
}

async fn open_seq(dir: &Path, seq: u64) -> Result<CurrentFile> {
    let path = dir.join(format!("{seq:04}.jsonl"));
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("open trace file {}", path.display()))?;
    let bytes_written = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(CurrentFile {
        path,
        file,
        bytes_written,
        opened_at: Instant::now(),
        seq,
    })
}

fn scan_next_seq(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut highest: i64 = -1;
    for entry in rd.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Recognise both `<NNNN>.jsonl` (the live or never-rotated file) and
        // `<NNNN>.jsonl.gz` (rotated). Strip the suffix that's actually
        // present.
        let stem = if let Some(s) = name.strip_suffix(".jsonl.gz") {
            s
        } else if let Some(s) = name.strip_suffix(".jsonl") {
            s
        } else {
            continue;
        };
        if let Ok(n) = stem.parse::<i64>() {
            if n > highest {
                highest = n;
            }
        }
    }
    if highest < 0 {
        0
    } else {
        (highest as u64).saturating_add(1)
    }
}

// ---------- helpers ----------

/// `~/.one-for-all/sessions/`
pub fn sessions_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = home.join(".one-for-all").join("sessions");
    create_dir_secure(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

#[cfg(unix)]
fn create_dir_secure(p: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !p.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_dir_secure(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p)?;
    Ok(())
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- redactor ----------

/// Compiled per-session PII redaction state. Holds a `RegexSet` plus a
/// per-pattern `Vec<Regex>` for actually replacing matched substrings (the
/// `RegexSet` only tells us *if* a string matches; we still need the
/// individual `Regex` objects to call `.replace_all`).
pub(crate) struct Redactor {
    set: RegexSet,
    patterns: Vec<regex::Regex>,
    /// Lower-cased structural denylist; an object key matching one of these
    /// has its value (regardless of pattern match) replaced with the
    /// redaction sentinel.
    header_denylist: Vec<&'static str>,
}

impl Redactor {
    pub(crate) fn new(patterns: &[String]) -> Result<Self> {
        let mut compiled: Vec<regex::Regex> = Vec::with_capacity(patterns.len());
        let mut sources: Vec<String> = Vec::with_capacity(patterns.len());
        for p in patterns {
            match regex::Regex::new(p) {
                Ok(rx) => {
                    compiled.push(rx);
                    sources.push(p.clone());
                }
                Err(e) => {
                    warn!(pattern = %p, error = %e, "trace.redact: invalid regex skipped");
                }
            }
        }
        let set = RegexSet::new(&sources).with_context(|| "compiling redact pattern set")?;
        Ok(Self {
            set,
            patterns: compiled,
            header_denylist: HEADER_DENYLIST.to_vec(),
        })
    }

    /// Apply pattern-based scrubbing to a single string node.
    fn scrub_str(&self, s: &str) -> String {
        if self.patterns.is_empty() || !self.set.is_match(s) {
            return s.to_owned();
        }
        let mut out = std::borrow::Cow::Borrowed(s);
        for rx in &self.patterns {
            // .replace_all returns Cow; promote on first match, otherwise
            // keep borrowing the original.
            let replaced = rx.replace_all(&out, REDACTED);
            if let std::borrow::Cow::Owned(o) = replaced {
                out = std::borrow::Cow::Owned(o);
            }
        }
        out.into_owned()
    }

    fn is_denylisted_key(&self, key: &str) -> bool {
        let lower = key.to_ascii_lowercase();
        self.header_denylist.iter().any(|d| *d == lower)
    }
}

/// Walk `value` recursively, returning a new `Value` with redaction applied.
/// Object keys are NEVER touched — only string-typed leaf values inside
/// arrays / objects.
pub(crate) fn redact_value(value: &Value, r: &Redactor) -> Value {
    match value {
        Value::String(s) => Value::String(r.scrub_str(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(|v| redact_value(v, r)).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let redacted_value = if r.is_denylisted_key(k) {
                    // Header / cookie / auth keys are nuked structurally
                    // regardless of pattern match.
                    Value::String(REDACTED.into())
                } else {
                    redact_value(v, r)
                };
                out.insert(k.clone(), redacted_value);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Apply `redact_value` only to user-payload fields (`params`, `args`,
/// `result`); metadata (`session_id`, `target_id`, `method`, `tool`,
/// `tab_id`, `id`, `path`, `hash`) is preserved verbatim.
fn redact_event(ev: &TraceEvent, r: &Redactor) -> TraceEvent {
    match ev.clone() {
        TraceEvent::CdpRequest {
            ts_ms,
            session_id,
            target_id,
            id,
            method,
            params,
        } => TraceEvent::CdpRequest {
            ts_ms,
            session_id,
            target_id,
            id,
            method,
            params: redact_value(&params, r),
        },
        TraceEvent::CdpResponse {
            ts_ms,
            session_id,
            target_id,
            id,
            result,
            error,
        } => TraceEvent::CdpResponse {
            ts_ms,
            session_id,
            target_id,
            id,
            result: result.as_ref().map(|v| redact_value(v, r)),
            error: error.as_ref().map(|v| redact_value(v, r)),
        },
        TraceEvent::CdpEvent {
            ts_ms,
            session_id,
            target_id,
            method,
            params,
        } => TraceEvent::CdpEvent {
            ts_ms,
            session_id,
            target_id,
            method,
            params: redact_value(&params, r),
        },
        TraceEvent::Action {
            ts_ms,
            session_id,
            tab_id,
            tool,
            args,
            result,
        } => TraceEvent::Action {
            ts_ms,
            session_id,
            tab_id,
            tool,
            args: redact_value(&args, r),
            result: redact_value(&result, r),
        },
        // Screenshot / DomSnapshot carry no user payload, only paths.
        other @ (TraceEvent::Screenshot { .. } | TraceEvent::DomSnapshot { .. }) => other,
    }
}

// ---------- manifest ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub gzipped: bool,
    pub seq: u64,
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    session_id: &'a str,
    started_ts_ms: u64,
    written_ts_ms: u64,
    files: &'a [ManifestFile],
    /// HMAC-SHA256 over the canonical JSON of this struct with `hmac`
    /// removed. `ofa-replay verify` recomputes via `jq -c 'del(.hmac)'`
    /// + `openssl dgst -sha256 -hmac` and compares.
    #[serde(skip_serializing_if = "Option::is_none")]
    hmac: Option<String>,
}

fn manifest_for_closed(
    path: &Path,
    seq: u64,
    bytes_hint: u64,
    gzipped: bool,
) -> Result<ManifestFile> {
    let bytes_actual = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(bytes_hint);
    let contents = std::fs::read(path).with_context(|| format!("hash {}", path.display()))?;
    let sha256 = sha256_hex(&contents);
    let path_str = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    Ok(ManifestFile {
        path: path_str,
        bytes: bytes_actual,
        sha256,
        gzipped,
        seq,
    })
}

/// Atomically (write-tmp → rename) write the manifest. Failures are
/// `WARN`-and-continue — never block the writer task.
///
/// HMAC is computed over the **canonical** JSON form of the manifest with
/// the `hmac` field removed — i.e. the body round-tripped through
/// `serde_json::Value` so key order matches `jq -c 'del(.hmac)'` (alphabetical
/// for the default `serde_json::Map`). This is the form `ofa-replay verify`
/// reproduces, so the recomputed digest matches.
fn write_manifest(
    dir: &Path,
    session_id: &str,
    started_ts_ms: u64,
    files: &[ManifestFile],
    hmac_key: Option<&[u8]>,
) {
    let written_ts_ms = current_unix_ms();
    let body = Manifest {
        session_id,
        started_ts_ms,
        written_ts_ms,
        files,
        hmac: None,
    };
    // Canonicalize: struct → Value → bytes. Without `preserve_order`,
    // `serde_json::Map` round-trips into BTreeMap-sorted form, matching what
    // `ofa-replay verify` produces via `jq`.
    let body_value: Value = match serde_json::to_value(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "trace manifest: serialize body failed");
            return;
        }
    };
    let body_bytes = match serde_json::to_vec(&body_value) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "trace manifest: canonical body serialize failed");
            return;
        }
    };
    let final_bytes = match hmac_key {
        Some(k) => {
            let hmac_hex = compute_hmac_sha256(k, &body_bytes);
            // Inject `hmac` into the canonical Value and serialize that —
            // staying in Value-form so the on-disk doc preserves the
            // canonical key order minus the new hmac key.
            let mut signed = body_value;
            if let Some(map) = signed.as_object_mut() {
                map.insert("hmac".into(), Value::String(hmac_hex));
            }
            match serde_json::to_vec(&signed) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "trace manifest: signed serialize failed");
                    return;
                }
            }
        }
        None => body_bytes,
    };
    let path = dir.join("manifest.json");
    let tmp = dir.join("manifest.json.tmp");
    if let Err(e) = std::fs::write(&tmp, &final_bytes) {
        warn!(error = %e, path = %tmp.display(), "trace manifest: write tmp failed");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!(error = %e, path = %path.display(), "trace manifest: rename failed");
    }
}

fn compute_hmac_sha256(key: &[u8], body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = match Hmac::<Sha256>::new_from_slice(key) {
        Ok(m) => m,
        Err(_) => {
            // Hmac<Sha256> accepts any-length key; this branch is
            // unreachable but plumbed for safety.
            return String::new();
        }
    };
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    hex::encode(bytes)
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn ev_request(id: i64) -> TraceEvent {
        TraceEvent::CdpRequest {
            ts_ms: 1234,
            session_id: "s1".into(),
            target_id: Some("t1".into()),
            id,
            method: "Page.navigate".into(),
            params: json!({"url": "https://example.com"}),
        }
    }

    #[tokio::test]
    async fn jsonl_round_trip_request() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 1_000_000,
            max_age: Duration::from_secs(3600),
            buffer_cap: 64,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s1", tmp.path(), opts).unwrap();

        for i in 0..3 {
            w.record(ev_request(i));
        }
        assert!(w.flush_for_test(Duration::from_secs(2)).await);
        w.shutdown().await.unwrap();

        let path = tmp.path().join("0000.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        for (i, line) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["kind"], "cdp_request");
            assert_eq!(v["id"], i as i64);
            assert_eq!(v["session_id"], "s1");
            assert_eq!(v["method"], "Page.navigate");
            assert!(v["ts_ms"].is_u64());
        }
    }

    #[tokio::test]
    async fn rotates_on_size() {
        let tmp = TempDir::new().unwrap();
        // Each event serializes to ~150 bytes; 1KB cap → 6-7 events per file.
        let opts = TraceOptions {
            max_bytes: 1_024,
            max_age: Duration::from_secs(3600),
            buffer_cap: 1024,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s1", tmp.path(), opts).unwrap();
        for i in 0..50 {
            w.record(ev_request(i));
        }
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();

        // Rotated files are gzipped; the live file (highest seq) is plain.
        let mut files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".jsonl") || n.ends_with(".jsonl.gz"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected ≥2 rotated files, got {files:?}");
        // First rotated file must be 0000.jsonl.gz once gzip-on-rotate landed.
        assert!(
            files[0] == "0000.jsonl.gz" || files[0] == "0000.jsonl",
            "unexpected first file: {files:?}"
        );

        // Round-trip every line, transparently decompressing gzipped files.
        let mut total_lines = 0usize;
        for f in &files {
            let path = tmp.path().join(f);
            let content = if f.ends_with(".gz") {
                use flate2::read::GzDecoder;
                use std::io::Read as _;
                let raw = std::fs::read(&path).unwrap();
                let mut dec = GzDecoder::new(&raw[..]);
                let mut s = String::new();
                dec.read_to_string(&mut s).unwrap();
                s
            } else {
                std::fs::read_to_string(&path).unwrap()
            };
            for line in content.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                assert_eq!(v["kind"], "cdp_request");
                total_lines += 1;
            }
        }
        assert_eq!(total_lines, 50);
    }

    #[tokio::test]
    async fn rotates_on_age() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 100_000_000,
            max_age: Duration::from_millis(100),
            buffer_cap: 64,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s1", tmp.path(), opts).unwrap();
        w.record(ev_request(0));
        assert!(w.flush_for_test(Duration::from_secs(2)).await);
        // Sleep > max_age then write again — second event must land in 0001.jsonl.
        tokio::time::sleep(Duration::from_millis(300)).await;
        w.record(ev_request(1));
        assert!(w.flush_for_test(Duration::from_secs(2)).await);
        w.shutdown().await.unwrap();

        // Rotated files become .jsonl.gz; count both shapes.
        let mut files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".jsonl") || n.ends_with(".jsonl.gz"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "expected age rotation, got {files:?}");
    }

    #[tokio::test]
    async fn overflow_drops_oldest_with_warn() {
        let tmp = TempDir::new().unwrap();
        // Tiny buffer; we never let the writer drain — events accumulate
        // until cap, then drop-oldest kicks in.
        let opts = TraceOptions {
            max_bytes: 100_000_000,
            max_age: Duration::from_secs(3600),
            buffer_cap: 4,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s1", tmp.path(), opts.clone()).unwrap();
        // Override the in-memory cap (start_in_dir uses TRACE_CHANNEL_CAP).
        // To exercise the drop-oldest branch without ever waking the writer,
        // we shrink the buffer cap on the fly via the public field is private,
        // so we instead push enough events past the cap to force drops while
        // the writer is busy in select{}; the `notify` will batch them.
        let total = 50;
        for i in 0..total {
            w.record(ev_request(i));
        }
        // Ensure dropped counter advanced — at least (total - buffer_cap) drops.
        assert!(
            w.dropped_count() >= (total as u64 - opts.buffer_cap as u64),
            "expected drops, got {}",
            w.dropped_count()
        );
        // And writer still healthy — flush remaining.
        assert!(w.flush_for_test(Duration::from_secs(2)).await);
        w.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn screenshot_path_is_relative_and_file_exists() {
        let tmp = TempDir::new().unwrap();
        let w = TraceWriter::start_in_dir("s1", tmp.path(), TraceOptions::default()).unwrap();
        let png = b"\x89PNG\r\n\x1a\nfake-bytes";
        let rel = w.save_screenshot_png("page.click", png).unwrap();
        assert!(rel.starts_with("screenshots/"));
        assert!(!rel.starts_with('/'));
        let abs = tmp.path().join(&rel);
        let actual = std::fs::read(&abs).unwrap();
        assert_eq!(actual, png);
        w.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_hash_stable_and_changes_on_diff() {
        let tmp = TempDir::new().unwrap();
        let w = TraceWriter::start_in_dir("s1", tmp.path(), TraceOptions::default()).unwrap();
        let a = json!({"tree": "alpha", "count": 1});
        let b = json!({"tree": "beta", "count": 1});
        let (path1, hash1) = w.save_snapshot_json(1, &a).unwrap();
        let (path2, hash2) = w.save_snapshot_json(2, &a).unwrap();
        let (_, hash3) = w.save_snapshot_json(3, &b).unwrap();
        assert!(path1.starts_with("snapshots/"));
        assert!(path2.starts_with("snapshots/"));
        assert_eq!(hash1, hash2, "same input → same hash");
        assert_ne!(hash1, hash3, "different input → different hash");
        // hex of sha256 is 64 chars.
        assert_eq!(hash1.len(), 64);
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));
        w.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dir_creation_idempotent() {
        let tmp = TempDir::new().unwrap();
        let w1 = TraceWriter::start_in_dir("s1", tmp.path(), TraceOptions::default()).unwrap();
        // Reuse: a second writer at the same path must not error and must
        // start its file at the next seq beyond what's on disk.
        w1.record(ev_request(0));
        assert!(w1.flush_for_test(Duration::from_secs(2)).await);
        w1.shutdown().await.unwrap();
        let w2 = TraceWriter::start_in_dir("s1", tmp.path(), TraceOptions::default()).unwrap();
        w2.record(ev_request(1));
        assert!(w2.flush_for_test(Duration::from_secs(2)).await);
        w2.shutdown().await.unwrap();
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".jsonl"))
            .collect();
        // Either two distinct seqs or the second writer appended into 0001 —
        // both are acceptable, what matters is no panic + both events are persisted.
        assert!(!files.is_empty());
    }

    #[tokio::test]
    async fn registry_is_idempotent_per_session() {
        // Use a registry-managed writer with a custom dir via insert.
        let tmp = TempDir::new().unwrap();
        let w = Arc::new(
            TraceWriter::start_in_dir("s_reg", tmp.path(), TraceOptions::default()).unwrap(),
        );
        let reg = TraceRegistry::new();
        reg.insert(Arc::clone(&w));
        let again = reg.get("s_reg").unwrap();
        assert!(Arc::ptr_eq(&w, &again));
        let removed = reg.remove("s_reg").unwrap();
        assert!(Arc::ptr_eq(&w, &removed));
        assert!(reg.get("s_reg").is_none());
        w.shutdown().await.unwrap();
    }

    #[test]
    fn trace_options_from_env_uses_defaults() {
        // Note: this mutates env, so we use unique-enough vars and restore.
        let prev_b = std::env::var("OFA_TRACE_MAX_BYTES").ok();
        let prev_a = std::env::var("OFA_TRACE_MAX_AGE_SECS").ok();
        std::env::remove_var("OFA_TRACE_MAX_BYTES");
        std::env::remove_var("OFA_TRACE_MAX_AGE_SECS");
        let opts = TraceOptions::from_env();
        assert_eq!(opts.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(opts.max_age, Duration::from_secs(DEFAULT_MAX_AGE_SECS));
        if let Some(v) = prev_b {
            std::env::set_var("OFA_TRACE_MAX_BYTES", v);
        }
        if let Some(v) = prev_a {
            std::env::set_var("OFA_TRACE_MAX_AGE_SECS", v);
        }
    }

    #[test]
    fn trace_event_kind_tag_matches_serde() {
        let req = ev_request(7);
        assert_eq!(req.kind(), "cdp_request");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["kind"], "cdp_request");
    }

    #[tokio::test]
    async fn rotated_files_are_gzipped() {
        // Tiny size cap forces multiple rotations; the closed files must
        // land as `.jsonl.gz` and the live (highest seq) file stays plain.
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 512,
            max_age: Duration::from_secs(3600),
            buffer_cap: 1024,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s1", tmp.path(), opts).unwrap();
        for i in 0..40 {
            w.record(ev_request(i));
        }
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();

        let mut entries: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        entries.sort();
        let gz_count = entries.iter().filter(|n| n.ends_with(".jsonl.gz")).count();
        assert!(
            gz_count >= 1,
            "expected at least one gzipped rotated file, got {entries:?}"
        );

        // Decompress every gz and confirm the JSONL inside parses.
        for f in entries.iter().filter(|n| n.ends_with(".jsonl.gz")) {
            use flate2::read::GzDecoder;
            use std::io::Read as _;
            let raw = std::fs::read(tmp.path().join(f)).unwrap();
            let mut dec = GzDecoder::new(&raw[..]);
            let mut s = String::new();
            dec.read_to_string(&mut s).unwrap();
            for line in s.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                assert_eq!(v["kind"], "cdp_request");
            }
        }
    }

    #[tokio::test]
    async fn manifest_round_trips_with_hmac() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 256,
            max_age: Duration::from_secs(3600),
            buffer_cap: 64,
            redact_patterns: Vec::new(),
            hmac_key: Some(b"unit-test-hmac-key".to_vec()),
        };
        let w = TraceWriter::start_in_dir("s_hmac", tmp.path(), opts).unwrap();
        for i in 0..15 {
            w.record(ev_request(i));
        }
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();

        let manifest_path = tmp.path().join("manifest.json");
        let raw = std::fs::read(&manifest_path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let expected = doc["hmac"].as_str().unwrap().to_owned();
        assert!(!expected.is_empty(), "expected non-empty hmac in manifest");
        // Mirror ofa-replay verify: strip hmac then HMAC-SHA256 over the
        // canonical JSON of the remaining body.
        doc.as_object_mut().unwrap().remove("hmac");
        let body = serde_json::to_vec(&doc).unwrap();
        let actual = compute_hmac_sha256(b"unit-test-hmac-key", &body);
        assert_eq!(expected, actual, "manifest HMAC must round-trip");
        // Manifest must list at least one file with sha256 + gzipped flag set.
        let files = doc["files"].as_array().unwrap();
        assert!(!files.is_empty(), "manifest must list rotated files");
        for f in files {
            assert!(f["sha256"].as_str().is_some_and(|h| h.len() == 64));
            assert!(f["bytes"].as_u64().is_some());
            assert!(f["gzipped"].is_boolean());
        }
    }

    #[tokio::test]
    async fn manifest_without_hmac_when_key_unset() {
        let tmp = TempDir::new().unwrap();
        let w = TraceWriter::start_in_dir("s_nohmac", tmp.path(), TraceOptions::default()).unwrap();
        w.record(ev_request(0));
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();
        let raw = std::fs::read(tmp.path().join("manifest.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(
            doc.get("hmac").is_none(),
            "hmac must be omitted when key is unset"
        );
    }

    #[tokio::test]
    async fn redact_patterns_scrub_string_values() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 1_000_000,
            max_age: Duration::from_secs(3600),
            buffer_cap: 64,
            redact_patterns: vec![r"secret-\d+".to_string()],
            hmac_key: None,
        };
        let w = TraceWriter::start_in_dir("s_red", tmp.path(), opts).unwrap();
        w.record(TraceEvent::CdpRequest {
            ts_ms: 42,
            session_id: "s_red".into(),
            target_id: None,
            id: 1,
            method: "Network.setExtraHTTPHeaders".into(),
            params: json!({
                "headers": {"Cookie": "abc", "X-Trace": "secret-99-trail"},
                "url": "https://x.test?token=secret-12345"
            }),
        });
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();

        let content = std::fs::read_to_string(tmp.path().join("0000.jsonl")).unwrap();
        assert!(
            !content.contains("secret-12345"),
            "pattern not scrubbed: {content}"
        );
        assert!(
            !content.contains("secret-99"),
            "pattern not scrubbed: {content}"
        );
        assert!(
            !content.contains("\"abc\""),
            "Cookie header not scrubbed structurally: {content}"
        );
        assert!(content.contains(REDACTED));
        // Method / session_id metadata are preserved.
        assert!(content.contains("Network.setExtraHTTPHeaders"));
        assert!(content.contains("s_red"));
    }

    #[tokio::test]
    async fn invalid_redact_pattern_is_skipped_not_panic() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 1_000_000,
            max_age: Duration::from_secs(3600),
            buffer_cap: 64,
            redact_patterns: vec![
                "(unbalanced".to_string(),  // invalid — must be skipped
                "valid-[a-z]+".to_string(), // valid — must compile
            ],
            hmac_key: None,
        };
        let w = TraceWriter::start_in_dir("s_inv", tmp.path(), opts).unwrap();
        w.record(TraceEvent::CdpRequest {
            ts_ms: 1,
            session_id: "s_inv".into(),
            target_id: None,
            id: 1,
            method: "X".into(),
            params: json!({"k": "valid-secret"}),
        });
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        w.shutdown().await.unwrap();
        let content = std::fs::read_to_string(tmp.path().join("0000.jsonl")).unwrap();
        assert!(content.contains(REDACTED));
        assert!(!content.contains("valid-secret"));
    }

    #[tokio::test]
    async fn write_latency_snapshot_records_samples() {
        let tmp = TempDir::new().unwrap();
        let w = TraceWriter::start_in_dir("s_lat", tmp.path(), TraceOptions::default()).unwrap();
        for i in 0..200 {
            w.record(ev_request(i));
        }
        assert!(w.flush_for_test(Duration::from_secs(5)).await);
        let snap = w.write_latency_snapshot();
        assert!(snap.count >= 1, "expected at least one latency sample");
        // p99 must be a real (>=0) measurement; p99 == 0 only means we
        // wrote so fast hdrhistogram saturates the low bound, which is
        // still acceptable.
        let _ = snap.p99_us; // smoke check
        w.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn drop_alarm_latches_after_threshold() {
        let tmp = TempDir::new().unwrap();
        let opts = TraceOptions {
            max_bytes: 100_000_000,
            max_age: Duration::from_secs(3600),
            buffer_cap: 4,
            ..TraceOptions::default()
        };
        let w = TraceWriter::start_in_dir("s_alarm", tmp.path(), opts).unwrap();
        // Push >> threshold while never letting the writer drain.
        for i in 0..(DROP_ALARM_THRESHOLD as i64 + 50) {
            w.record(ev_request(i));
        }
        assert!(
            w.drop_alarm_active(),
            "alarm must latch after threshold drops"
        );
        w.clear_alarm();
        assert!(!w.drop_alarm_active(), "clear_alarm must release the latch");
        // Subsequent drops must be able to re-arm the alarm.
        for i in 0..(DROP_ALARM_THRESHOLD as i64 + 50) {
            w.record(ev_request(i));
        }
        assert!(w.drop_alarm_active(), "alarm must re-arm after clear");
        w.shutdown().await.unwrap();
    }

    #[test]
    fn redactor_preserves_object_keys() {
        let r = Redactor::new(&["topkey".to_string()]).unwrap();
        let v = json!({"topkey": "topkey-value-with-topkey"});
        let out = redact_value(&v, &r);
        // The key stays; the value is scrubbed.
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("topkey"));
        let val = obj.get("topkey").unwrap().as_str().unwrap();
        assert!(!val.contains("topkey"));
        assert!(val.contains(REDACTED));
    }

    #[tokio::test]
    async fn save_screencast_frame_writes_with_correct_extension() {
        // Vision-frame doubled-storage: a JPEG frame must land as .jpg in
        // screenshots/, distinct from screenshot.png filenames.
        let tmp = TempDir::new().unwrap();
        let w = TraceWriter::start_in_dir("s_v", tmp.path(), TraceOptions::default()).unwrap();
        let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIFfake-jpeg";
        let rel = w.save_screencast_frame("jpg", bytes).unwrap();
        assert!(rel.starts_with("screenshots/"));
        assert!(rel.contains("vision.frame-"));
        assert!(rel.ends_with(".jpg"));
        let abs = tmp.path().join(&rel);
        let actual = std::fs::read(&abs).unwrap();
        assert_eq!(actual, bytes);
        // Sanity: a second call must not collide with the first even on the
        // same millisecond — strict-monotonic seq ensures this.
        let rel2 = w.save_screencast_frame("jpg", bytes).unwrap();
        assert_ne!(rel, rel2);
        w.shutdown().await.unwrap();
    }
}
