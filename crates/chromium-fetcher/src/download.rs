//! Single-stream, resumable downloader for Chrome-for-Testing zips.
//!
//! Owned by `chromium-fetcher`. One [`download_zip`] call:
//!
//! 1. HEADs the URL to learn the total size and whether the server supports
//!    `Range` requests.
//! 2. Streams the body into `<dest>.tmp` (a single open connection).
//! 3. On any transport error or stall, sleeps with exponential backoff and
//!    re-issues `GET` with `Range: bytes=<existing_size>-` to append from
//!    the current offset.
//! 4. Atomically renames `<dest>.tmp` → `<dest>` once the byte count matches
//!    the server-reported total. SHA verification is the caller's job.
//!
//! Why single-stream only: parallel-chunk downloads are a fragility
//! multiplier on flaky residential networks — N independent peer-resets
//! produce N stale chunks that fail SHA verification. One connection,
//! resumed aggressively, is what `curl --retry 50 --retry-all-errors -C -`
//! does and matches the observed reliable behavior. The previous
//! parallel-chunk path was deleted in T8: per the team-lead's hardening
//! directive, the option is closed entirely (no env-var escape hatch).
//!
//! Progress is reported in two channels per the broker daemon model:
//! - `tracing::info` every 5 MiB (the daemon has no terminal, so no
//!   `indicatif`).
//! - `observability::metrics::fetch_metrics()` — process-wide histograms
//!   and counters surfaced through `_internal.metrics`.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use observability::metrics::{fetch_metrics, FetchMetrics};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const DEFAULT_PROGRESS_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_MAX_ATTEMPTS: u32 = 30;
const DEFAULT_STALL: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT: Duration = Duration::from_secs(60);
const DEFAULT_TOTAL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Tunables for [`download_zip`].
///
/// `parallel_chunks` and `chunk_timeout` are retained for source-level
/// API stability with existing callers (the broker uses
/// `..FetchOptions::default()`); they have no effect — the download path
/// is single-stream by construction.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// DEPRECATED — has no effect. Kept for source-level API stability.
    pub parallel_chunks: u32,
    /// Timeout for the initial HEAD probe.
    pub head_timeout: Duration,
    /// DEPRECATED — superseded by [`stall_timeout`] + [`total_timeout`].
    /// Kept for source-level API stability.
    pub chunk_timeout: Duration,
    /// Emit a `tracing::info` every N bytes downloaded. Zero disables.
    pub progress_interval_bytes: u64,
    /// Compatibility flag. When `false`, suppresses progress logs entirely.
    pub progress: bool,
    /// Max attempts for the whole download (default 30).
    pub max_attempts: u32,
    /// If no body bytes arrive for this long, abort the current attempt
    /// and retry from the current offset (default 30s).
    pub stall_timeout: Duration,
    /// TCP connect timeout for each attempt (default 60s).
    pub connect_timeout: Duration,
    /// Hard wall-clock cap across all attempts (default 30 min).
    pub total_timeout: Duration,
    /// Initial backoff after a transport error (default 1s).
    pub initial_backoff: Duration,
    /// Maximum backoff between attempts (default 30s).
    pub max_backoff: Duration,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            parallel_chunks: 1,
            head_timeout: Duration::from_secs(30),
            chunk_timeout: Duration::from_secs(600),
            progress_interval_bytes: DEFAULT_PROGRESS_BYTES,
            progress: true,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            stall_timeout: DEFAULT_STALL,
            connect_timeout: DEFAULT_CONNECT,
            total_timeout: DEFAULT_TOTAL,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

/// Server probe result.
#[derive(Debug, Clone, Copy)]
struct Head {
    /// Total bytes the server says the resource is.
    total: u64,
    /// Whether `Accept-Ranges: bytes` is advertised.
    accept_ranges: bool,
}

async fn head(client: &reqwest::Client, url: &str) -> Result<Head> {
    let resp = client
        .head(url)
        .send()
        .await
        .with_context(|| format!("HEAD {url}"))?
        .error_for_status()
        .with_context(|| format!("HEAD {url} status"))?;
    let total = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("server did not return Content-Length for {url}"))?;
    let accept_ranges = resp
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);
    Ok(Head {
        total,
        accept_ranges,
    })
}

/// Classify a `reqwest` error as retryable (transport/transient) or fatal
/// (genuine 4xx other than 416, malformed URL, etc.).
fn is_reqwest_error_retryable(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() || err.is_request() || err.is_body() {
        return true;
    }
    if let Some(status) = err.status() {
        if status.is_server_error() {
            return true;
        }
        if status.as_u16() == 408 || status.as_u16() == 429 {
            return true;
        }
        return false;
    }
    // No status → most likely transport-level (peer reset, DNS, IO).
    true
}

/// What happened on a single attempt at streaming bytes from `url` into
/// `tmp_path` starting at offset `already`.
enum AttemptOutcome {
    /// Reached EOF and the file is now `expected_total` bytes long.
    Done,
    /// Transport-level failure; safe to retry with the new offset.
    Retry(anyhow::Error),
    /// Server says the requested Range is not satisfiable. Caller should
    /// truncate the tmp file and retry from offset 0 (one-shot).
    RangeNotSatisfiable,
    /// Fatal — do not retry.
    Fatal(anyhow::Error),
}

/// Stream one HTTP request body into `tmp_path` starting at byte `already`.
/// Honors a no-bytes stall watchdog. Returns the number of bytes received
/// in this attempt (for histogram bookkeeping) via the metrics handle.
#[allow(clippy::too_many_arguments)]
async fn stream_attempt(
    client: &reqwest::Client,
    url: &str,
    tmp_path: &Path,
    already: u64,
    expected_total: u64,
    opts: &DownloadOptions,
    progress_state: &mut ProgressState,
    metrics: &FetchMetrics,
) -> AttemptOutcome {
    metrics.record_attempt();

    let mut req = client.get(url);
    if already > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return if is_reqwest_error_retryable(&e) {
                AttemptOutcome::Retry(anyhow::Error::new(e).context("send GET"))
            } else {
                AttemptOutcome::Fatal(anyhow::Error::new(e).context("send GET"))
            };
        }
    };

    let status = resp.status();
    if status.as_u16() == 416 {
        metrics.record_range_416();
        return AttemptOutcome::RangeNotSatisfiable;
    }
    if !status.is_success() {
        let err = anyhow!("GET {url} returned status {}", status.as_u16());
        return if status.is_server_error() || status.as_u16() == 408 || status.as_u16() == 429 {
            AttemptOutcome::Retry(err)
        } else {
            AttemptOutcome::Fatal(err)
        };
    }
    // If we asked for a Range and the server returned 200 OK (not 206), it
    // ignored our offset and is sending the whole body again. Reset the
    // tmp file so we don't double-write the prefix.
    if already > 0 && status.as_u16() != 206 {
        if let Err(e) = tokio::fs::File::create(tmp_path).await {
            return AttemptOutcome::Fatal(
                anyhow::Error::new(e).context("truncate tmp after non-206 resume"),
            );
        }
        progress_state.bytes_total = 0;
        progress_state.bytes_since_last_log = 0;
    }

    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tmp_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            return AttemptOutcome::Fatal(
                anyhow::Error::new(e).context(format!("open tmp {}", tmp_path.display())),
            );
        }
    };

    let mut attempt_bytes: u64 = 0;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::time::timeout(opts.stall_timeout, stream.next()).await;
        match next {
            Err(_) => {
                metrics.record_stall();
                metrics.record_attempt_bytes(attempt_bytes);
                return AttemptOutcome::Retry(anyhow!(
                    "stall: no bytes received in {}s",
                    opts.stall_timeout.as_secs()
                ));
            }
            Ok(None) => break, // clean EOF
            Ok(Some(Err(e))) => {
                metrics.record_attempt_bytes(attempt_bytes);
                return if is_reqwest_error_retryable(&e) {
                    AttemptOutcome::Retry(anyhow::Error::new(e).context("read body"))
                } else {
                    AttemptOutcome::Fatal(anyhow::Error::new(e).context("read body"))
                };
            }
            Ok(Some(Ok(bytes))) => {
                if let Err(e) = file.write_all(&bytes).await {
                    return AttemptOutcome::Fatal(
                        anyhow::Error::new(e).context(format!("write tmp {}", tmp_path.display())),
                    );
                }
                let n = bytes.len() as u64;
                attempt_bytes = attempt_bytes.saturating_add(n);
                metrics.record_bytes(n);
                progress_state.observe(n, expected_total, opts);
            }
        }
    }

    metrics.record_attempt_bytes(attempt_bytes);

    if let Err(e) = file.flush().await {
        return AttemptOutcome::Fatal(
            anyhow::Error::new(e).context(format!("flush {}", tmp_path.display())),
        );
    }
    if let Err(e) = file.sync_all().await {
        return AttemptOutcome::Fatal(
            anyhow::Error::new(e).context(format!("fsync {}", tmp_path.display())),
        );
    }
    AttemptOutcome::Done
}

#[derive(Debug)]
struct ProgressState {
    bytes_total: u64,
    bytes_since_last_log: u64,
}

impl ProgressState {
    fn new(initial: u64) -> Self {
        Self {
            bytes_total: initial,
            bytes_since_last_log: 0,
        }
    }
    fn observe(&mut self, n: u64, expected_total: u64, opts: &DownloadOptions) {
        self.bytes_total = self.bytes_total.saturating_add(n);
        self.bytes_since_last_log = self.bytes_since_last_log.saturating_add(n);
        if !opts.progress || opts.progress_interval_bytes == 0 {
            return;
        }
        if self.bytes_since_last_log >= opts.progress_interval_bytes {
            tracing::info!(
                bytes = self.bytes_total,
                total = expected_total,
                pct = format!(
                    "{:.1}%",
                    (self.bytes_total as f64 / expected_total.max(1) as f64) * 100.0
                ),
                "chromium-fetcher: download progress"
            );
            self.bytes_since_last_log = 0;
        }
    }
}

/// Tmp-file companion to `dest`. We append `.tmp` to the full filename
/// (e.g. `chrome-mac-arm64.zip` → `chrome-mac-arm64.zip.tmp`) instead of
/// `with_extension("tmp")` (which would replace `.zip`).
fn tmp_path_for(dest: &Path) -> Result<PathBuf> {
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow!("dest has no filename: {}", dest.display()))?;
    let mut s = name.to_os_string();
    s.push(".tmp");
    Ok(dest.with_file_name(s))
}

/// Download `url` into `dest`, single-stream and resumable.
///
/// `dest` must include the final filename (e.g. `…/chrome-mac-arm64.zip`).
/// The tmp file lives at `<dest>.tmp` and is atomically renamed into place
/// once the byte count matches the server-reported total. Caller is
/// responsible for SHA verification — `dest` may be a stale
/// (wrong-hash) file from a prior run, in which case the caller should
/// remove it before re-invoking.
pub async fn download_zip(url: &str, dest: &Path, opts: &DownloadOptions) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create dest parent {}", parent.display()))?;
        }
    }
    if dest.exists() {
        // Already downloaded — caller is responsible for hash-verifying.
        tracing::info!(path = %dest.display(), "zip already present, skipping download");
        return Ok(());
    }

    let metrics = fetch_metrics();
    metrics.record_download_start();
    let started = Instant::now();

    let client = build_client(opts)?;
    let probe = match tokio::time::timeout(opts.head_timeout, head(&client, url))
        .await
        .map_err(|_| anyhow!("HEAD timed out for {url}"))
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            metrics.record_download_failure();
            return Err(e.context(format!("HEAD {url}")));
        }
        Err(e) => {
            metrics.record_download_failure();
            return Err(e);
        }
    };
    if !probe.accept_ranges {
        tracing::warn!(
            url,
            "server does not advertise Accept-Ranges: bytes; resume across attempts may force a full restart"
        );
    }

    let tmp = tmp_path_for(dest)?;
    let stream_result = single_stream_to_tmp(&client, url, &tmp, probe.total, opts, metrics).await;
    if let Err(e) = stream_result {
        metrics.record_download_failure();
        return Err(e);
    }

    finalize(&tmp, dest, probe.total).await?;
    metrics.record_download_completion(started.elapsed().as_millis() as u64);
    Ok(())
}

/// Build the shared `reqwest::Client`. `rustls-tls` is the only TLS backend
/// (workspace feature set). `connect_timeout` is the connection establishment
/// budget; total request timeout is intentionally NOT set — body liveness
/// is governed by the stall watchdog inside `stream_attempt`.
fn build_client(opts: &DownloadOptions) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(opts.connect_timeout)
        .build()
        .context("build reqwest client")
}

/// Drive the retry loop for one byte stream into `tmp`.
async fn single_stream_to_tmp(
    client: &reqwest::Client,
    url: &str,
    tmp: &Path,
    expected_total: u64,
    opts: &DownloadOptions,
    metrics: &FetchMetrics,
) -> Result<()> {
    let initial = match tokio::fs::metadata(tmp).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if initial > expected_total {
        // Corrupt resume state — server claims the resource is shorter
        // than what we have on disk. Truncate and start over.
        tokio::fs::File::create(tmp)
            .await
            .with_context(|| format!("truncate stale tmp {}", tmp.display()))?;
    }
    let mut progress = ProgressState::new(initial.min(expected_total));

    let deadline = Instant::now() + opts.total_timeout;
    let mut backoff = opts.initial_backoff;
    let mut range_416_consumed = false;

    for attempt in 1..=opts.max_attempts {
        let already = match tokio::fs::metadata(tmp).await {
            Ok(m) => m.len(),
            Err(_) => 0,
        };
        if already == expected_total {
            return Ok(());
        }
        if already > expected_total {
            tokio::fs::File::create(tmp)
                .await
                .with_context(|| format!("truncate stale tmp {}", tmp.display()))?;
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "total timeout exceeded after {} attempts (cap {}s)",
                attempt - 1,
                opts.total_timeout.as_secs()
            ));
        }

        let outcome = stream_attempt(
            client,
            url,
            tmp,
            already,
            expected_total,
            opts,
            &mut progress,
            metrics,
        )
        .await;
        match outcome {
            AttemptOutcome::Done => {
                // Re-check on disk; the server may have closed early.
                let now_size = tokio::fs::metadata(tmp).await.map(|m| m.len()).unwrap_or(0);
                if now_size == expected_total {
                    return Ok(());
                }
                metrics.record_retry();
                tracing::warn!(
                    attempt,
                    have = now_size,
                    want = expected_total,
                    "underrun: server closed before total reached, retrying"
                );
            }
            AttemptOutcome::Retry(e) => {
                metrics.record_retry();
                tracing::warn!(
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "transport error; retrying after backoff"
                );
            }
            AttemptOutcome::RangeNotSatisfiable => {
                if range_416_consumed {
                    return Err(anyhow!(
                        "GET {url} returned 416 Range Not Satisfiable twice — giving up"
                    ));
                }
                range_416_consumed = true;
                tracing::warn!(
                    attempt,
                    "server returned 416 — wiping tmp and retrying from offset 0"
                );
                tokio::fs::File::create(tmp)
                    .await
                    .with_context(|| format!("truncate after 416 {}", tmp.display()))?;
                progress = ProgressState::new(0);
                continue; // no backoff
            }
            AttemptOutcome::Fatal(e) => return Err(e.context("download fatal")),
        }

        if attempt == opts.max_attempts {
            return Err(anyhow!(
                "exhausted {} retries downloading {}",
                opts.max_attempts,
                url
            ));
        }
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(anyhow!(
                "total timeout exceeded after {} attempts (cap {}s)",
                attempt,
                opts.total_timeout.as_secs()
            ));
        }
        let sleep = backoff.min(remaining);
        tokio::time::sleep(sleep).await;
        backoff = (backoff * 2).min(opts.max_backoff);
    }

    Err(anyhow!(
        "exhausted {} retries downloading {}",
        opts.max_attempts,
        url
    ))
}

/// Verify final size, fsync, atomically rename `tmp` → `dest`.
async fn finalize(tmp: &Path, dest: &Path, expected_total: u64) -> Result<()> {
    let actual = tokio::fs::metadata(tmp)
        .await
        .with_context(|| format!("stat tmp {}", tmp.display()))?
        .len();
    if actual != expected_total {
        return Err(anyhow!(
            "tmp size {} != expected total {} after retries; aborting rename",
            actual,
            expected_total
        ));
    }
    // sync_all the tmp before rename so a crash post-rename can't leave a
    // zero-length dest.
    let f = tokio::fs::OpenOptions::new()
        .read(true)
        .open(tmp)
        .await
        .with_context(|| format!("open tmp for fsync {}", tmp.display()))?;
    f.sync_all()
        .await
        .with_context(|| format!("fsync tmp {}", tmp.display()))?;
    drop(f);
    tokio::fs::rename(tmp, dest)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt as TokioRead;
    use tokio::io::AsyncWriteExt as TokioWrite;
    use tokio::net::TcpListener;

    // ----- pure unit tests on the helpers --------------------------------

    #[test]
    fn tmp_path_appends_dot_tmp() {
        let p = tmp_path_for(Path::new("/x/y/chrome-mac-arm64.zip")).unwrap();
        assert_eq!(p, PathBuf::from("/x/y/chrome-mac-arm64.zip.tmp"));
    }

    #[test]
    fn is_retryable_classifies_status_buckets() {
        // We rely on these StatusCode buckets in `is_reqwest_error_retryable`
        // and the `stream_attempt` status-handling branch. If a future
        // reqwest version reshuffles them, this test will catch it.
        assert!(reqwest::StatusCode::from_u16(503)
            .unwrap()
            .is_server_error());
        assert!(reqwest::StatusCode::from_u16(404)
            .unwrap()
            .is_client_error());
        assert_eq!(reqwest::StatusCode::from_u16(429).unwrap().as_u16(), 429);
        assert_eq!(reqwest::StatusCode::from_u16(408).unwrap().as_u16(), 408);
    }

    // ----- mock HTTP server fixtures ------------------------------------

    #[derive(Debug, Clone)]
    enum Behavior {
        Ok,
        DropOnceMidstream { drop_after_bytes: u64 },
        StallForever,
        Always503,
        Range416Once,
    }

    struct Mock {
        addr: SocketAddr,
        attempts: Arc<AtomicU32>,
    }

    impl Mock {
        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }
        fn attempt_count(&self) -> u32 {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    async fn spawn_mock(body: Vec<u8>, behavior: Behavior) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let body_arc = Arc::new(body);
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let body = body_arc.clone();
                let counter = counter.clone();
                let beh = behavior.clone();
                tokio::spawn(async move {
                    let req = match read_request(&mut sock).await {
                        Some(r) => r,
                        None => return,
                    };
                    let range_start = parse_range_start(&req).unwrap_or(0);
                    let is_head = req.starts_with("HEAD ");
                    if is_head {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }

                    // Count body-serving requests only. `download_zip` HEADs the
                    // URL first to size it, and counting that probe as attempt 1
                    // meant every `n == 1` behaviour below fired against the HEAD
                    // — which returns above — and never against the real GET.
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;

                    match beh {
                        Behavior::Ok => {
                            serve_body(&mut sock, &body, range_start, None).await;
                        }
                        Behavior::DropOnceMidstream { drop_after_bytes } => {
                            if n == 1 {
                                serve_body(&mut sock, &body, range_start, Some(drop_after_bytes))
                                    .await;
                            } else {
                                serve_body(&mut sock, &body, range_start, None).await;
                            }
                        }
                        Behavior::StallForever => {
                            let headers = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = sock.write_all(headers.as_bytes()).await;
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                        Behavior::Always503 => {
                            let resp = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        Behavior::Range416Once => {
                            if n == 1 {
                                let resp = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                                let _ = sock.write_all(resp.as_bytes()).await;
                            } else {
                                serve_body(&mut sock, &body, 0, None).await;
                            }
                        }
                    }
                });
            }
        });
        Mock { addr, attempts }
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<String> {
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).await.ok()?;
        Some(String::from_utf8_lossy(&buf[..n]).to_string())
    }

    fn parse_range_start(req: &str) -> Option<u64> {
        for line in req.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("range:") {
                let rest = rest.trim();
                let rest = rest.strip_prefix("bytes=").unwrap_or(rest);
                let start = rest.split('-').next()?;
                return start.trim().parse::<u64>().ok();
            }
        }
        None
    }

    async fn serve_body(
        sock: &mut tokio::net::TcpStream,
        body: &[u8],
        range_start: u64,
        drop_after: Option<u64>,
    ) {
        let total = body.len() as u64;
        let start = range_start.min(total);
        let payload = &body[start as usize..];
        let status = if range_start > 0 {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let mut headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
            payload.len()
        );
        if range_start > 0 {
            headers.push_str(&format!(
                "Content-Range: bytes {}-{}/{}\r\n",
                start,
                total - 1,
                total
            ));
        }
        headers.push_str("\r\n");
        if sock.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        let limit = drop_after
            .map(|d| (d as usize).min(payload.len()))
            .unwrap_or(payload.len());
        for chunk in payload[..limit].chunks(8192) {
            if sock.write_all(chunk).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn body_pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn fast_opts() -> DownloadOptions {
        DownloadOptions {
            head_timeout: Duration::from_secs(2),
            stall_timeout: Duration::from_millis(300),
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(20),
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            max_attempts: 8,
            progress: false,
            progress_interval_bytes: 0,
            ..DownloadOptions::default()
        }
    }

    fn read_file(p: &Path) -> Vec<u8> {
        let mut f = std::fs::File::open(p).unwrap();
        let mut v = Vec::new();
        f.read_to_end(&mut v).unwrap();
        v
    }

    // ----- tests over the mock server -----------------------------------

    #[tokio::test]
    async fn single_stream_full_download() {
        let body = body_pattern(64 * 1024 + 17);
        let mock = spawn_mock(body.clone(), Behavior::Ok).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        download_zip(&mock.url("/x"), &dest, &fast_opts())
            .await
            .unwrap();
        assert_eq!(read_file(&dest), body);
        assert!(!dest.with_file_name("file.zip.tmp").exists());
    }

    #[tokio::test]
    async fn resumes_after_dropped_connection() {
        let body = body_pattern(50_000);
        let mock = spawn_mock(
            body.clone(),
            Behavior::DropOnceMidstream {
                drop_after_bytes: 20_000,
            },
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        download_zip(&mock.url("/y"), &dest, &fast_opts())
            .await
            .unwrap();
        // The file ending up correct AT ALL after a mid-stream drop is the
        // proof that resume worked — that requires both a partial GET and
        // a follow-up GET. Be tolerant about the exact server-hit count
        // (HEAD + ≥2 GETs nominally, but reqwest may consolidate).
        assert_eq!(read_file(&dest), body);
        assert!(
            mock.attempt_count() >= 2,
            "expected ≥2 server hits, got {}",
            mock.attempt_count()
        );
    }

    #[tokio::test]
    async fn honors_206_partial_content() {
        let body = body_pattern(40_000);
        let mock = spawn_mock(body.clone(), Behavior::Ok).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        let tmp = dest.with_file_name("file.zip.tmp");
        // Pre-seed half the body in the tmp file. The downloader should
        // send Range: bytes=20000- and the mock should return 206.
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(&body[..20_000])
            .unwrap();
        download_zip(&mock.url("/z"), &dest, &fast_opts())
            .await
            .unwrap();
        assert_eq!(read_file(&dest), body);
    }

    #[tokio::test]
    async fn stall_watchdog_aborts_idle_stream() {
        // Headers OK, but body never arrives → stall_timeout fires.
        let body = body_pattern(10_000);
        let mock = spawn_mock(body, Behavior::StallForever).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        let opts = DownloadOptions {
            stall_timeout: Duration::from_millis(150),
            max_attempts: 3,
            total_timeout: Duration::from_secs(2),
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            head_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(2),
            progress: false,
            progress_interval_bytes: 0,
            ..DownloadOptions::default()
        };
        let err = download_zip(&mock.url("/s"), &dest, &opts)
            .await
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("exhausted") || msg.contains("total timeout") || msg.contains("stall"),
            "unexpected error: {msg}"
        );
        assert!(mock.attempt_count() >= 2);
    }

    #[tokio::test]
    async fn total_timeout_returns_error() {
        let body = body_pattern(10_000);
        let mock = spawn_mock(body, Behavior::Always503).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        let opts = DownloadOptions {
            head_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_millis(400),
            stall_timeout: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(100),
            max_attempts: 100,
            connect_timeout: Duration::from_secs(2),
            progress: false,
            progress_interval_bytes: 0,
            ..DownloadOptions::default()
        };
        let err = download_zip(&mock.url("/t"), &dest, &opts)
            .await
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("total timeout") || msg.contains("exhausted"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn range_416_wipes_tmp_and_retries_once() {
        let body = body_pattern(8_000);
        let mock = spawn_mock(body.clone(), Behavior::Range416Once).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        let tmp = dest.with_file_name("file.zip.tmp");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::File::create(&tmp)
            .unwrap()
            .write_all(&body[..2_000])
            .unwrap();
        download_zip(&mock.url("/r"), &dest, &fast_opts())
            .await
            .unwrap();
        assert_eq!(read_file(&dest), body);
    }

    #[tokio::test]
    async fn metrics_record_retry_and_completion() {
        // The fetch_metrics() singleton is process-wide and other parallel
        // tests in this module also drive it. We only assert that this call
        // contributes a strictly positive delta to the relevant counters.
        let body = body_pattern(50_000);
        let mock = spawn_mock(
            body.clone(),
            Behavior::DropOnceMidstream {
                drop_after_bytes: 20_000,
            },
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("file.zip");
        let before = fetch_metrics().snapshot();
        download_zip(&mock.url("/m"), &dest, &fast_opts())
            .await
            .unwrap();
        let after = fetch_metrics().snapshot();
        assert!(after.attempts > before.attempts, "attempts didn't increase");
        assert!(
            after.retries > before.retries,
            "retries didn't increase across a known mid-stream drop"
        );
        assert!(
            after.download_completions > before.download_completions,
            "download_completions didn't increase"
        );
        assert!(after.bytes_written >= before.bytes_written + 50_000);
    }
}
