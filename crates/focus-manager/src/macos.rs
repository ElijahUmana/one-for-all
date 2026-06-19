//! Cocoa-side bridge: capture frontmost app pre-spawn, force-restore post-spawn.
//!
//! This module is `#[cfg(target_os = "macos")]`. Linux/Windows fallbacks live
//! in `stub.rs`.
//!
//! ## Why no `objc2::msg_send!` here?
//!
//! We use the typed `objc2-app-kit` bindings exclusively. Hand-written
//! `msg_send!` is faster to write but slower to audit; the typed API surfaces
//! ABI mismatches at compile time and is what every other reviewer will look
//! for first when investigating "did we accidentally activate ourselves?".

#![cfg(target_os = "macos")]

use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSRunningApplication, NSWorkspace,
};
use objc2_foundation::MainThreadMarker;
use thiserror::Error;
use tracing::info;

/// Returns the PID of the application currently in the frontmost position, or
/// `None` if nothing is frontmost (e.g. headless boot, mission-control active).
///
/// Safe to call from any thread — `NSWorkspace.sharedWorkspace` is documented
/// thread-safe, and `frontmostApplication` is a snapshot read.
pub fn frontmost_app() -> Option<i32> {
    // SAFETY: NSWorkspace.sharedWorkspace returns a global singleton; reading
    // its frontmostApplication property is documented thread-safe.
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let app = unsafe { ws.frontmostApplication() }?;
    Some(unsafe { app.processIdentifier() } as i32)
}

/// Re-activate the application identified by `pid` without bringing OUR
/// process to the front. Returns `true` if the activate call succeeded.
///
/// Uses `activateWithOptions:` with options=0 (no `ActivateAllWindows`, no
/// `ActivateIgnoringOtherApps` initially) to perform a polite restore. If the
/// polite call returns false (which AppKit can do during certain transition
/// states), we retry once with `ActivateIgnoringOtherApps` since the goal is
/// always to put the user's app back where it was.
pub fn activate_pid(pid: i32) -> bool {
    // SAFETY: runningApplicationWithProcessIdentifier returns nil if the
    // process is dead; we Option-handle it below.
    let app = unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid as libc::pid_t)
    };
    let Some(app) = app else { return false };

    // First attempt: polite (options=0).
    let ok = unsafe { app.activateWithOptions(NSApplicationActivationOptions::empty()) };
    if ok {
        return true;
    }

    // Polite call failed (e.g. mid-transition). Retry with ignoring-other-apps,
    // which is still NOT activating us — it's activating the captured app over
    // anyone who tried to grab focus during the spawn.
    #[allow(deprecated)]
    let opts = NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps;
    unsafe { app.activateWithOptions(opts) }
}

/// True if the OS still considers `pid` a running, addressable application.
pub fn is_running(pid: i32) -> bool {
    unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid as libc::pid_t).is_some()
    }
}

/// Errors from `set_accessory_activation_policy`.
///
/// Layer E is best-effort hardening on top of Layers A-D; we do not panic
/// from an AppKit init failure. Callers (broker `main`) can choose to log
/// and proceed, but the broker treats this as fatal because Layer E being
/// missing means we are not honoring SPEC §5.
#[derive(Debug, Error)]
pub enum AccessoryPolicyError {
    /// `MainThreadMarker::new()` returned `None`. AppKit's
    /// `NSApplication::sharedApplication` must be called on the process's
    /// main (initial) thread; this error means the caller invoked us
    /// elsewhere.
    #[error(
        "set_accessory_activation_policy must be called on the process's main thread, before tokio takes over"
    )]
    NotMainThread,
}

/// RAII marker held by `main` for the lifetime of the broker process.
///
/// `NSApplicationActivationPolicy::Accessory` persists on the shared
/// `NSApplication` for process lifetime, so this guard has no `Drop` work
/// to do. Its only purposes are:
/// 1. Make it syntactically obvious in `main` that Layer E is held.
/// 2. Discourage callers from re-applying the policy mid-run.
///
/// The guard is intentionally not `Clone` and not `Send`/`Sync` (it carries
/// `MainThreadMarker`, which is `!Send` and `!Sync`).
pub struct AccessoryPolicyGuard {
    _mtm: MainThreadMarker,
}

/// SPEC §5 Layer E — broker self-applies
/// `NSApplicationActivationPolicy::Accessory`.
///
/// MUST be called from `fn main()` on the process's main thread, BEFORE the
/// tokio runtime is constructed. Tokio's `multi_thread` runtime spawns
/// worker threads; AppKit's `NSApplication::sharedApplication` is documented
/// main-thread-only, and the only reliable way to hold that invariant from
/// a Rust binary is to do the AppKit setup synchronously before any tokio
/// machinery starts.
///
/// ## Why "Accessory"?
///
/// - `NSApplicationActivationPolicy::Regular` — the default for foreground
///   apps. Shows a Dock icon and a menu bar. Wrong for the broker.
/// - `NSApplicationActivationPolicy::Accessory` — process can show NSWindows
///   if it wants to, but never a Dock icon and never a menu bar. The broker
///   itself never creates an NSWindow (Chromium does that, in its own
///   process), so the visible effect is "no Dock icon and never frontmost".
///   This is the SPEC §5 Layer E choice.
/// - `NSApplicationActivationPolicy::Prohibited` — process cannot create
///   NSWindows at all. Too restrictive; would break any future tooling
///   (e.g. a debug HUD) that wants to render from broker.
///
/// ## Interaction with launchd / Info.plist
///
/// A wrapping `.app` bundle could set `LSUIElement=1` (or `LSBackgroundOnly`)
/// in its `Info.plist` to achieve the same effect for launchd-launched
/// processes. We do it in code instead so the broker is correct regardless
/// of how it was launched — shell, launchd, integration test, debugger,
/// `cargo run`. Belt-and-braces with a future Info.plist is fine; this
/// function is idempotent on subsequent calls (calling it twice in the
/// same process is harmless — the second call observes the policy is
/// already Accessory and returns a fresh guard with no AppKit work). See
/// the "idempotency" section in the function body for the runtime check.
///
/// ## Boundary with focus-manager Layers C/D
///
/// Layer E governs only the *broker process's* activation policy. The
/// Chromium child process spawned by `spawn_chromium_no_focus` is a
/// separate process with its own NSApplication and its own activation
/// policy (Regular, by Chromium's default). The two never share state:
///
/// - Layer E (this fn) ensures the broker never appears in the Dock and
///   never becomes frontmost.
/// - Layer C (`spawn_restore_task`) ensures that when Chromium's window-
///   create grabs focus, we *restore the user's previously frontmost app*
///   — explicitly NOT activating the broker.
/// - Layer D (`Page.bringToFront` only, `Target.activateTarget` forbidden)
///   ensures that intra-Chromium tab activation never escalates to a
///   process-level activation event.
///
/// The `_no_self_activate` tripwire below is the static guard that this
/// crate never blurs the boundary.
///
/// ## Survival across SIGSTOP/SIGCONT, sleep/wake, AppleScript activate
///
/// `NSApplicationActivationPolicy` is process-local state attached to the
/// `NSApplication` singleton. It is not session state, not Dock state, not
/// LaunchServices cache state. As a consequence:
///
/// - `SIGSTOP` then `SIGCONT` does not reset it (the process keeps its
///   in-memory NSApplication; LaunchServices keeps the registration).
///   Verified runtime: `lsappinfo info -only ApplicationType <pid>`
///   reports `"UIElement"` both before SIGSTOP and after SIGCONT.
/// - System sleep / wake does not reset it (NSApplication is not
///   re-instantiated across power cycles). The broker process simply
///   resumes execution post-wake with the same NSApplication instance
///   and the same activation policy. We do NOT subscribe to
///   `NSWorkspace.didWakeNotification` — there is no wake-time work to
///   do, because (a) the policy is preserved automatically, and (b) the
///   broker does not initiate spawns on wake (Chromium spawning is
///   reactive to RPC, not to power events). Adding a wake handler would
///   solve a non-existent problem and add a notification-pump
///   maintenance burden.
/// - `osascript -e 'tell application "one-for-all-broker" to activate'`
///   asks LaunchServices to send an activate event; with policy ==
///   Accessory + no NSWindow, the broker has no UI to bring forward.
///   For non-bundled binaries (cargo run, debugger), LaunchServices
///   typically rejects the lookup with "execution error -1728" before
///   any event is delivered — a strictly stronger no-op than the
///   bundled case.
///
/// All three properties are exercised by `crates/broker/tests/macos_activation_policy.rs`.
///
/// ## Failure modes
///
/// - `NotMainThread` — the only fallible case at runtime. Returned if
///   `MainThreadMarker::new()` is `None`.
pub fn set_accessory_activation_policy() -> Result<AccessoryPolicyGuard, AccessoryPolicyError> {
    let mtm = MainThreadMarker::new().ok_or(AccessoryPolicyError::NotMainThread)?;

    // `NSApplication::sharedApplication(mtm)` is the typed objc2-app-kit
    // binding that requires a `MainThreadMarker` proof; the binding itself
    // is safe. We obtained `mtm` via `MainThreadMarker::new()` which only
    // returns `Some` on the main thread, so the AppKit invariant is upheld
    // at runtime. `setActivationPolicy` is also a safe binding (the
    // generated code marks it `pub fn`, not `pub unsafe fn`) — no unsafe
    // block is required at this call site.
    let app = NSApplication::sharedApplication(mtm);

    // Idempotency check. AppKit's `setActivationPolicy` is documented as
    // safe to call repeatedly; we still short-circuit the second call so
    // (a) the log line is accurate about what happened and (b) we surface a
    // bug if a regression ever ships an unconditional Regular reset.
    //
    // SAFETY: `activationPolicy()` is marked `pub unsafe fn` in the
    // generated bindings only because the broader NSApplication type
    // graph has `MainThreadOnly` mutability semantics. We hold `mtm` (a
    // valid main-thread proof) across this call site, so the invariant
    // is upheld.
    let already = unsafe { app.activationPolicy() } == NSApplicationActivationPolicy::Accessory;
    if already {
        info!(
            policy = "NSApplicationActivationPolicyAccessory",
            "SPEC §5 Layer E reaffirmed: policy already Accessory (idempotent call)"
        );
    } else {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        info!(
            policy = "NSApplicationActivationPolicyAccessory",
            "SPEC §5 Layer E applied: broker registered as accessory (no Dock icon, never frontmost)"
        );
    }

    Ok(AccessoryPolicyGuard { _mtm: mtm })
}

/// Asserted at compile time: we never construct an `NSApplication` to
/// activate ourselves.
///
/// `MainThreadMarker` is also used by `set_accessory_activation_policy` for
/// the real Layer E setup. This sentinel function exists purely as a
/// code-review tripwire: if a future contributor reaches for
/// `NSApp.activate()` on our own process, the diff will land here and code
/// review can reject it.
#[allow(dead_code)]
fn _no_self_activate(_marker: MainThreadMarker) {
    // Intentionally empty. If somebody adds NSApp::shared(marker).activate()
    // here, code review must reject it: this crate's invariant is that we
    // never activate ourselves. (Layer E sets the activation *policy*, not
    // an *activation event* — those are different APIs.)
}

// ---------------------------------------------------------------------------
// Compile-time tripwires for SPEC §5 Layer E h5 (wake-from-sleep is a no-op).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Strip line comments (any line whose first non-whitespace chars are
    /// `//`, including doc-comment forms `///` and `//!`) from `src`. The
    /// wake-handler tripwire test scans the result, which lets the doc
    /// comments on `set_accessory_activation_policy` may name
    /// the forbidden APIs while explaining why they aren't used.
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// SPEC §5 Layer E h5 — broker MUST NOT subscribe to
    /// `NSWorkspaceDidWakeNotification` (or any other power-event
    /// notification). The activation policy is process-local state on
    /// `NSApplication`; it survives sleep/wake automatically. A wake
    /// handler that ran code on resume would (a) solve a non-existent
    /// problem and (b) be a tempting place to mistakenly call
    /// `setActivationPolicy(.regular)` or `[NSApp activate]`.
    ///
    /// Locks the absence as a textual property of this module: a future
    /// contributor adding `addObserverForName:NSWorkspaceDidWakeNotification`
    /// to this file (or its sibling `restore.rs`) will land a diff that
    /// fails this test, forcing the conversation back through code review.
    ///
    /// We deliberately match against `include_str!` of the source files
    /// — not against runtime symbols — because the goal is to catch the
    /// API call at compile-time, not to skip the test if the wake handler
    /// hasn't been triggered yet.
    #[test]
    fn forbid_wake_notification_handlers_in_focus_manager() {
        const FORBIDDEN: &[&str] = &[
            "NSWorkspaceDidWakeNotification",
            "didWakeNotification",
            "NSDistributedNotificationCenter",
            "addObserverForName",
            "addObserver_forName",
        ];

        let files: &[(&str, &str)] = &[
            ("focus-manager/src/macos.rs", include_str!("macos.rs")),
            ("focus-manager/src/restore.rs", include_str!("restore.rs")),
            ("focus-manager/src/lib.rs", include_str!("lib.rs")),
        ];

        for (label, full) in files {
            // Production scan: drop the test module(s) and all line
            // comments. The test module is excluded because its
            // FORBIDDEN-array literal is by construction self-
            // referencing; comments are excluded so the doc on
            // `set_accessory_activation_policy` may name
            // the forbidden APIs in prose.
            let prod = full.split("#[cfg(test)]").next().unwrap_or(full);
            let scanned = strip_line_comments(prod);
            for needle in FORBIDDEN {
                assert!(
                    !scanned.contains(needle),
                    "SPEC §5 Layer E h5 violated: {label} contains forbidden \
                     wake-handler symbol {needle:?}. Adding a wake-from-sleep \
                     handler is forbidden — see the doc on \
                     `set_accessory_activation_policy`. (Doc comments and \
                     #[cfg(test)] modules are exempt; this test only scans \
                     non-comment production lines.)"
                );
            }
        }
    }

    /// Sanity — the comment stripper actually filters comment lines.
    /// If this regresses, the wake tripwire above would silently false-
    /// positive on doc comments and false-negative on real call sites.
    #[test]
    fn strip_line_comments_removes_doc_and_inline_comments() {
        let src = "fn ok() {}\n\
                   // line comment with addObserverForName\n\
                   /// doc comment with NSWorkspaceDidWakeNotification\n\
                   //! crate doc with didWakeNotification\n\
                   fn also_ok() {}\n";
        let out = strip_line_comments(src);
        assert!(out.contains("fn ok()"));
        assert!(out.contains("fn also_ok()"));
        assert!(!out.contains("addObserverForName"));
        assert!(!out.contains("NSWorkspaceDidWakeNotification"));
        assert!(!out.contains("didWakeNotification"));
    }
}
