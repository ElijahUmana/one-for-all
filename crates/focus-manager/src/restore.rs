//! Post-spawn focus-restore loop.
//!
//! Chromium's window creation is asynchronous from `Command::spawn`. By the
//! time `spawn` returns, the window may not exist yet. AppKit will surface the
//! window-create as a focus event a beat later. We run a short-lived task that
//! polls the frontmost app and re-activates the captured pid every ~50 ms for
//! up to `max_duration` (typically 1.5 s).
//!
//! After that window we stop. Either:
//! - Restoration worked and frontmost == captured, or
//! - The user has actively switched to a different app — we MUST stop in that
//!   case because the user's intent overrides ours.
//!
//! We track the first-known frontmost as our authority. If it ever differs
//! from `captured` AND differs from a stable "user-chosen" pid that's held for
//! >300ms, we assume user intent and stop.

use std::time::Duration;

use tokio::time::{interval, Instant, MissedTickBehavior};
use tracing::{debug, trace};

#[cfg(target_os = "macos")]
use crate::macos as platform;
#[cfg(not(target_os = "macos"))]
use crate::stub as platform;

/// Canonical window size for the post-Chromium-spawn focus-restore loop.
///
/// Per SPEC §5 D9, the layered focus-no-steal defense holds the post-spawn
/// frontmost-restore loop for 3 seconds at 50 ms ticks. This duration was
/// chosen against macOS testing: Chromium's window-create-then-grab can
/// arrive up to ~2.5 s after `Command::spawn` returns on a cold launch,
/// and a one-tick safety margin past that catches it without bleeding
/// into user-perceptible activation thrash.
///
/// This constant is the single source of truth for that window. Both the
/// public `focus_manager::spawn_chromium_no_focus` façade in `lib.rs` and
/// `browser-engine`'s direct-spawn path call into `spawn_restore_task`
/// with this exact value. Hard-coding `Duration::from_millis(N)` at any
/// other call site is a SPEC §5 D9 violation; reviewers should reject it.
pub const FOCUS_RESTORE_WINDOW: Duration = Duration::from_millis(3_000);

/// Spawn the restore task. Detached — caller does not need to await.
pub fn spawn_restore_task(captured_pid: i32, max_duration: Duration) {
    if !cfg!(target_os = "macos") {
        return;
    }
    tokio::spawn(restore_loop(captured_pid, max_duration));
}

async fn restore_loop(captured_pid: i32, max_duration: Duration) {
    let start = Instant::now();
    let mut tick = interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut user_chosen_first_seen: Option<(i32, Instant)> = None;
    let mut activations = 0u32;

    while start.elapsed() < max_duration {
        tick.tick().await;

        let Some(current) = platform::frontmost_app() else {
            trace!("frontmost app unavailable, retrying");
            continue;
        };

        if current == captured_pid {
            // All good — the user is still focused on the same app. Done.
            user_chosen_first_seen = None;
            continue;
        }

        // Frontmost differs from captured. Decide: chromium grabbing focus, or
        // user deliberately switching?
        match user_chosen_first_seen {
            None => {
                user_chosen_first_seen = Some((current, Instant::now()));
            }
            Some((seen_pid, seen_at)) if seen_pid == current => {
                // Held for ≥300ms — that's user intent, stop fighting it.
                if seen_at.elapsed() > Duration::from_millis(300) {
                    debug!(
                        captured_pid,
                        usurper_pid = current,
                        "user switched apps deliberately, ending restore loop"
                    );
                    return;
                }
            }
            Some((_other, _)) => {
                // pid changed again — chromium-induced flicker, reset and
                // restore.
                user_chosen_first_seen = Some((current, Instant::now()));
            }
        }

        let ok = platform::activate_pid(captured_pid);
        activations += 1;
        trace!(captured_pid, current, ok, activations, "restore activate");
    }

    debug!(captured_pid, activations, "restore loop completed");
}
