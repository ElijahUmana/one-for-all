//! # focus-manager
//!
//! Spawn Chromium on macOS such that the *currently frontmost application stays
//! frontmost*. Zero observable focus event from the user's perspective.
//!
//! This is the load-bearing constraint for one-for-all: if any spawn ever
//! steals focus, the user notices and the product is dead.
//!
//! ## Strategy
//!
//! Two modes:
//! - [`SpawnMode::Headless`] — uses `--headless=new`. Chromium never creates a
//!   visible window, so there is *no* focus contention by definition.
//! - [`SpawnMode::Headed`] — Chromium creates a real window. We:
//!   1. Capture the currently frontmost `NSRunningApplication` BEFORE spawn.
//!   2. Launch Chromium with `--window-position=-32000,-32000` (offscreen) plus
//!      `--no-startup-window` so the initial render window is suppressed; new
//!      pages are opened by the broker via `Target.createTarget`.
//!   3. After spawn, run a bounded restore loop that re-activates the captured
//!      app via `[NSRunningApplication activateWithOptions:0]` whenever a new
//!      app rises to frontmost. This counters Chromium's brief focus grab on
//!      window creation. The loop self-cancels after [`FOCUS_RESTORE_WINDOW`]
//!      (3 s per SPEC §5 D9).
//!
//! ## What this crate does NOT do
//!
//! - It does *not* call `[NSApplication activate]` on the *current* process.
//!   The goal is to RESTORE the user's app, not to steal focus for ourselves.
//! - It does not own Chromium's lifecycle past spawn. The returned `Child` is
//!   the caller's to manage.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use thiserror::Error;
use tokio::process::{Child, Command};

pub mod restore;
pub mod spawn_flags;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(not(target_os = "macos"))]
mod stub;

// Re-export Layer E surface unconditionally so `broker::main` can call into
// it without `#[cfg]` gates. The `macos` module owns the real impl; `stub`
// provides an inert no-op on Linux/Windows.
#[cfg(target_os = "macos")]
pub use macos::{set_accessory_activation_policy, AccessoryPolicyError, AccessoryPolicyGuard};
#[cfg(not(target_os = "macos"))]
pub use stub::{set_accessory_activation_policy, AccessoryPolicyError, AccessoryPolicyGuard};

// Re-export the canonical SPEC §5 D9 restore window so out-of-crate callers
// (browser-engine's direct-spawn path) can reach it without depending on the
// private `restore` module path.
pub use restore::FOCUS_RESTORE_WINDOW;

/// Whether to launch Chromium with a headless or headed UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// `--headless=new`. Truly invisible. Default.
    Headless,
    /// Visible window, but spawned offscreen and with the focus-restore shim.
    Headed,
}

#[derive(Debug, Error)]
pub enum FocusError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("platform not supported: {0}")]
    Unsupported(&'static str),
}

/// Spawn Chromium without stealing focus from the currently frontmost app.
///
/// `binary` is the absolute path to the Chromium executable (resolved by
/// `chromium-fetcher`). `user_data_dir` will be created if missing.
///
/// On non-macOS platforms this falls back to a plain `tokio::process::Command`
/// spawn — focus-stealing isn't a concept on Linux servers (where headless is
/// the only sensible mode anyway) and Windows is out of scope.
pub async fn spawn_chromium_no_focus(
    binary: &Path,
    mode: SpawnMode,
    user_data_dir: &Path,
    extra_args: &[OsString],
) -> Result<Child> {
    tokio::fs::create_dir_all(user_data_dir)
        .await
        .with_context(|| format!("creating user-data-dir at {}", user_data_dir.display()))?;

    let argv = spawn_flags::build_argv(mode, user_data_dir, extra_args);

    // Capture the frontmost app BEFORE spawn. We do this even in headless mode
    // as cheap insurance — it costs ~microseconds and means we never get caught
    // out if a flag we set elsewhere ever triggers a window.
    #[cfg(target_os = "macos")]
    let captured = macos::frontmost_app();
    #[cfg(not(target_os = "macos"))]
    let captured: Option<i32> = None;

    let mut cmd = Command::new(binary);
    cmd.args(&argv);
    cmd.kill_on_drop(true);
    // Inherit stdout/stderr so the broker can attach to them; pipe fd 3/4 for
    // CDP is wired up by browser-engine, not here.
    let child = cmd.spawn().with_context(|| {
        format!(
            "spawning chromium binary={} mode={:?}",
            binary.display(),
            mode
        )
    })?;

    if mode == SpawnMode::Headed {
        if let Some(pid) = captured {
            #[cfg(target_os = "macos")]
            restore::spawn_restore_task(pid, FOCUS_RESTORE_WINDOW);
            #[cfg(not(target_os = "macos"))]
            {
                let _ = pid; // suppress unused
            }
        }
    }

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_includes_remote_debugging_pipe_in_both_modes() {
        let dir = std::path::PathBuf::from("/tmp/ofa-test-uddir");
        for mode in [SpawnMode::Headless, SpawnMode::Headed] {
            let argv = spawn_flags::build_argv(mode, &dir, &[]);
            assert!(
                argv.iter().any(|a| a == "--remote-debugging-pipe"),
                "missing --remote-debugging-pipe in {:?}: {:?}",
                mode,
                argv
            );
            assert!(
                argv.iter()
                    .any(|a| a.to_string_lossy().starts_with("--user-data-dir=")),
                "missing --user-data-dir in {:?}",
                mode
            );
        }
    }

    #[test]
    fn headless_argv_is_distinct_from_headed() {
        let dir = std::path::PathBuf::from("/tmp/ofa-test-uddir");
        let h = spawn_flags::build_argv(SpawnMode::Headless, &dir, &[]);
        let v = spawn_flags::build_argv(SpawnMode::Headed, &dir, &[]);
        assert!(h.iter().any(|a| a == "--headless=new"));
        assert!(!v.iter().any(|a| a == "--headless=new"));
        assert!(v.iter().any(|a| a == "--window-position=-32000,-32000"));
    }
}
