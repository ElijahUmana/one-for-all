//! Broker daemon entry point.
//!
//! Per SPEC D7 (opportunistic singleton via flock) and SPEC §3 (drop order
//! on shutdown).
//!
//! ## Why not `#[tokio::main]`?
//!
//! SPEC §5 Layer E requires the broker to call
//! `NSApplication::sharedApplication().setActivationPolicy(.accessory)` on
//! the process's main (initial) thread, BEFORE any tokio worker thread is
//! spawned. `#[tokio::main]` does technically run on the main thread, but
//! it composes poorly with synchronous-before-runtime AppKit setup: any
//! future refactor that moves Layer E behind an `await` would silently
//! move it off the main thread. We make the ordering explicit by hand-
//! rolling the runtime construction: Layer E first, runtime build second,
//! async body third.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tokio::signal;
use tracing::{error, info, warn};

use broker::lifecycle::IdleConfig;
use broker::server;
use broker::State;

fn main() -> Result<()> {
    // SPEC §11 V2 — `--check-ax` is an early-exit probe used by
    // installer/install.sh and installer/doctor.sh. It returns 0 if
    // `AXIsProcessTrusted()` is true, 1 otherwise. Pass `--prompt` to
    // additionally show the OS Accessibility-grant dialog (used by the
    // installer on first run; macOS shows the prompt at most once per
    // process, so re-runs are silent re-checks).
    let mut argv = std::env::args().skip(1);
    if let Some(arg) = argv.next() {
        if arg == "--check-ax" {
            let prompt = argv.any(|a| a == "--prompt");
            let trusted = if prompt {
                native_control::permission::ensure_trusted_with_prompt().is_ok()
            } else {
                native_control::permission::is_trusted()
            };
            if trusted {
                eprintln!("AX permission: granted");
                std::process::exit(0);
            } else {
                eprintln!(
                    "AX permission: missing — open {}",
                    native_control::permission::SETTINGS_DEEPLINK
                );
                std::process::exit(1);
            }
        }
    }

    // SPEC §5 Layer E (macOS only) — apply NSApplicationActivationPolicyAccessory
    // synchronously on the main thread before tokio takes over. The guard is
    // bound for the lifetime of `main` so the `MainThreadMarker` proof
    // outlives every AppKit-relevant call. On non-macOS targets this is an
    // inert no-op stub; the call is cross-platform with no `#[cfg]` gate at
    // the call site.
    let _accessory_guard = focus_manager::set_accessory_activation_policy()
        .context("SPEC §5 Layer E: applying NSApplicationActivationPolicy::Accessory")?;

    // SPEC §5 Layer E h2 — debug-only idempotency exercise.
    //
    // The runtime "already accessory" branch in
    // `focus_manager::set_accessory_activation_policy` is otherwise
    // exercised only by the doc-comment-described path. We expose it
    // here behind an env var (debug builds only) so an integration
    // test can spawn the broker and probe `lsappinfo` after a
    // double-apply. Production binaries built with
    // `--profile release` strip this entirely; even debug binaries
    // without the env var are unchanged.
    //
    // The double-apply is value-preserving: the second call observes
    // policy == Accessory and short-circuits AppKit work (see
    // `set_accessory_activation_policy`'s idempotency block). The
    // returned guard is dropped immediately — only the first guard
    // bound above keeps the `MainThreadMarker` lifetime live.
    #[cfg(all(target_os = "macos", debug_assertions))]
    if std::env::var_os("OFA_LAYER_E_DOUBLE_APPLY").is_some() {
        let _second = focus_manager::set_accessory_activation_policy()
            .context("SPEC §5 Layer E h2: idempotent double-apply (OFA_LAYER_E_DOUBLE_APPLY)")?;
        info!("SPEC §5 Layer E h2: double-apply ran; both calls returned Ok");
    }

    // Build the tokio runtime by hand so the AppKit init above is unambiguously
    // ordered before any tokio worker thread spawns. Mirrors the prior
    // `#[tokio::main(flavor = "multi_thread")]` configuration (multi-thread
    // runtime, all features enabled).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio multi-thread runtime")?;

    let result = runtime.block_on(async_main());

    // Hold the guard until after the runtime returns so the policy stays
    // live for the entire process lifetime; the explicit drop here makes
    // the intended lifetime visible to a reader.
    drop(_accessory_guard);

    result
}

async fn async_main() -> Result<()> {
    let _guard = observability::init("broker").context("observability::init")?;

    let socket_path = resolve_socket_path()?;
    let lock_path = resolve_lock_path()?;
    let user_data_root = resolve_user_data_root()?;
    let max_sessions = resolve_max_sessions().unwrap_or(broker::DEFAULT_MAX_SESSIONS);

    // SPEC D7: opportunistic singleton via flock(LOCK_EX | LOCK_NB).
    let _lock = match acquire_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                lock = %lock_path.display(),
                error = %e,
                "broker lock held by another process — exiting cleanly"
            );
            return Ok(());
        }
    };

    let listener = server::bind_socket(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), max_sessions, "broker listening");

    let state = State::new_with_caps(IdleConfig::default(), user_data_root, max_sessions);

    let server_task = {
        let state = std::sync::Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = server::run(state, listener).await {
                error!(error = %e, "server task exited with error");
            }
        })
    };

    // Wait for SIGTERM/SIGINT.
    wait_for_shutdown().await;
    info!("broker shutdown signal received; draining sessions");

    // SPEC §3 drop order:
    // 1. Stop accepting new connections (abort the server task).
    server_task.abort();
    // 2. Notify every session of broker.shutdown.
    for (sid, entry) in state.registry.iter() {
        let _ = entry.try_push(broker::ServerEvent {
            jsonrpc: "2.0".into(),
            method: "event/notify".into(),
            params: serde_json::json!({
                "topic": "broker.shutdown",
                "session_id": sid,
                "payload": {},
            }),
        });
    }
    // 3. For each Browser: graceful close → 5s wait → kill.
    // CR-1 — first drain recovery watchers BEFORE the per-browser shutdown so
    // a respawn-in-flight cannot race teardown. Each watcher is awaited with
    // a 2 s deadline; expired tasks are dropped (their tokio JoinHandle goes
    // away, ending the task) so we never hang the broker.
    let recovery_handles: Vec<_> = state.recovery_handles.lock().drain(..).collect();
    for h in recovery_handles {
        if tokio::time::timeout(std::time::Duration::from_secs(2), h)
            .await
            .is_err()
        {
            warn!("recovery watcher did not exit within 2s; dropping handle");
        }
    }
    for (_sid, entry) in state.registry.iter() {
        entry.shutdown_system_watches();
        entry.shutdown_terminals().await;
        let browser = entry.browser.load_full();
        if let Err(e) = browser.shutdown().await {
            warn!(session_id = %entry.session_id, error = %e, "session shutdown error");
        }
    }
    // 4. Logs flush via the LogGuard's drop.
    // 5. Release flock + unlink socket.
    let _ = std::fs::remove_file(&socket_path);

    Ok(())
}

fn resolve_socket_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
    Ok(home.join(".one-for-all").join("broker.sock"))
}

fn resolve_lock_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
    let dir = home.join(".one-for-all");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir.join("broker.lock"))
}

fn resolve_user_data_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
    let dir = home.join(".one-for-all").join("sessions");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

/// SPEC §11 R12 / N17 — read the `--max-sessions <N>` CLI flag if present.
/// Falls back to env override `ONE_FOR_ALL_MAX_SESSIONS=<N>` for daemon
/// configurations (launchd plists override env, not argv). `None` means
/// "use the default ([`broker::DEFAULT_MAX_SESSIONS`])".
fn resolve_max_sessions() -> Option<usize> {
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        if a == "--max-sessions" {
            if let Some(v) = iter.next() {
                if let Ok(n) = v.parse::<usize>() {
                    if n >= 1 {
                        return Some(n);
                    }
                }
            }
        } else if let Some(rest) = a.strip_prefix("--max-sessions=") {
            if let Ok(n) = rest.parse::<usize>() {
                if n >= 1 {
                    return Some(n);
                }
            }
        }
    }
    if let Ok(v) = std::env::var("ONE_FOR_ALL_MAX_SESSIONS") {
        if let Ok(n) = v.parse::<usize>() {
            if n >= 1 {
                return Some(n);
            }
        }
    }
    None
}

/// Acquire `flock(LOCK_EX | LOCK_NB)` on `lock_path`. The returned `Flock`
/// must be kept alive for the lifetime of the process — the kernel releases
/// the lock when the underlying file descriptor closes (i.e. when the
/// `Flock` drops).
fn acquire_lock(lock_path: &std::path::Path) -> Result<nix::fcntl::Flock<std::fs::File>> {
    use nix::fcntl::{Flock, FlockArg};

    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    Flock::lock(f, FlockArg::LockExclusiveNonblock).map_err(|(_f, e)| anyhow!("flock: {e}"))
}

async fn wait_for_shutdown() {
    let ctrl_c = signal::ctrl_c();

    // SIGTERM is best-effort: if the handler can't be installed (e.g. no
    // permissions, signal slot already taken) we fall through to ctrl_c
    // only rather than panicking. This keeps the rule "zero .unwrap()/
    // .expect() outside tests" intact.
    #[cfg(unix)]
    let sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            error!(error = %e, "failed to install SIGTERM handler; ctrl_c only");
            None
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = async {
            #[cfg(unix)]
            {
                if let Some(mut s) = sigterm {
                    s.recv().await;
                } else {
                    futures_util::future::pending::<()>().await;
                }
            }
            #[cfg(not(unix))]
            { futures_util::future::pending::<()>().await; }
        } => {}
    }
}

// `State` is shared across tasks via `Arc`. Cloning the Arc is the canonical
// pattern; nothing custom needed here.
