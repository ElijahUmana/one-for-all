//! SPEC §5 Layer E integration tests — broker self-applies
//! `NSApplicationActivationPolicy::Accessory` on macOS.
//!
//! Black-box: spawn the real broker binary, ask the OS via `lsappinfo`
//! whether it sees the broker as a UI element / accessory app. Plus a
//! suite of hardening tests that prove the policy survives SIGSTOP /
//! SIGCONT, repeated spawn cycles, and AppleScript-driven activate
//! requests.
//!
//! Gating:
//! - `#![cfg(target_os = "macos")]` — Layer E is a macOS concept.
//! - `#[ignore]` on every test — running these by default would launch
//!   the broker binary on every developer's `cargo test`, race against
//!   their real launchd-managed broker for the SPEC D7 flock, and slow
//!   the default test suite. Run with:
//!   `cargo test -p broker --test macos_activation_policy -- --ignored`
//!
//! HOME isolation: the spawned broker resolves its socket / lock / data
//! paths via `dirs::home_dir()`, which honors `HOME` on Unix. Pointing
//! `HOME` at a tempdir prevents the test broker from competing with the
//! developer's real broker for `~/.one-for-all/broker.lock`, which
//! would otherwise cause the test broker to "exit cleanly" within
//! milliseconds (per `acquire_lock`'s `LOCK_NB` semantics) and the
//! assertion would race a dead PID.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const STARTUP_GRACE: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Test 1 (T15-original): broker self-applies NSApplicationActivationPolicy::Accessory
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "spawns broker binary; run with `cargo test -p broker --test macos_activation_policy -- --ignored`"]
async fn broker_self_applies_accessory_activation_policy() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut handle = spawn_broker(bin, tmp.path()).await;
    sleep(STARTUP_GRACE).await;

    let probe = probe_lsappinfo(handle.pid).await;
    let stderr_dump = shutdown(&mut handle).await;

    assert!(
        probe.is_accessory(),
        "broker did not register as accessory.\n{probe}\nbroker shutdown: {stderr_dump}"
    );

    println!("{probe}");
}

// ---------------------------------------------------------------------------
// Test 2 (T15-h1): policy is stable across 10 rapid spawn cycles
// ---------------------------------------------------------------------------

/// Catches state-leak regressions: if some future change accidentally
/// polluted process-global LaunchServices state, a single spawn might
/// look fine while the second/third would fall back to Regular. Repeat
/// 10× to confirm every spawn re-registers as accessory.
#[tokio::test]
#[ignore = "spawns broker 10× back-to-back; run with --ignored"]
async fn broker_accessory_stable_across_10_spawns() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let mut failures: Vec<String> = Vec::new();

    for iter in 0..10 {
        // Each iteration uses its own tempdir so the per-iteration broker
        // never collides with the previous one's flock or socket. (The
        // previous broker should have been reaped before we reach here,
        // but defense in depth.)
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut handle = spawn_broker(bin, tmp.path()).await;
        sleep(STARTUP_GRACE).await;

        let probe = probe_lsappinfo(handle.pid).await;
        let stderr_dump = shutdown(&mut handle).await;

        if !probe.is_accessory() {
            failures.push(format!(
                "iter {iter}: not accessory.\n{probe}\nshutdown: {stderr_dump}"
            ));
        } else {
            println!("iter {iter}: pid={} OK", handle.pid);
        }
    }

    assert!(
        failures.is_empty(),
        "{}/10 spawns failed Layer E:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

// ---------------------------------------------------------------------------
// Test 3 (T15-h3): policy survives SIGSTOP/SIGCONT
// ---------------------------------------------------------------------------

/// `NSApplicationActivationPolicy` is process-local state on the
/// `NSApplication` singleton. SIGSTOP suspends the process; SIGCONT
/// resumes it. Neither should reset NSApplication state, because the
/// process keeps its memory image and LaunchServices keeps the
/// registration. This test pins that property so a future regression
/// (e.g. someone moving Layer E init behind a runtime hook that re-fires
/// on a signal) is caught immediately.
#[tokio::test]
#[ignore = "spawns broker + sends SIGSTOP/SIGCONT; run with --ignored"]
async fn broker_accessory_survives_sigstop_sigcont() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut handle = spawn_broker(bin, tmp.path()).await;
    sleep(STARTUP_GRACE).await;

    let before = probe_lsappinfo(handle.pid).await;
    assert!(
        before.is_accessory(),
        "broker not accessory before SIGSTOP.\n{before}"
    );

    // SIGSTOP, brief pause, SIGCONT.
    //
    // SAFETY: `handle.pid` was just returned by `child.id()` and the
    // process has not been waited on. `libc::kill` is async-signal-safe.
    unsafe {
        libc::kill(handle.pid as libc::pid_t, libc::SIGSTOP);
    }
    sleep(Duration::from_millis(500)).await;
    unsafe {
        libc::kill(handle.pid as libc::pid_t, libc::SIGCONT);
    }
    // Give the kernel a moment to actually resume scheduling.
    sleep(Duration::from_millis(500)).await;

    let after = probe_lsappinfo(handle.pid).await;
    let stderr_dump = shutdown(&mut handle).await;

    assert!(
        after.is_accessory(),
        "broker lost accessory policy across SIGSTOP/SIGCONT cycle.\nbefore:\n{before}\nafter:\n{after}\nshutdown: {stderr_dump}"
    );

    println!("--- before SIGSTOP ---\n{before}\n--- after SIGCONT ---\n{after}");
}

// ---------------------------------------------------------------------------
// Test 4 (T15-h3 cont.): AppleScript `activate` does not flip the policy
// ---------------------------------------------------------------------------

/// `osascript -e 'tell application "one-for-all-broker" to activate'`
/// asks LaunchServices to send an activate event. With policy ==
/// Accessory and no NSWindow, the broker has no UI to bring forward and
/// the event should be a no-op for our policy. This test verifies it.
///
/// If `osascript` cannot find the app (because it's not bundled), the
/// command may print "execution error: …" and exit non-zero. That is
/// acceptable — it means activate cannot even be requested, which is a
/// strictly stronger guarantee. We do NOT fail the test on osascript
/// errors; we only fail if the post-activate probe shows the policy
/// changed.
#[tokio::test]
#[ignore = "spawns broker + osascript activate; run with --ignored"]
async fn broker_accessory_unaffected_by_applescript_activate() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut handle = spawn_broker(bin, tmp.path()).await;
    sleep(STARTUP_GRACE).await;

    let before = probe_lsappinfo(handle.pid).await;
    assert!(
        before.is_accessory(),
        "broker not accessory before AppleScript activate.\n{before}"
    );

    // Best-effort activate. We do not assert on osascript's exit status —
    // an error here just means activate could not be delivered, which is
    // a stronger no-op than what we're testing for.
    let osa = Command::new("osascript")
        .arg("-e")
        .arg(r#"tell application "one-for-all-broker" to activate"#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    let osa_summary = match &osa {
        Ok(o) => format!(
            "osascript exit={:?} stdout={:?} stderr={:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => format!("osascript invocation failed: {e}"),
    };

    sleep(Duration::from_millis(500)).await;

    let after = probe_lsappinfo(handle.pid).await;
    let stderr_dump = shutdown(&mut handle).await;

    assert!(
        after.is_accessory(),
        "broker policy changed after AppleScript activate.\nbefore:\n{before}\nafter:\n{after}\n{osa_summary}\nshutdown: {stderr_dump}"
    );

    println!(
        "--- before activate ---\n{before}\n--- {osa_summary} ---\n--- after activate ---\n{after}"
    );
}

// ---------------------------------------------------------------------------
// Test 5 (T15-h2): idempotent double-apply preserves Accessory
// ---------------------------------------------------------------------------

/// `set_accessory_activation_policy` has a runtime "already accessory"
/// branch that short-circuits AppKit work on the second call. Production
/// `main` only calls it once. To exercise the second-call branch end-to-
/// end, the broker `main` is wired with a debug-only env-gated re-apply
/// (see `crates/broker/src/main.rs::OFA_LAYER_E_DOUBLE_APPLY`). This test
/// flips that env var, spawns the broker, and asserts the policy is still
/// Accessory after both calls have run.
///
/// What this catches: a future regression where the second call
/// accidentally fights with the first (e.g. someone replaces the
/// idempotency block with `app.setActivationPolicy(Regular)` then
/// `app.setActivationPolicy(Accessory)` — a brief Regular flicker would
/// not be caught by Test 1 which probes only once after a single apply).
#[tokio::test]
#[ignore = "spawns broker with OFA_LAYER_E_DOUBLE_APPLY=1; run with --ignored"]
async fn broker_accessory_idempotent_under_double_apply() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let tmp = tempfile::tempdir().expect("tempdir");

    let child = Command::new(bin)
        .env("HOME", tmp.path())
        .env("OFA_LAYER_E_DOUBLE_APPLY", "1")
        .env_remove("ONE_FOR_ALL_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn broker binary with double-apply");
    let pid = child.id().expect("broker child pid");
    let mut handle = BrokerHandle { child, pid };
    sleep(STARTUP_GRACE).await;

    let probe = probe_lsappinfo(handle.pid).await;
    let stderr_dump = shutdown(&mut handle).await;

    assert!(
        probe.is_accessory(),
        "broker policy regressed under double-apply.\n{probe}\nbroker shutdown: {stderr_dump}"
    );

    // Sanity: the broker should have logged the double-apply marker. We
    // don't make this fatal — the log target may be configured to drop
    // info-level lines in some test environments — but a missing marker
    // is worth surfacing.
    if !stderr_dump.contains("Layer E h2") {
        eprintln!(
            "[note] broker stderr did not contain the 'Layer E h2' marker; \
             double-apply branch may not have executed. stderr={stderr_dump}"
        );
    }

    println!("{probe}");
}

// ---------------------------------------------------------------------------
// Test 6 (T15-h4): broker stays Accessory while a third-party app activates
// ---------------------------------------------------------------------------

/// Layer E (broker policy) and Layer C (focus-restore against a captured
/// pid) operate on disjoint subjects: Layer E governs the broker's own
/// `NSApplication`, Layer C activates a *different* process. This test
/// pins the boundary by triggering an AppleScript activate against
/// Finder (always running on macOS) and confirming broker's
/// `lsappinfo`-reported policy is unchanged.
///
/// What this catches: a regression where someone introduces a "convenience"
/// call that ends up activating the broker as a side effect of activating
/// another app (e.g. by mistakenly passing the broker's pid into
/// `activateWithOptions`).
#[tokio::test]
#[ignore = "spawns broker + osascript activate Finder; run with --ignored"]
async fn broker_accessory_unaffected_by_third_party_activate() {
    let bin = env!("CARGO_BIN_EXE_one-for-all-broker");
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut handle = spawn_broker(bin, tmp.path()).await;
    sleep(STARTUP_GRACE).await;

    let before = probe_lsappinfo(handle.pid).await;
    assert!(
        before.is_accessory(),
        "broker not accessory before third-party activate.\n{before}"
    );

    // Activate Finder. Finder is the per-user macOS shell process and is
    // always running; it is the safest "always-available foreign target"
    // for this test. If for some reason osascript fails (rare; e.g.
    // sandbox restrictions on a CI runner), we treat that as a
    // strictly-stronger no-op and proceed — Layer E's invariant is that
    // *whatever* a third-party tool tries, the broker stays Accessory.
    let osa = Command::new("osascript")
        .arg("-e")
        .arg(r#"tell application "Finder" to activate"#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    let osa_summary = match &osa {
        Ok(o) => format!(
            "osascript exit={:?} stdout={:?} stderr={:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => format!("osascript invocation failed: {e}"),
    };

    // Brief settle so any spurious activation-related events have a chance
    // to land before we re-probe.
    sleep(Duration::from_millis(500)).await;

    let after = probe_lsappinfo(handle.pid).await;
    let stderr_dump = shutdown(&mut handle).await;

    assert!(
        after.is_accessory(),
        "broker policy changed after third-party activate (Finder).\n\
         before:\n{before}\nafter:\n{after}\n{osa_summary}\nshutdown: {stderr_dump}"
    );

    println!(
        "--- before activate ---\n{before}\n--- {osa_summary} ---\n--- after activate ---\n{after}"
    );
}

// ---------------------------------------------------------------------------
// Helpers — shared spawn / probe / shutdown plumbing
// ---------------------------------------------------------------------------

struct BrokerHandle {
    child: Child,
    pid: u32,
}

async fn spawn_broker(bin: &str, home: &Path) -> BrokerHandle {
    let child = Command::new(bin)
        .env("HOME", home)
        .env_remove("ONE_FOR_ALL_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn broker binary");
    let pid = child.id().expect("broker child pid");
    BrokerHandle { child, pid }
}

/// Bundle of three lsappinfo signals plus an `is_accessory` predicate.
/// `Display` formats the full transcript so it can be embedded in panic
/// messages and `--nocapture` output.
struct LsappinfoProbe {
    pid: u32,
    by_pid: String,
    by_name: String,
    by_pid_full: String,
}

impl LsappinfoProbe {
    fn is_accessory(&self) -> bool {
        let signals = [&self.by_pid, &self.by_name, &self.by_pid_full];
        signals
            .iter()
            .any(|s| s.contains("UIElement") || s.contains("Accessory"))
    }
}

impl std::fmt::Display for LsappinfoProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "--- lsappinfo info -only ApplicationType {} ---",
            self.pid
        )?;
        writeln!(f, "{}", self.by_pid)?;
        writeln!(
            f,
            "--- lsappinfo info -only ApplicationType -app one-for-all-broker ---"
        )?;
        writeln!(f, "{}", self.by_name)?;
        writeln!(f, "--- lsappinfo info {} ---", self.pid)?;
        writeln!(f, "{}", self.by_pid_full)
    }
}

async fn probe_lsappinfo(pid: u32) -> LsappinfoProbe {
    let pid_str = format!("{pid}");
    let by_pid = run_lsappinfo(&["info", "-only", "ApplicationType", &pid_str]).await;
    let by_name = run_lsappinfo(&[
        "info",
        "-only",
        "ApplicationType",
        "-app",
        "one-for-all-broker",
    ])
    .await;
    let by_pid_full = run_lsappinfo(&["info", &pid_str]).await;
    LsappinfoProbe {
        pid,
        by_pid,
        by_name,
        by_pid_full,
    }
}

/// Send SIGTERM, wait up to SHUTDOWN_GRACE for the process to exit, then
/// SIGKILL on timeout. Returns a one-line summary of the broker's exit
/// state suitable for embedding in panic messages.
async fn shutdown(handle: &mut BrokerHandle) -> String {
    // SAFETY: pid was returned by `child.id()` and we have not yet
    // waited on the child, so it is a live, owned descendant.
    unsafe {
        libc::kill(handle.pid as libc::pid_t, libc::SIGTERM);
    }
    let wait_result = timeout(SHUTDOWN_GRACE, handle.child.wait()).await;
    let _ = handle.child.start_kill();
    match wait_result {
        Ok(Ok(status)) => format!("[broker exited: {status}]"),
        Ok(Err(e)) => format!("[broker wait failed: {e}]"),
        Err(_) => "[broker still running after SHUTDOWN_GRACE; SIGKILL'd]".to_string(),
    }
}

async fn run_lsappinfo(args: &[&str]) -> String {
    match Command::new("lsappinfo")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.trim().is_empty() {
                stdout
            } else {
                format!("{stdout}\n[stderr]\n{stderr}")
            }
        }
        Err(e) => format!("[lsappinfo invocation failed: {e}]"),
    }
}

// Silence the unused-imports lint for `TempDir` on toolchains where the
// associated-types lint promotes it.
#[allow(dead_code)]
fn _hold_tempdir(_: TempDir) {}
