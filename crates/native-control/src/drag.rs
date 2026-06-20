//! SPEC §12 U7 — `drag.from_finder`, `drag.between_apps`.
//!
//! Drag synthesis on macOS is intricate: a real OS-level drag begins when
//! the user holds-down + moves a mouse button while a particular pasteboard
//! payload is loaded. We synthesize the same event sequence:
//!
//! 1. Load the source payload onto the *drag* pasteboard
//!    (`NSPasteboardName::Drag`).
//! 2. Emit a `LeftMouseDown` at the source point.
//! 3. Emit ≥3 `LeftMouseDragged` events with intermediate positions so the
//!    receiving app's `wantsPeriodicDraggingUpdates` recognizer fires.
//! 4. Emit `LeftMouseUp` at the destination point.
//!
//! `drag.from_finder` is a thin convenience wrapper that builds the payload
//! from a path list before invoking the synthesized sequence.

#![cfg(target_os = "macos")]

use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::types::NativeControlError;

/// Drag from `(from_x, from_y)` to `(to_x, to_y)` after loading `paths`
/// onto the drag pasteboard. Caller is responsible for screen coordinates
/// matching the actual Finder window.
pub async fn from_finder(
    paths: Vec<String>,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
) -> Result<(), NativeControlError> {
    if paths.is_empty() {
        return Err(NativeControlError::Internal(
            "drag.from_finder requires ≥1 path".into(),
        ));
    }
    // Load drag pasteboard with file URLs first.
    load_drag_pasteboard_files(paths)?;
    drag_sequence(from_x, from_y, to_x, to_y).await
}

/// Generic drag — caller has prepared whatever payload was needed.
pub async fn between_apps(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
) -> Result<(), NativeControlError> {
    drag_sequence(from_x, from_y, to_x, to_y).await
}

fn load_drag_pasteboard_files(paths: Vec<String>) -> Result<(), NativeControlError> {
    use objc2::msg_send;
    use objc2::msg_send_id;
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSArray, NSString, NSURL};

    let name = NSString::from_str("Apple CFPasteboard drag");
    // Resolve NSPasteboard's class via the runtime so we don't depend on the
    // `class!` macro being in scope.
    let cls = AnyClass::get("NSPasteboard")
        .ok_or_else(|| NativeControlError::Internal("NSPasteboard class not registered".into()))?;
    // SAFETY: pasteboardWithName: returns +1; msg_send_id! handles ownership.
    let pb: Retained<NSPasteboard> = unsafe { msg_send_id![cls, pasteboardWithName: &*name] };
    unsafe {
        pb.clearContents();
    }
    let mut urls: Vec<Retained<NSURL>> = Vec::with_capacity(paths.len());
    for p in &paths {
        let path_str = NSString::from_str(p);
        // SAFETY: fileURLWithPath returns a +1 NSURL for any path string.
        let url: Retained<NSURL> = unsafe { NSURL::fileURLWithPath(&path_str) };
        urls.push(url);
    }
    let arr: Retained<NSArray<NSURL>> = NSArray::from_id_slice(&urls);
    // writeObjects: takes `NSArray<ProtocolObject<NSPasteboardWriting>>`.
    // NSURL conforms to NSPasteboardWriting, but objc2's typed binding wants
    // an explicit cast. We dispatch via raw msg_send to bypass the typed
    // wrapper — Cocoa's runtime accepts the array as-is.
    // SAFETY: writeObjects: takes any NSArray of NSPasteboardWriting-conforming
    // objects; NSURL conforms.
    let _ok: bool = unsafe { msg_send![&*pb, writeObjects:&*arr] };
    Ok(())
}

async fn drag_sequence(
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
) -> Result<(), NativeControlError> {
    tokio::task::spawn_blocking(move || drag_blocking(from_x, from_y, to_x, to_y))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn drag_blocking(from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> Result<(), NativeControlError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
        NativeControlError::Internal("CGEventSource::new(HIDSystemState) failed".into())
    })?;
    let from = CGPoint::new(from_x, from_y);
    let to = CGPoint::new(to_x, to_y);

    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::LeftMouseDown,
        from,
        CGMouseButton::Left,
    )
    .map_err(|_| NativeControlError::Internal("LeftMouseDown event create failed".into()))?;
    down.post(CGEventTapLocation::HID);

    // Inter-step dragged events. ≥3 so the recognizer fires.
    let steps = 12;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let xx = from_x + (to_x - from_x) * t;
        let yy = from_y + (to_y - from_y) * t;
        let dragged = CGEvent::new_mouse_event(
            source.clone(),
            CGEventType::LeftMouseDragged,
            CGPoint::new(xx, yy),
            CGMouseButton::Left,
        )
        .map_err(|_| NativeControlError::Internal("LeftMouseDragged create failed".into()))?;
        dragged.post(CGEventTapLocation::HID);
        // Brief sleep so the receiving app has a chance to react before the
        // next event lands. ~10ms total gap matches a slow user drag.
        std::thread::sleep(std::time::Duration::from_millis(8));
    }

    let up = CGEvent::new_mouse_event(source, CGEventType::LeftMouseUp, to, CGMouseButton::Left)
        .map_err(|_| NativeControlError::Internal("LeftMouseUp event create failed".into()))?;
    up.post(CGEventTapLocation::HID);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_paths_rejected() {
        let r = from_finder(vec![], 0.0, 0.0, 100.0, 100.0).await;
        match r {
            Err(NativeControlError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
