//! SPEC §12 — `app.subscribe` AX event stream.
//!
//! Each subscription owns a dedicated OS thread running a `CFRunLoop`. We
//! create an `AXObserver` for the target app's pid, attach it to the
//! thread's runloop, register one callback per requested topic, and pump
//! the runloop until the subscription is dropped. The callback writes
//! [`AxEvent`] structs into a `tokio::sync::mpsc::Sender` (capacity 1024,
//! drop-oldest on overflow with a `tracing::warn`) which the broker drains
//! into `event/notify {topic: "app.event", …}`.
//!
//! Cleanup: dropping the [`Subscription`] handle posts a `CFRunLoopStop` to
//! the thread, which then joins. The observer's run-loop source is
//! detached and the AXObserver itself is released by `AxOwned::Drop`.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedUIElementChangedNotification, kAXSelectedTextChangedNotification,
    kAXTitleAttribute, kAXUIElementDestroyedNotification, kAXValueAttribute,
    kAXValueChangedNotification, kAXWindowCreatedNotification, AXObserverAddNotification,
    AXObserverCreate, AXObserverGetRunLoopSource, AXObserverRef, AXObserverRemoveNotification,
    AXUIElementCreateApplication, AXUIElementRef,
};
use core_foundation::base::TCFType;
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopAddSource, CFRunLoopRemoveSource,
};
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::ax_walk::copy_string_attr;
use crate::types::{AxEvent, AxEventTopic, AxSubscription, NativeControlError};

/// Channel capacity per subscription. Drop-oldest applied beyond this.
pub const CHANNEL_CAPACITY: usize = 1024;

/// One live subscription.
pub struct Subscription {
    info: AxSubscription,
    /// Set false on drop → observer thread exits its runloop.
    alive: Arc<AtomicBool>,
    runloop_handle: Mutex<Option<RunLoopHandle>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

struct RunLoopHandle {
    runloop: CFRunLoop,
}

// SAFETY: CFRunLoop is documented as thread-safe for `Stop`/`WakeUp`. We
// never share its non-thread-safe operations across threads — the observer
// thread is sole owner; this Send wrapper is only used to ferry the
// runloop pointer once at thread startup.
unsafe impl Send for RunLoopHandle {}

impl Subscription {
    pub fn info(&self) -> &AxSubscription {
        &self.info
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(rl) = self.runloop_handle.lock().take() {
            // SAFETY: CFRunLoopStop is thread-safe per Apple docs.
            rl.runloop.stop();
        }
        if let Some(j) = self.join.lock().take() {
            // Best-effort join — if the thread panicked we don't propagate.
            let _ = j.join();
        }
    }
}

/// Spawn a subscription on `bundle_id` listening for the given topics.
///
/// Returns the [`Subscription`] handle and an mpsc receiver of [`AxEvent`].
pub fn spawn(
    bundle_id: &str,
    topics: &[AxEventTopic],
    subscription_id: String,
) -> Result<(Subscription, mpsc::Receiver<AxEvent>), NativeControlError> {
    let pid = crate::actions::resolve_pid(bundle_id)?;

    let (tx, rx) = mpsc::channel::<AxEvent>(CHANNEL_CAPACITY);
    let alive = Arc::new(AtomicBool::new(true));
    let topics_set: BTreeSet<AxEventTopic> = topics.iter().copied().collect();

    let info = AxSubscription {
        subscription_id,
        bundle_id: bundle_id.to_string(),
        topics: topics_set.iter().copied().collect(),
    };

    let (rl_tx, rl_rx) = std::sync::mpsc::sync_channel::<RunLoopHandle>(1);

    let bundle_id_owned = bundle_id.to_string();
    let topics_for_thread = topics_set.clone();
    let alive_thread = Arc::clone(&alive);

    let join = std::thread::Builder::new()
        .name(format!("ax-observer-{bundle_id_owned}"))
        .spawn(move || {
            run_observer_thread(
                pid,
                bundle_id_owned,
                topics_for_thread,
                tx,
                alive_thread,
                rl_tx,
            );
        })
        .map_err(|e| NativeControlError::Internal(format!("spawn observer thread: {e}")))?;

    // Wait briefly for the runloop to be ready.
    let runloop_handle = rl_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| {
            NativeControlError::Internal("observer thread never reported runloop".into())
        })?;

    Ok((
        Subscription {
            info,
            alive,
            runloop_handle: Mutex::new(Some(runloop_handle)),
            join: Mutex::new(Some(join)),
        },
        rx,
    ))
}

// ----------------------- observer thread internals ------------------------

struct CallbackContext {
    bundle_id: String,
    tx: mpsc::Sender<AxEvent>,
    alive: Arc<AtomicBool>,
}

extern "C" fn ax_callback(
    _observer: AXObserverRef,
    elem: AXUIElementRef,
    notification: core_foundation_sys::string::CFStringRef,
    refcon: *mut std::ffi::c_void,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: refcon was set to a leaked Box<CallbackContext>.
    let ctx = unsafe { &*(refcon as *const CallbackContext) };
    if !ctx.alive.load(Ordering::SeqCst) {
        return;
    }
    let topic_name = unsafe { CFString::wrap_under_get_rule(notification) }.to_string();
    let topic = match topic_str_to_enum(&topic_name) {
        Some(t) => t,
        None => return,
    };
    let role = copy_string_attr(elem, accessibility_sys::kAXRoleAttribute);
    let name = copy_string_attr(elem, kAXTitleAttribute);
    let value = copy_string_attr(elem, kAXValueAttribute);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let event = AxEvent {
        bundle_id: ctx.bundle_id.clone(),
        topic,
        timestamp_ms: now_ms,
        element_ref: None, // resolved by broker against latest snapshot
        role,
        name,
        value,
    };
    // try_send; on full channel, drop-oldest = drop-this-one with a warn.
    if ctx.tx.try_send(event).is_err() {
        warn!(bundle_id = %ctx.bundle_id, "AX event channel full; dropping event");
    }
}

fn topic_str_to_enum(s: &str) -> Option<AxEventTopic> {
    Some(if s == kAXValueChangedNotification {
        AxEventTopic::ValueChanged
    } else if s == kAXFocusedUIElementChangedNotification {
        AxEventTopic::FocusedChanged
    } else if s == kAXWindowCreatedNotification {
        AxEventTopic::WindowCreated
    } else if s == kAXUIElementDestroyedNotification {
        AxEventTopic::WindowDestroyed
    } else if s == kAXSelectedTextChangedNotification {
        AxEventTopic::SelectionChanged
    } else {
        return None;
    })
}

fn topic_enum_to_str(t: AxEventTopic) -> &'static str {
    match t {
        AxEventTopic::ValueChanged => kAXValueChangedNotification,
        AxEventTopic::FocusedChanged => kAXFocusedUIElementChangedNotification,
        AxEventTopic::WindowCreated => kAXWindowCreatedNotification,
        AxEventTopic::WindowDestroyed => kAXUIElementDestroyedNotification,
        AxEventTopic::SelectionChanged => kAXSelectedTextChangedNotification,
    }
}

fn run_observer_thread(
    pid: i32,
    bundle_id: String,
    topics: BTreeSet<AxEventTopic>,
    tx: mpsc::Sender<AxEvent>,
    alive: Arc<AtomicBool>,
    rl_tx: std::sync::mpsc::SyncSender<RunLoopHandle>,
) {
    // Build the AXUIElement for the app pid + an AXObserver bound to a C
    // callback. We leak a Box<CallbackContext> as refcon — released when the
    // observer is removed.

    // SAFETY: AXUIElementCreateApplication returns +1 or NULL.
    let app_ref = unsafe { AXUIElementCreateApplication(pid as accessibility_sys::pid_t) };
    if app_ref.is_null() {
        warn!(pid, %bundle_id, "AXUIElementCreateApplication returned NULL; observer thread exiting");
        let _ = rl_tx.send(RunLoopHandle {
            runloop: CFRunLoop::get_current(),
        });
        return;
    }
    let mut observer: AXObserverRef = std::ptr::null_mut();
    // SAFETY: AXObserverCreate writes a +1 AXObserverRef into the out slot
    // when it returns kAXErrorSuccess.
    let err =
        unsafe { AXObserverCreate(pid as accessibility_sys::pid_t, ax_callback, &mut observer) };
    if err != kAXErrorSuccess || observer.is_null() {
        warn!(pid, %bundle_id, err, "AXObserverCreate failed; observer thread exiting");
        // SAFETY: app_ref is a +1 ref we own; release.
        unsafe { core_foundation_sys::base::CFRelease(app_ref as CFTypeRef) };
        let _ = rl_tx.send(RunLoopHandle {
            runloop: CFRunLoop::get_current(),
        });
        return;
    }

    // refcon = leaked Box<CallbackContext>.
    let ctx = Box::new(CallbackContext {
        bundle_id: bundle_id.clone(),
        tx,
        alive: Arc::clone(&alive),
    });
    let refcon_ptr = Box::into_raw(ctx) as *mut std::ffi::c_void;

    // Register every applicable topic.
    let mut registered: Vec<&'static str> = Vec::new();
    for t in &topics {
        let name = topic_enum_to_str(*t);
        let cf_name = CFString::new(name);
        // SAFETY: observer is +1; app_ref is +1; cf_name is borrowed by
        // AXObserverAddNotification. refcon_ptr is non-null.
        let r = unsafe {
            AXObserverAddNotification(observer, app_ref, cf_name.as_concrete_TypeRef(), refcon_ptr)
        };
        if r != kAXErrorSuccess {
            warn!(%bundle_id, topic = ?t, err = r, "AXObserverAddNotification failed");
            continue;
        }
        registered.push(name);
    }

    let runloop = CFRunLoop::get_current();
    // SAFETY: observer is alive; we get its runloop source.
    let src = unsafe { AXObserverGetRunLoopSource(observer) };
    if !src.is_null() {
        // SAFETY: runloop is the current thread's runloop; mode is borrowed.
        unsafe { CFRunLoopAddSource(runloop.as_concrete_TypeRef(), src, kCFRunLoopDefaultMode) };
    }
    // Hand the runloop back to the spawning thread so Drop can stop it.
    let _ = rl_tx.send(RunLoopHandle {
        runloop: runloop.clone(),
    });
    debug!(%bundle_id, registered = ?registered, "AX observer thread running");

    // Pump until alive goes false (Drop posts CFRunLoopStop).
    CFRunLoop::run_current();

    // Cleanup.
    for name in registered {
        let cf_name = CFString::new(name);
        // SAFETY: observer alive, app_ref alive.
        let _ = unsafe {
            AXObserverRemoveNotification(observer, app_ref, cf_name.as_concrete_TypeRef())
        };
    }
    if !src.is_null() {
        // SAFETY: matched-pair removal.
        unsafe { CFRunLoopRemoveSource(runloop.as_concrete_TypeRef(), src, kCFRunLoopDefaultMode) };
    }
    // SAFETY: observer is +1; release.
    unsafe { core_foundation_sys::base::CFRelease(observer as CFTypeRef) };
    // SAFETY: app_ref is +1; release.
    unsafe { core_foundation_sys::base::CFRelease(app_ref as CFTypeRef) };
    // Reclaim the leaked refcon so its destructor (closing the mpsc
    // sender) runs.
    if !refcon_ptr.is_null() {
        // SAFETY: refcon_ptr was Box::into_raw'd above and not freed.
        unsafe {
            drop(Box::from_raw(refcon_ptr as *mut CallbackContext));
        }
    }
    debug!(%bundle_id, "AX observer thread exited cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_enum_str_round_trip() {
        for t in [
            AxEventTopic::ValueChanged,
            AxEventTopic::FocusedChanged,
            AxEventTopic::WindowCreated,
            AxEventTopic::WindowDestroyed,
            AxEventTopic::SelectionChanged,
        ] {
            let s = topic_enum_to_str(t);
            assert_eq!(topic_str_to_enum(s), Some(t));
        }
    }

    #[test]
    fn topic_parse_known_strings() {
        assert_eq!(
            AxEventTopic::parse("focused_changed"),
            Some(AxEventTopic::FocusedChanged)
        );
        assert_eq!(AxEventTopic::parse("not-a-real-topic"), None);
    }

    #[test]
    fn channel_capacity_constant() {
        assert_eq!(CHANNEL_CAPACITY, 1024);
    }
}
