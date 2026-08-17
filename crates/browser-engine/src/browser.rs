//! Per-session [`Browser`]. Owns one Chromium child + one CDP connection +
//! the focus-restore guardian. **One `Browser` per session** per SPEC D2/D3.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cdp_client::{generated::domains::browser as cdp_browser, Connection};
use observability::trace::TraceSink;
use observability::CdpMetricsSink;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::process::Child;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use focus_manager::SpawnMode;

use crate::context::{BrowserContext, ContextId};
use crate::network::ProxyConfig;

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

/// User-supplied configuration for [`Browser::launch`]. Defaults satisfy
/// the spec's "headless by default" rule.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub binary: PathBuf,
    pub user_data_dir: PathBuf,
    pub mode: SpawnMode,
    pub extra_args: Vec<OsString>,
    /// SPEC §11 V3 — when set, Chromium is spawned via
    /// `/usr/bin/sandbox-exec -f <profile> -- <binary> ...` so the agent
    /// is confined to the per-session rootfs.
    ///
    /// `None` skips the wrap and behaves identically to pre-V3 — used by
    /// integration tests, the focus-manager spawn path, and any caller
    /// that already runs inside its own confinement.
    pub sandbox_profile: Option<PathBuf>,
    /// SPEC §11 V3 V-R1 — optional seed plan staged by the broker when the
    /// Chrome profile clone path is unavailable. When present, `Browser::launch`
    /// must apply it before the session is exposed.
    pub seed_plan_path: Option<PathBuf>,
    /// SPEC §12 U3 — `net.proxy`. When `Some`, `Browser::launch`
    /// appends `--proxy-server=<scheme>://<host>:<port>` (and
    /// `--proxy-bypass-list=…` when configured) to the Chromium argv.
    pub proxy: Option<ProxyConfig>,
}

impl BrowserConfig {
    pub fn new_headless(binary: PathBuf, user_data_dir: PathBuf) -> Self {
        Self {
            binary,
            user_data_dir,
            mode: SpawnMode::Headless,
            extra_args: Vec::new(),
            sandbox_profile: None,
            seed_plan_path: None,
            proxy: None,
        }
    }
}

/// Per-session Chromium driver. Cheap to clone (Arc internally).
#[derive(Clone)]
pub struct Browser {
    inner: Arc<BrowserInner>,
}

struct BrowserInner {
    /// CDP transport from `cdp-client`. Wires the actor pair onto the parent
    /// ends of the pipes we set up in `spawn_with_pipe_fds` below.
    cdp: Connection,
    /// Owns the spawned `tokio::process::Child` and SIGKILLs it on drop /
    /// explicit `kill()`. `cdp-client` does not own the child here — we keep
    /// spawn ownership so we can layer `pre_exec` (RLIMIT_AS, RLIMIT_CPU) and
    /// focus-restore around it.
    child: Arc<CdpChild>,
    /// Locked at launch — controls whether realistic input defaults to on.
    mode: SpawnMode,
    /// 1:1 with session in v1 (SPEC D2 reconciles `context_id == session_id`).
    /// Held inside an `Mutex<Option<...>>` so shutdown is idempotent.
    default_context: Mutex<Option<Arc<BrowserContext>>>,
    /// `closed_rx.recv().await => ()` resolves when the CDP reader task exits
    /// (i.e. Chromium has died or the pipe was closed).
    closed_rx: Mutex<Option<mpsc::Receiver<()>>>,
    /// SPEC §10 M10 — when `Some`, every CDP call routed through
    /// [`crate::Page::cdp_call`] emits `cdp_request`/`cdp_response` records
    /// to this sink. The broker installs the sink at session register-time
    /// when `trace=true`. None means trace recording is off (zero overhead).
    trace_sink: Mutex<Option<Arc<dyn TraceSink>>>,
    /// SPEC §12 U3 — staged proxy config for this Browser launch, if any.
    /// Stored so page/bootstrap and Fetch auth handling can inspect whether
    /// proxy authentication was requested.
    proxy_config: Option<ProxyConfig>,
    /// Per-method CDP latency histograms + outcome counters. Attached to
    /// the `Connection` at launch so every send (root + every attached
    /// target) records into the same sink. Surfaced via
    /// [`Browser::cdp_metrics`] for `_internal.metrics`.
    cdp_metrics: Arc<CdpMetricsSink>,
    /// SPEC §11 V3 V-R1 — optional bootstrap script that seeds
    /// `sessionStorage` on first navigation to matching origins. Stored on
    /// the Browser so every newly-opened page can install it before any page
    /// scripts run.
    session_storage_seed_script: Mutex<Option<String>>,
}

/// Newtype around `tokio::process::Child` that ensures Chromium gets killed
/// when this connection drops, even if other clones are still around.
///
/// Lives in browser-engine (not cdp-client) because this crate owns Chromium
/// spawn — see `spawn_with_pipe_fds` below for the rationale.
pub struct CdpChild {
    child: Mutex<Option<Child>>,
}

impl CdpChild {
    pub async fn kill(&self) {
        let taken = self.child.lock().take();
        if let Some(mut c) = taken {
            let _ = c.kill().await;
        }
    }
}

impl Drop for CdpChild {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().take() {
            // We're in a sync drop path; do a non-blocking kill via start_kill
            // and let the OS reap.
            let _ = c.start_kill();
        }
    }
}

impl Browser {
    /// Launch Chromium without stealing focus, wire up CDP, and return.
    ///
    /// On macOS, if `mode == SpawnMode::Headed`, this still uses the layered
    /// defense from SPEC §5; the caller does not need to do anything extra.
    pub async fn launch(config: BrowserConfig) -> Result<Browser, LaunchError> {
        // Build pipes for fd 3 (Chromium reads, we write) and fd 4 (Chromium
        // writes, we read), per SPEC D4. `make_pipe` returns the pair as
        // `(read_fd, write_fd)` matching `libc::pipe` semantics.
        //
        //   pipe1: chromium reads on fd 3, we write to it.
        //     read end  → dup2'd onto child fd 3 (`chromium_read_fd`)
        //     write end → kept by parent (`parent_writes_to_chromium_read`)
        //
        //   pipe2: chromium writes on fd 4, we read from it.
        //     read end  → kept by parent (`parent_reads_from_chromium_write`)
        //     write end → dup2'd onto child fd 4 (`chromium_write_fd`)
        let (chromium_read_fd, parent_writes_to_chromium_read) = make_pipe()?;
        let (parent_reads_from_chromium_write, chromium_write_fd) = make_pipe()?;

        // Hand the two raw fds to focus-manager via a fd-mapping helper. We
        // need them at fd 3 and 4 in the child; tokio::process::Command does
        // not expose raw fd remap, so we use a small unix helper.
        // SPEC §12 U3 — append `--proxy-server` (+ optional bypass list)
        // when a proxy is configured. We splice into `extra_args` so the
        // existing `build_argv` path picks it up and pre_exec/sandbox
        // wrappers stay untouched.
        let mut effective_extra: Vec<OsString> = config.extra_args.clone();
        if let Some(proxy) = config.proxy.as_ref() {
            effective_extra.push(OsString::from(proxy.to_proxy_server_arg()));
            if let Some(bypass) = proxy.bypass.as_deref() {
                effective_extra.push(OsString::from(format!("--proxy-bypass-list={bypass}")));
            }
        }
        let argv = focus_manager::spawn_flags::build_argv(
            config.mode,
            &config.user_data_dir,
            &effective_extra,
        );

        // SPEC §11 V3 — if a sandbox profile is set, swap the binary for
        // `/usr/bin/sandbox-exec` and prepend `["-f", profile, "--",
        // <orig_bin>]`. The pre_exec fd-3/4 dup2 + RLIMIT setup still runs
        // in the immediate child (sandbox-exec); sandbox-exec then exec()s
        // Chromium without resetting fds, so CDP keeps working.
        let (effective_binary, effective_argv) = match config.sandbox_profile.as_deref() {
            Some(profile) => sandbox::wrap_argv(profile, &config.binary, &argv)
                .map_err(|e| LaunchError::Other(anyhow!("sandbox::wrap_argv: {e}")))?,
            None => (config.binary.clone(), argv),
        };

        let child = unsafe {
            spawn_with_pipe_fds(
                &effective_binary,
                &effective_argv,
                chromium_read_fd,  // becomes fd 3 in child (chromium reads)
                chromium_write_fd, // becomes fd 4 in child (chromium writes)
            )
            .map_err(LaunchError::Other)?
        };

        // The child has now inherited dup'd copies of `chromium_read_fd` and
        // `chromium_write_fd` on its fd 3 / fd 4. Our parent-side originals
        // must be closed: leaving them open prevents EOF propagation when the
        // child exits (a closed-pipe `read` only returns 0 once *all* writers
        // are gone — so a leaked write end here would mask Chromium's death
        // forever, defeating the `closed_rx` shutdown signal below).
        unsafe {
            libc::close(chromium_read_fd);
            libc::close(chromium_write_fd);
        }

        // Run focus-restore for headed mode.
        if config.mode == SpawnMode::Headed {
            #[cfg(target_os = "macos")]
            if let Some(captured) = focus_manager::macos::frontmost_app() {
                focus_manager::restore::spawn_restore_task(
                    captured,
                    focus_manager::FOCUS_RESTORE_WINDOW,
                );
            }
        }

        // Convert the parent fds into async pipe halves and hand them to
        // cdp-client. We retain ownership of `child` separately via
        // [`CdpChild`] because cdp-client's `Connection` only takes the
        // pipe halves — process lifecycle stays our responsibility.
        let (read_half, write_half) = pipe_halves_from_fds(
            parent_reads_from_chromium_write,
            parent_writes_to_chromium_read,
        )
        .map_err(LaunchError::Other)?;
        let (cdp, closed_rx) = Connection::from_pipe_halves(read_half, write_half);
        let child_arc = Arc::new(CdpChild {
            child: Mutex::new(Some(child)),
        });

        // Build the per-method CDP metrics sink and attach it to the
        // connection BEFORE the handshake send so the very first
        // `Browser.getVersion` is recorded. The sink propagates to every
        // session created later via `Target.attachedToTarget`.
        let cdp_metrics = Arc::new(CdpMetricsSink::new());
        cdp.with_metrics_sink(Some(
            Arc::clone(&cdp_metrics) as Arc<dyn cdp_client::MetricsSink>
        ));

        let browser = Browser {
            inner: Arc::new(BrowserInner {
                cdp,
                child: child_arc,
                mode: config.mode,
                default_context: Mutex::new(None),
                closed_rx: Mutex::new(Some(closed_rx)),
                trace_sink: Mutex::new(None),
                proxy_config: config.proxy.clone(),
                cdp_metrics,
                session_storage_seed_script: Mutex::new(None),
            }),
        };

        // Verify the CDP pipe is alive by issuing `Browser.getVersion`. The
        // command is codegen-marked `IDEMPOTENT = true`, so we use
        // `send_with_retry` to ride out a transient EBADF/pipe flap on the
        // freshly-opened pipe. The outer 5-second timeout still bounds the
        // total handshake wall-clock; the retry policy caps at 3 attempts
        // (50→100ms backoff capped at 200ms).
        match tokio::time::timeout(
            Duration::from_secs(5),
            browser
                .inner
                .cdp
                .root_session()
                .send_with_retry(cdp_browser::GetVersionParams::default()),
        )
        .await
        {
            Ok(Ok(v)) => {
                info!(
                    product = %v.product,
                    revision = %v.revision,
                    user_agent = %v.user_agent,
                    "chromium handshake ok"
                );
            }
            Ok(Err(e)) => {
                browser.inner.child.kill().await;
                return Err(LaunchError::Other(
                    anyhow::Error::new(e).context("Browser.getVersion handshake"),
                ));
            }
            Err(_) => {
                browser.inner.child.kill().await;
                return Err(LaunchError::Other(anyhow!(
                    "timed out waiting for Browser.getVersion"
                )));
            }
        }

        if let Some(seed_plan_path) = config.seed_plan_path.as_deref() {
            apply_seed_plan(&browser, seed_plan_path)
                .await
                .map_err(|e| LaunchError::Other(e.context("apply V-R1 seed plan")))?;
        }

        Ok(browser)
    }

    /// CDP transport handle. Cheap to clone (Arc internally).
    pub fn cdp(&self) -> &Connection {
        &self.inner.cdp
    }

    /// Per-method CDP metrics sink for this Browser. Cheap to clone (Arc).
    /// The broker reads `cdp_metrics().snapshot()` for `_internal.metrics`.
    pub fn cdp_metrics(&self) -> Arc<CdpMetricsSink> {
        Arc::clone(&self.inner.cdp_metrics)
    }

    /// Spawn mode used to launch this Chromium. Powers SPEC §10 M6 — the
    /// default realistic-input policy depends on whether we're headless or
    /// headed.
    pub fn mode(&self) -> SpawnMode {
        self.inner.mode
    }

    /// SPEC §10 M6 — default value for `page.click {realistic}` /
    /// `page.type {realistic}`. Headless = false (fast), headed = true
    /// (defeats bot detection that watches mouse paths).
    pub fn default_realistic(&self) -> bool {
        matches!(self.inner.mode, SpawnMode::Headed)
    }

    /// SPEC §10 M10 — install a trace sink. Once attached every CDP call
    /// routed through [`crate::Page::cdp_call`] emits `cdp_request` /
    /// `cdp_response` records and every action's screenshot is saved.
    /// Pass `None` to detach.
    pub fn attach_trace_sink(&self, sink: Option<Arc<dyn TraceSink>>) {
        *self.inner.trace_sink.lock() = sink;
    }

    /// True if a trace sink is currently attached.
    pub fn trace_enabled(&self) -> bool {
        self.inner.trace_sink.lock().is_some()
    }

    /// SPEC §10 M10 — currently-attached sink, if any. Cheap clone (Arc).
    pub fn trace_sink(&self) -> Option<Arc<dyn TraceSink>> {
        self.inner.trace_sink.lock().clone()
    }

    /// OS process id of the owned Chromium child, if it is still live.
    pub fn pid(&self) -> Option<u32> {
        self.inner
            .child
            .child
            .lock()
            .as_ref()
            .and_then(tokio::process::Child::id)
    }

    /// SPEC §12 U3 — proxy config supplied at Browser launch, if any.
    pub fn proxy_config(&self) -> Option<ProxyConfig> {
        self.inner.proxy_config.clone()
    }

    /// SPEC §11 V3 V-R1 — bootstrap script that seeds `sessionStorage`
    /// on first navigation to matching origins, if any was staged.
    pub fn session_storage_seed_script(&self) -> Option<String> {
        self.inner.session_storage_seed_script.lock().clone()
    }

    /// SPEC §11 V3 V-R1 — install or clear the sessionStorage bootstrap
    /// script shared by every page in this Browser.
    pub fn set_session_storage_seed_script(&self, script: Option<String>) {
        *self.inner.session_storage_seed_script.lock() = script;
    }

    /// Default context, lazily created on first call. In v1 (per SPEC D2)
    /// this is 1:1 with the session — there's no separate
    /// `Target.createBrowserContext` step because storage is process-isolated.
    pub fn default_context(&self) -> Arc<BrowserContext> {
        let mut g = self.inner.default_context.lock();
        if let Some(c) = g.as_ref() {
            return Arc::clone(c);
        }
        let ctx = Arc::new(BrowserContext::new_default(self.clone()));
        *g = Some(Arc::clone(&ctx));
        ctx
    }

    /// Returns a future that resolves when the Chromium child has exited.
    pub fn wait_for_exit(&self) -> Option<mpsc::Receiver<()>> {
        self.inner.closed_rx.lock().take()
    }

    /// Graceful shutdown per SPEC §3 drop order: send `Browser.close`, wait
    /// up to 5s, then SIGKILL.
    pub async fn shutdown(&self) -> Result<()> {
        debug!("browser shutdown: sending Browser.close");
        let _ = self
            .inner
            .cdp
            .root_session()
            .send(cdp_browser::CloseParams::default())
            .await;
        // Take the rx out of the mutex BEFORE entering the async block, so
        // the parking_lot guard doesn't cross any .await point.
        let rx_opt = self.inner.closed_rx.lock().take();
        match tokio::time::timeout(Duration::from_secs(5), async move {
            if let Some(mut rx) = rx_opt {
                let _ = rx.recv().await;
            }
        })
        .await
        {
            Ok(()) => {
                debug!("chromium exited gracefully");
            }
            Err(_) => {
                warn!("chromium did not exit within 5s, killing");
                self.inner.child.kill().await;
            }
        }
        Ok(())
    }
}

/// Create a unix pipe and return the (read_fd, write_fd) pair. Caller owns
/// both fds; once handed to a child via `spawn_with_pipe_fds`, the child's
/// fd is consumed and closed in our process.
fn make_pipe() -> Result<(RawFd, RawFd), LaunchError> {
    let (r, w) = nix_pipe().map_err(LaunchError::Io)?;
    Ok((r, w))
}

#[cfg(unix)]
fn nix_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() writes into the array of two ints; we then validate.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Set CLOEXEC on both — pre_exec on the child will clear CLOEXEC on the
    // two we hand to it (fd 3 and 4) after dup2.
    for &fd in &fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    Ok((fds[0], fds[1]))
}

#[cfg(not(unix))]
fn nix_pipe() -> std::io::Result<(RawFd, RawFd)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "pipes not supported off-unix",
    ))
}

/// Convert raw parent-side pipe fds into async halves suitable for
/// `cdp_client::Connection::from_pipe_halves`.
///
/// We use `tokio::net::unix::pipe::{Receiver, Sender}` (not `tokio::fs::File`)
/// because pipes register with the runtime as a non-blocking pollable, which
/// matches what cdp-client expects from its pipe-mode tests.
#[cfg(unix)]
fn pipe_halves_from_fds(
    read_fd: RawFd,
    write_fd: RawFd,
) -> Result<(
    tokio::net::unix::pipe::Receiver,
    tokio::net::unix::pipe::Sender,
)> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: we own the fds (created by libc::pipe in this process).
    let read_owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(read_fd) };
    let write_owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(write_fd) };
    let read = tokio::net::unix::pipe::Receiver::from_owned_fd(read_owned)
        .context("wrap parent read fd as tokio pipe Receiver")?;
    let write = tokio::net::unix::pipe::Sender::from_owned_fd(write_owned)
        .context("wrap parent write fd as tokio pipe Sender")?;
    Ok((read, write))
}

#[cfg(not(unix))]
fn pipe_halves_from_fds(
    _read_fd: RawFd,
    _write_fd: RawFd,
) -> Result<(tokio::io::DuplexStream, tokio::io::DuplexStream)> {
    Err(anyhow!("pipes not supported off-unix"))
}

fn encode_js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn js_origin(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &url[scheme_end + 3..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    Some(format!("{}://{}", scheme, &rest[..host_end]))
}

/// Validate that a user-supplied navigation target is safe for `tab.open` /
/// `tab.navigate`. Public so the broker can fast-fail before any Chromium work,
/// while browser-engine still enforces the same policy at its own boundary.
pub fn validate_navigable_url(url: &str) -> Result<()> {
    if url == "about:blank" {
        return Ok(());
    }

    let Some((scheme, remainder)) = url.split_once(':') else {
        return Err(anyhow!("navigation URL must include an explicit scheme"));
    };
    let scheme = scheme.to_ascii_lowercase();

    match scheme.as_str() {
        "http" | "https" => {
            if !remainder.starts_with("//") {
                return Err(anyhow!(
                    "navigation URL must use an absolute {scheme}:// URL"
                ));
            }
            Ok(())
        }
        "about" => Err(anyhow!("only about:blank is allowed for navigation")),
        "file" | "chrome" | "chrome-error" | "chrome-extension" | "javascript" | "data"
        | "view-source" | "devtools" => Err(anyhow!("unsafe navigation URL scheme: {scheme}")),
        _ => Err(anyhow!("unsupported navigation URL scheme: {scheme}")),
    }
}

fn storage_origin(origin: &str) -> Option<String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() || trimmed == "null" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn grouped_storage_entries(
    entries: &[sandbox::StorageEntry],
    kind: &str,
) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for entry in entries {
        if entry.kind != kind {
            continue;
        }
        if let Some(origin) = storage_origin(&entry.origin) {
            out.entry(origin)
                .or_default()
                .push((entry.key.clone(), entry.value.clone()));
        }
    }
    out
}

fn build_session_storage_seed_script(entries: &[sandbox::StorageEntry]) -> Option<String> {
    let grouped = grouped_storage_entries(entries, "session");
    if grouped.is_empty() {
        return None;
    }
    let map_json = serde_json::to_string(&grouped).ok()?;
    Some(format!(
        "(() => {{\n  if (window.__oneForAllSessionSeedInstalled) return;\n  window.__oneForAllSessionSeedInstalled = true;\n  const seeds = {map_json};\n  const apply = () => {{\n    try {{\n      const origin = location.origin;\n      const entries = seeds[origin];\n      if (!entries) return;\n      for (const [k, v] of entries) sessionStorage.setItem(k, v);\n      delete seeds[origin];\n    }} catch (_e) {{}}\n  }};\n  apply();\n  addEventListener('DOMContentLoaded', apply, {{ once: true }});\n}})();"
    ))
}

#[derive(Debug, Clone, Serialize)]
struct IndexedDbStoreSeed {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_path: Option<Value>,
    auto_increment: bool,
    records: Vec<IndexedDbRecordSeed>,
}

#[derive(Debug, Clone, Serialize)]
struct IndexedDbRecordSeed {
    key: Value,
    value: Value,
}

#[derive(Debug, Clone, Serialize)]
struct IndexedDbDatabaseSeed {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    stores: Vec<IndexedDbStoreSeed>,
}

async fn apply_seed_plan(browser: &Browser, seed_plan_path: &Path) -> Result<()> {
    let Some(plan) = sandbox::read_seed_plan(seed_plan_path.parent().unwrap_or(seed_plan_path))?
    else {
        return Ok(());
    };
    if plan.is_empty() {
        let _ = std::fs::remove_file(seed_plan_path);
        return Ok(());
    }

    let session_storage_script = build_session_storage_seed_script(&plan.storage);
    browser.set_session_storage_seed_script(session_storage_script);

    if !plan.cookies.is_empty() {
        apply_seed_cookies(browser, &plan.cookies).await?;
    }
    if !plan.storage.is_empty() {
        apply_seed_local_storage(browser, &plan.storage).await?;
    }
    if !plan.indexed_db.is_empty() {
        apply_seed_indexed_db(browser, &plan.indexed_db).await?;
    }
    if !plan.service_workers.is_empty() {
        apply_seed_service_workers(browser, &plan.service_workers).await?;
    }
    if !plan.cache_storage.is_empty() {
        apply_seed_cache_storage(browser, &plan.cache_storage).await?;
    }

    std::fs::remove_file(seed_plan_path).map_err(|e| {
        anyhow!(
            "remove consumed seed plan {}: {e}",
            seed_plan_path.display()
        )
    })?;
    Ok(())
}

async fn apply_seed_cookies(browser: &Browser, cookies: &[sandbox::CookieRecord]) -> Result<()> {
    use cdp_client::generated::domains::network as cdp_network;

    let cookie_params: Vec<Value> = cookies
        .iter()
        .map(|c| {
            crate::cookies::Cookie {
                name: c.name.clone(),
                value: c.value.clone(),
                domain: c.domain.clone(),
                path: c.path.clone(),
                expires: c.expires.unwrap_or(-1.0),
                size: 0,
                http_only: c.http_only,
                secure: c.secure,
                session: c.expires.is_none(),
                same_site: c.same_site.clone(),
            }
            .to_cdp_param()
        })
        .collect();
    browser
        .cdp()
        .root_session()
        .send(cdp_network::SetCookiesParams {
            cookies: json!(cookie_params),
        })
        .await
        .context("Network.setCookies (V-R1 seed)")?;
    Ok(())
}

async fn apply_seed_local_storage(
    browser: &Browser,
    entries: &[sandbox::StorageEntry],
) -> Result<()> {
    let grouped = grouped_storage_entries(entries, "local");
    for (origin, kvs) in grouped {
        let page = open_bootstrap_page(browser, &origin).await?;
        let storage_id = json!({
            "securityOrigin": origin,
            "isLocalStorage": true,
        });
        page.cdp_call("DOMStorage.enable", None)
            .await
            .context("DOMStorage.enable (V-R1 localStorage seed)")?;
        for (key, value) in kvs {
            page.cdp_call(
                "DOMStorage.setDOMStorageItem",
                Some(json!({
                    "storageId": storage_id,
                    "key": key,
                    "value": value,
                })),
            )
            .await
            .context("DOMStorage.setDOMStorageItem (V-R1 localStorage seed)")?;
        }
        page.close()
            .await
            .context("close bootstrap page (localStorage seed)")?;
    }
    Ok(())
}

async fn apply_seed_indexed_db(
    browser: &Browser,
    records: &[sandbox::IndexedDbRecord],
) -> Result<()> {
    let grouped = build_indexed_db_seed(records)?;
    for (origin, databases) in grouped {
        let db_json =
            serde_json::to_string(&databases).context("serialize IndexedDB seed payload")?;
        let page = open_bootstrap_page(browser, &origin).await?;
        let expr = format!(
            r#"(async () => {{
  const databases = {db_json};
  for (const db of databases) {{
    await new Promise((resolve, reject) => {{
      const openReq = indexedDB.open(db.name, db.version || 1);
      openReq.onupgradeneeded = () => {{
        const idb = openReq.result;
        for (const store of db.stores) {{
          if (!idb.objectStoreNames.contains(store.name)) {{
            idb.createObjectStore(store.name, store.key_path === undefined ? undefined : {{ keyPath: store.key_path, autoIncrement: store.auto_increment }});
          }}
        }}
      }};
      openReq.onerror = () => reject(openReq.error || new Error('indexedDB.open failed'));
      openReq.onsuccess = () => {{
        const idb = openReq.result;
        const storeNames = db.stores.map((s) => s.name);
        const tx = idb.transaction(storeNames, 'readwrite');
        tx.onerror = () => reject(tx.error || new Error('indexedDB transaction failed'));
        tx.oncomplete = () => {{
          idb.close();
          resolve(undefined);
        }};
        for (const store of db.stores) {{
          const os = tx.objectStore(store.name);
          for (const rec of store.records) os.put(rec.value, rec.key);
        }}
      }};
    }});
  }}
  return 'ok';
}})()"#
        );
        let _ = page
            .eval(&expr, true)
            .await
            .context("apply IndexedDB seed")?;
        page.close()
            .await
            .context("close bootstrap page (IndexedDB seed)")?;
    }
    Ok(())
}

async fn apply_seed_service_workers(
    browser: &Browser,
    regs: &[sandbox::ServiceWorkerReg],
) -> Result<()> {
    let mut grouped: BTreeMap<String, Vec<&sandbox::ServiceWorkerReg>> = BTreeMap::new();
    for reg in regs {
        if let Some(origin) = js_origin(&reg.script_url).or_else(|| js_origin(&reg.scope)) {
            grouped.entry(origin).or_default().push(reg);
        }
    }
    for (origin, regs) in grouped {
        let regs_json =
            serde_json::to_string(&regs).context("serialize service worker seed payload")?;
        let page = open_bootstrap_page(browser, &origin).await?;
        let expr = format!(
            r#"(async () => {{
  const regs = {regs_json};
  for (const reg of regs) {{
    const opts = reg.scope ? {{ scope: reg.scope }} : undefined;
    await navigator.serviceWorker.register(reg.script_url, opts);
  }}
  return 'ok';
}})()"#
        );
        let _ = page
            .eval(&expr, true)
            .await
            .context("apply service worker seed")?;
        page.close()
            .await
            .context("close bootstrap page (service worker seed)")?;
    }
    Ok(())
}

async fn apply_seed_cache_storage(
    browser: &Browser,
    entries: &[sandbox::CacheStorageEntry],
) -> Result<()> {
    let mut grouped: BTreeMap<String, Vec<&sandbox::CacheStorageEntry>> = BTreeMap::new();
    for entry in entries {
        if let Some(origin) = storage_origin(&entry.origin) {
            grouped.entry(origin).or_default().push(entry);
        }
    }
    for (origin, entries) in grouped {
        let entries_json =
            serde_json::to_string(&entries).context("serialize cache storage seed payload")?;
        let page = open_bootstrap_page(browser, &origin).await?;
        let expr = format!(
            r#"(async () => {{
  const entries = {entries_json};
  for (const entry of entries) {{
    const cache = await caches.open(entry.cache_name);
    const headers = new Headers();
    for (const h of entry.response_headers || []) headers.append(h.name, h.value);
    const body = Uint8Array.from(atob(entry.response_body_b64), (c) => c.charCodeAt(0));
    const resp = new Response(body, {{ status: entry.response_status, statusText: entry.response_status_text || '', headers }});
    await cache.put(entry.request_url, resp);
  }}
  return 'ok';
}})()"#
        );
        let _ = page
            .eval(&expr, true)
            .await
            .context("apply CacheStorage seed")?;
        page.close()
            .await
            .context("close bootstrap page (cache storage seed)")?;
    }
    Ok(())
}

fn build_indexed_db_seed(
    records: &[sandbox::IndexedDbRecord],
) -> Result<BTreeMap<String, Vec<IndexedDbDatabaseSeed>>> {
    let mut grouped: BTreeMap<String, BTreeMap<String, BTreeMap<String, IndexedDbStoreSeed>>> =
        BTreeMap::new();
    let mut versions: BTreeMap<(String, String), Option<u64>> = BTreeMap::new();
    for record in records {
        let Some(origin) = storage_origin(&record.origin) else {
            continue;
        };
        let key_json = decode_b64_json_value(&record.key_b64).with_context(|| {
            format!(
                "decode IndexedDB key for {}:{}",
                record.database_name, record.object_store
            )
        })?;
        let value_json = decode_b64_json_value(&record.value_b64).with_context(|| {
            format!(
                "decode IndexedDB value for {}:{}",
                record.database_name, record.object_store
            )
        })?;
        versions.insert(
            (origin.clone(), record.database_name.clone()),
            record.database_version,
        );
        grouped
            .entry(origin)
            .or_default()
            .entry(record.database_name.clone())
            .or_default()
            .entry(record.object_store.clone())
            .or_insert_with(|| IndexedDbStoreSeed {
                name: record.object_store.clone(),
                key_path: None,
                auto_increment: false,
                records: Vec::new(),
            })
            .records
            .push(IndexedDbRecordSeed {
                key: key_json,
                value: value_json,
            });
    }

    let mut out: BTreeMap<String, Vec<IndexedDbDatabaseSeed>> = BTreeMap::new();
    for (origin, dbs) in grouped {
        let mut databases = Vec::with_capacity(dbs.len());
        for (db_name, stores) in dbs {
            let version = versions
                .get(&(origin.clone(), db_name.clone()))
                .copied()
                .flatten();
            databases.push(IndexedDbDatabaseSeed {
                name: db_name,
                version,
                stores: stores.into_values().collect(),
            });
        }
        out.insert(origin, databases);
    }
    Ok(out)
}

fn decode_b64_json_value(encoded: &str) -> Result<Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .context("base64 decode")?;
    let s = String::from_utf8(bytes).context("utf8 decode")?;
    serde_json::from_str(&s).context("json decode")
}

async fn open_bootstrap_page(browser: &Browser, origin: &str) -> Result<Arc<crate::Page>> {
    let page = browser
        .default_context()
        .open_tab(origin, crate::WaitUntil::Load)
        .await
        .with_context(|| format!("open bootstrap page for origin {origin}"))?;
    Ok(page)
}

/// Spawn `binary` with `argv`, passing two fds to the child as fd 3 and fd 4
/// (clearing CLOEXEC on them so they survive into the child). Returns a
/// `tokio::process::Child` whose stdio is inherited.
///
/// # Safety
/// Sets up fd remap via `pre_exec`. Runs in the child between fork and exec.
/// Must avoid allocator and locks (this keeps to libc dup2/fcntl).
unsafe fn spawn_with_pipe_fds(
    binary: &Path,
    argv: &[OsString],
    fd_for_child_3: RawFd,
    fd_for_child_4: RawFd,
) -> Result<tokio::process::Child> {
    #[allow(unused_imports)]
    use std::os::unix::process::CommandExt;
    use tokio::process::Command;

    let mut cmd = Command::new(binary);
    cmd.args(argv);
    cmd.kill_on_drop(true);
    // Ensure stdio is inherited; CDP traffic is on fd 3/4 not stdout/stderr.

    // SAFETY: pre_exec runs after fork, before exec. We dup2 our pipe fds
    // into 3 and 4, then clear CLOEXEC on them.
    cmd.pre_exec(move || {
        // SPEC §10 M9 — RLIMIT_AS hard cap, RLIMIT_CPU soft cap. These
        // run after fork, before exec. We deliberately don't fail the
        // spawn if rlimit can't be set — Chromium subsystems (e.g. GPU
        // process) sometimes need higher; Layer A/B (headless + offscreen)
        // still hold without these. Errors are best-effort logged at the
        // outer call site via the Child's stderr.
        //
        // Constants live in `sandbox::limits` so the SBPL profile audit
        // and this `setrlimit` site share one source of truth.
        let rlim_as = libc::rlimit {
            rlim_cur: sandbox::CHROMIUM_MEMORY_BYTES as libc::rlim_t,
            rlim_max: sandbox::CHROMIUM_MEMORY_BYTES as libc::rlim_t,
        };
        let _ = libc::setrlimit(libc::RLIMIT_AS, &rlim_as);
        let rlim_cpu = libc::rlimit {
            rlim_cur: sandbox::CHROMIUM_CPU_SECONDS_SOFT as libc::rlim_t,
            rlim_max: libc::RLIM_INFINITY,
        };
        let _ = libc::setrlimit(libc::RLIMIT_CPU, &rlim_cpu);

        if libc::dup2(fd_for_child_3, 3) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(fd_for_child_4, 4) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Clear CLOEXEC on 3/4.
        for fd in [3, 4] {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    });

    cmd.spawn().context("spawning chromium with pipe fds")
}

/// `crate::context::ContextId` re-export for convenience.
pub type DefaultContextId = ContextId;

// Note: this implementation purposely skips the `focus_manager::spawn_chromium_no_focus`
// helper because we need to interleave fd setup between fork and exec. The
// flag generation is reused via `focus_manager::spawn_flags::build_argv`, and
// the post-spawn restore is invoked manually above. Keeping this in
// `browser-engine` avoids leaking `pre_exec` semantics into the public API of
// `focus-manager`.

#[cfg(test)]
mod tests {
    use super::validate_navigable_url;

    #[test]
    fn validate_navigable_url_allows_safe_targets() {
        assert!(validate_navigable_url("https://example.com/").is_ok());
        assert!(validate_navigable_url("http://127.0.0.1:8080/test").is_ok());
        assert!(validate_navigable_url("about:blank").is_ok());
    }

    #[test]
    fn validate_navigable_url_rejects_unsafe_targets() {
        for url in [
            "data:text/html,hi",
            "javascript:alert(1)",
            "file:///tmp/x",
            "chrome://settings",
            "about:srcdoc",
            "mailto:test@example.com",
            "/relative/path",
        ] {
            assert!(
                validate_navigable_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }
}
