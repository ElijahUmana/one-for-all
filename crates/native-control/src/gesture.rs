//! SPEC §12 U6 — `app.gesture.three_finger_swipe` + `app.force_touch`.
//!
//! Both verbs synthesize CGEvents at the systemwide event tap. Force touch
//! uses `CGEventCreateMouseEvent` with the `kCGMouseEventPressure` field;
//! three-finger swipe uses `CGEventCreateScrollWheelEvent2` with the
//! `kCGScrollWheelEventScrollPhase` field set to begin/changed/end.
//!
//! Focus discipline: gestures are applied at the systemwide event tap (not
//! `post_to_pid`) — they affect whatever window is under the cursor, which
//! by definition is the user's current focus. Not classified as
//! focus-stealing here because the gesture itself does not raise an app —
//! the underlying window's reaction is whatever its default handler does.

#![cfg(target_os = "macos")]

use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::types::NativeControlError;

/// Synthesize a three-finger swipe with the given pixel deltas. Posted as a
/// scroll wheel sequence with `phase = began → changed → ended` so the
/// receiving window's gesture recognizer treats it as a real swipe rather
/// than a discrete wheel tick.
pub async fn three_finger_swipe(dx: f64, dy: f64) -> Result<(), NativeControlError> {
    tokio::task::spawn_blocking(move || swipe_blocking(dx, dy))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn swipe_blocking(dx: f64, dy: f64) -> Result<(), NativeControlError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
        NativeControlError::Internal("CGEventSource::new(HIDSystemState) failed".into())
    })?;
    // Three-finger swipe is dispatched as a continuous scroll. We synthesize
    // begin/changed/end. The phase field is set via the SetIntegerValueField
    // accessor which the core-graphics crate exposes through `set_integer_value_field`.
    let total_steps = 8u32;
    let dx_step = (dx / total_steps as f64).round() as i32;
    let dy_step = (dy / total_steps as f64).round() as i32;
    for step in 0..total_steps {
        let evt = CGEvent::new_scroll_event(
            source.clone(),
            ScrollEventUnit::PIXEL,
            2,
            dy_step,
            dx_step,
            0,
        )
        .map_err(|_| NativeControlError::Internal("CGEvent::new_scroll_event failed".into()))?;
        // Phase: 1 = began, 2 = changed, 4 = ended (kCGScrollPhaseBegan etc.)
        let phase: i64 = if step == 0 {
            1
        } else if step + 1 == total_steps {
            4
        } else {
            2
        };
        // 99 == kCGScrollWheelEventScrollPhase.
        evt.set_integer_value_field(99, phase);
        evt.post(CGEventTapLocation::HID);
    }
    Ok(())
}

/// Synthesize a force-touch (deep press) at screen coordinate `(x, y)` with
/// the given normalized pressure (0.0 – 1.0).
pub async fn force_touch(x: f64, y: f64, pressure: f64) -> Result<(), NativeControlError> {
    if !(0.0..=1.0).contains(&pressure) {
        return Err(NativeControlError::Internal(
            "pressure must be in [0,1]".into(),
        ));
    }
    tokio::task::spawn_blocking(move || force_touch_blocking(x, y, pressure))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn force_touch_blocking(x: f64, y: f64, pressure: f64) -> Result<(), NativeControlError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
        NativeControlError::Internal("CGEventSource::new(HIDSystemState) failed".into())
    })?;
    let pt = CGPoint::new(x, y);
    let down = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::LeftMouseDown,
        pt,
        CGMouseButton::Left,
    )
    .map_err(|_| NativeControlError::Internal("mouse-down event create failed".into()))?;
    // 24 = kCGMouseEventPressure (CGEventField). Field is a u32 fixed-point
    // 0..255 in classic Quartz; we convert from f64 0..1.
    let p_int = (pressure.clamp(0.0, 1.0) * 255.0).round() as i64;
    down.set_integer_value_field(24, p_int);
    // 25 = kCGMouseEventStage (force touch stages 0..2 + click). 2 = deep
    // press.
    down.set_integer_value_field(25, 2);
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_mouse_event(source, CGEventType::LeftMouseUp, pt, CGMouseButton::Left)
        .map_err(|_| NativeControlError::Internal("mouse-up event create failed".into()))?;
    up.post(CGEventTapLocation::HID);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pressure_out_of_range_rejected() {
        let r = force_touch(0.0, 0.0, 1.5).await;
        match r {
            Err(NativeControlError::Internal(msg)) => assert!(msg.contains("pressure")),
            other => panic!("expected Internal, got {other:?}"),
        }
        let r = force_touch(0.0, 0.0, -0.1).await;
        match r {
            Err(NativeControlError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
