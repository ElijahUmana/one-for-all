//! High-level interactions: click, type, keypress, scroll, hover, drag, eval.
//!
//! Implements SPEC §7 `page.*` action methods. Element resolution happens
//! by `ref` (from a snapshot) — the broker validates the ref scope and
//! returns `-32004 ElementStale` if the ref is from an older snapshot.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use cdp_client::generated::domains::{
    emulation as cdp_emulation, input as cdp_input, network as cdp_network, page as cdp_page,
    runtime as cdp_runtime,
};
use observability::trace::{TraceEvent, TraceSink};
use serde_json::{json, Value};

use crate::input_translation::{
    bezier_mouse_path, build_file_drag_data, cdp_key_down, cdp_key_raw_down, cdp_key_up,
    cdp_mouse_move, cdp_mouse_press, cdp_mouse_wheel, cdp_pointer_event, cdp_touch_event,
    char_to_cdp_key, compose_dead_key, dead_key_accent_hint, key_code_for_token, parse_accelerator,
    typing_delay, wheel_profile, MouseButton, PointerDetails, Rng, ScrollEasing, TouchContact,
};
use crate::page::Page;
use crate::snapshot::{BBox, Element};

/// What the broker hands us along with a `ref`.
pub struct ResolvedElement<'a> {
    pub element: &'a Element,
}

impl<'a> ResolvedElement<'a> {
    pub fn center(&self) -> (f64, f64) {
        let b = &self.element.bbox;
        (b.x + b.w / 2.0, b.y + b.h / 2.0)
    }
}

/// Result of a click that triggered a navigation.
#[derive(Debug, Clone)]
pub struct NavOutcome {
    pub frame_id: String,
    pub url: String,
}

impl Page {
    /// Implements `page.click` (SPEC §7).
    ///
    /// `realistic = true` uses the Bezier path; `false` (default) issues a
    /// single move + press + release at the element's center.
    ///
    /// SPEC §10 M10 — when a trace sink is attached to the parent
    /// [`Browser`] this also records a post-click `Screenshot` event.
    pub async fn click(
        &self,
        elem: &Element,
        button: MouseButton,
        click_count: u32,
        realistic: bool,
        rng_seed: u64,
    ) -> Result<Option<NavOutcome>> {
        bbox_must_be_actionable(&elem.bbox)?;
        let (cx, cy) = (
            elem.bbox.x + elem.bbox.w / 2.0,
            elem.bbox.y + elem.bbox.h / 2.0,
        );

        if realistic {
            let mut rng = Rng::new(rng_seed.max(1));
            let path = bezier_mouse_path((cx - 60.0, cy - 30.0), (cx, cy), 18, &mut rng);
            for (x, y, sleep_ms) in path {
                self.cdp_send(cdp_mouse_move(x, y)).await?;
                if sleep_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(sleep_ms as u64)).await;
                }
            }
        } else {
            self.cdp_send(cdp_mouse_move(cx, cy)).await?;
        }

        // Subscribe to nav events so we can report whether the click triggered
        // a navigation. Subscribe BEFORE dispatching the press to avoid race.
        let mut nav_rx = self.nav_subscribe();

        self.cdp_send(cdp_mouse_press(cx, cy, button, click_count, true))
            .await?;
        self.cdp_send(cdp_mouse_press(cx, cy, button, click_count, false))
            .await?;

        // SPEC §10 M10 — trace post-click screenshot.
        if let Some(sink) = self.browser().trace_sink() {
            if let Err(e) = capture_action_screenshot(self, &sink, "page.click").await {
                tracing::debug!(error = %e, "click trace screenshot failed (non-fatal)");
            }
        }

        // Wait briefly for a frameNavigated; if none arrives, return None.
        match tokio::time::timeout(Duration::from_millis(500), nav_rx.recv()).await {
            Ok(Ok(url)) => Ok(Some(NavOutcome {
                frame_id: String::new(),
                url,
            })),
            _ => Ok(None),
        }
    }

    /// Implements `page.type` (SPEC §7). `clear_first` is best-effort via
    /// Cmd+A / Backspace.
    pub async fn type_text(
        &self,
        elem: &Element,
        text: &str,
        delay_ms: Option<u64>,
        clear_first: bool,
        rng_seed: u64,
    ) -> Result<()> {
        // Click into the field first so it has focus.
        self.click(elem, MouseButton::Left, 1, false, rng_seed)
            .await?;

        if clear_first {
            // Cmd+A on macOS, Ctrl+A elsewhere. Issue both — whichever
            // matches will succeed.
            let _ = self
                .cdp_send(cdp_key_down("a", "KeyA", None, 4 /* Meta */))
                .await;
            let _ = self.cdp_send(cdp_key_up("a", "KeyA", 4)).await;
            let _ = self
                .cdp_send(cdp_key_down("Backspace", "Backspace", None, 0))
                .await;
            let _ = self.cdp_send(cdp_key_up("Backspace", "Backspace", 0)).await;
        }

        let mut rng = Rng::new(rng_seed.max(1));
        for ch in text.chars() {
            let (key, code, text_str) = char_to_cdp_key(ch);
            let text_arg = if text_str.is_empty() {
                None
            } else {
                Some(text_str.as_str())
            };
            self.cdp_send(cdp_key_down(&key, &code, text_arg, 0))
                .await?;
            self.cdp_send(cdp_key_up(&key, &code, 0)).await?;
            let d = delay_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| typing_delay(&mut rng));
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }

        // SPEC §10 M10 — trace post-type screenshot.
        if let Some(sink) = self.browser().trace_sink() {
            if let Err(e) = capture_action_screenshot(self, &sink, "page.type").await {
                tracing::debug!(error = %e, "type trace screenshot failed (non-fatal)");
            }
        }
        Ok(())
    }

    /// Implements `page.keypress` (SPEC §7). `key` is a chord like
    /// "Enter", "Cmd+A", "Shift+Tab".
    pub async fn keypress(&self, key: &str) -> Result<()> {
        let (modifiers, key_name) = parse_chord(key);
        self.keypress_with_modifiers(key_name, modifiers as i64)
            .await
    }

    /// Implements the keypress backend with explicit modifier bits.
    pub async fn keypress_with_modifiers(&self, key: &str, modifiers: i64) -> Result<()> {
        let (key_name, code) =
            key_code_for_token(key).map_err(|e| anyhow!("keypress token parse failed: {e}"))?;
        self.cdp_send(cdp_key_raw_down(&key_name, &code, modifiers))
            .await?;
        self.cdp_send(cdp_key_up(&key_name, &code, modifiers))
            .await?;
        Ok(())
    }

    /// Implements `page.scroll` (SPEC §7). If `elem` is `None`, scroll the
    /// document; otherwise scroll the element's center.
    pub async fn scroll(&self, elem: Option<&Element>, dx: f64, dy: f64) -> Result<()> {
        self.precise_scroll(elem, dx, dy, false, ScrollEasing::Linear)
            .await
    }

    /// Implements `page.scroll.precise` (SPEC §12 U1).
    // CANCELLATION: multi-step wheel dispatch is best-effort. Cancellation may
    // stop mid-profile, leaving the page partially scrolled, but it never holds
    // external resources or background tasks beyond the current awaits.
    pub async fn precise_scroll(
        &self,
        elem: Option<&Element>,
        dx: f64,
        dy: f64,
        momentum: bool,
        easing: ScrollEasing,
    ) -> Result<()> {
        let (x, y) = if let Some(e) = elem {
            bbox_must_be_actionable(&e.bbox)?;
            (e.bbox.x + e.bbox.w / 2.0, e.bbox.y + e.bbox.h / 2.0)
        } else {
            (1.0, 1.0)
        };
        for step in wheel_profile(dx, dy, momentum, easing) {
            self.cdp_send(cdp_mouse_wheel(x, y, step.dx, step.dy))
                .await?;
            if step.sleep_ms > 0 {
                tokio::time::sleep(Duration::from_millis(step.sleep_ms)).await;
            }
        }
        capture_action_trace(self, "page.scroll.precise").await;
        Ok(())
    }

    /// Implements `page.hover` (SPEC §7).
    pub async fn hover(&self, elem: &Element) -> Result<()> {
        bbox_must_be_actionable(&elem.bbox)?;
        let (cx, cy) = (
            elem.bbox.x + elem.bbox.w / 2.0,
            elem.bbox.y + elem.bbox.h / 2.0,
        );
        self.cdp_send(cdp_mouse_move(cx, cy)).await?;
        Ok(())
    }

    /// Implements `page.drag` (SPEC §7).
    pub async fn drag(&self, from: &Element, to: &Element, rng_seed: u64) -> Result<()> {
        bbox_must_be_actionable(&from.bbox)?;
        bbox_must_be_actionable(&to.bbox)?;
        let (fx, fy) = (
            from.bbox.x + from.bbox.w / 2.0,
            from.bbox.y + from.bbox.h / 2.0,
        );
        let (tx, ty) = (to.bbox.x + to.bbox.w / 2.0, to.bbox.y + to.bbox.h / 2.0);

        let mut rng = Rng::new(rng_seed.max(1));
        self.cdp_send(cdp_mouse_move(fx, fy)).await?;
        self.cdp_send(cdp_mouse_press(fx, fy, MouseButton::Left, 1, true))
            .await?;
        let path = bezier_mouse_path((fx, fy), (tx, ty), 24, &mut rng);
        for (x, y, sleep_ms) in path {
            self.cdp_send(cdp_mouse_move(x, y)).await?;
            tokio::time::sleep(Duration::from_millis(sleep_ms as u64)).await;
        }
        self.cdp_send(cdp_mouse_press(tx, ty, MouseButton::Left, 1, false))
            .await?;
        Ok(())
    }

    /// Implements `page.touch.tap` (SPEC §12 U1).
    // CANCELLATION: cancellation can stop between touchstart/touchend events,
    // which may leave a partial gesture in-flight for the page, but no durable
    // state or background task survives beyond the current async call.
    pub async fn touch_tap(&self, x: f64, y: f64, duration_ms: u64, tap_count: u32) -> Result<()> {
        self.cdp_send(cdp_emulation::SetTouchEmulationEnabledParams {
            enabled: true,
            max_touch_points: Some(2),
        })
        .await?;
        let taps = tap_count.max(1);
        for idx in 0..taps {
            let contact = TouchContact::new(1, x, y);
            self.cdp_send(cdp_touch_event("touchStart", &[contact]))
                .await?;
            tokio::time::sleep(Duration::from_millis(duration_ms.max(1))).await;
            self.cdp_send(cdp_touch_event("touchEnd", &[])).await?;
            if idx + 1 != taps {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        capture_action_trace(self, "page.touch.tap").await;
        Ok(())
    }

    /// Implements `page.touch.swipe` (SPEC §12 U1).
    // CANCELLATION: cancellation can stop a swipe mid-path, leaving the page in
    // an intermediate gesture state, but does not leak tasks or resources.
    pub async fn touch_swipe(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        steps: u32,
        duration_ms: u64,
    ) -> Result<()> {
        self.cdp_send(cdp_emulation::SetTouchEmulationEnabledParams {
            enabled: true,
            max_touch_points: Some(2),
        })
        .await?;
        let steps = steps.max(1);
        let per_step = (duration_ms / steps as u64).max(1);
        let start = TouchContact::new(1, from.0, from.1);
        self.cdp_send(cdp_touch_event("touchStart", &[start.clone()]))
            .await?;
        for idx in 1..=steps {
            let t = idx as f64 / steps as f64;
            let x = from.0 + (to.0 - from.0) * t;
            let y = from.1 + (to.1 - from.1) * t;
            let mut contact = start.clone();
            contact.x = x;
            contact.y = y;
            self.cdp_send(cdp_touch_event("touchMove", &[contact]))
                .await?;
            tokio::time::sleep(Duration::from_millis(per_step)).await;
        }
        self.cdp_send(cdp_touch_event("touchEnd", &[])).await?;
        capture_action_trace(self, "page.touch.swipe").await;
        Ok(())
    }

    /// Implements `page.touch.pinch` (SPEC §12 U1).
    // CANCELLATION: cancellation can stop the two-finger gesture mid-sequence,
    // but does not leave background work running after the future is dropped.
    pub async fn touch_pinch(
        &self,
        center_x: f64,
        center_y: f64,
        start_radius: f64,
        end_radius: f64,
        steps: u32,
        duration_ms: u64,
    ) -> Result<()> {
        if start_radius <= 0.0 || end_radius <= 0.0 {
            return Err(anyhow!("pinch radii must be > 0"));
        }
        self.cdp_send(cdp_emulation::SetTouchEmulationEnabledParams {
            enabled: true,
            max_touch_points: Some(2),
        })
        .await?;
        let steps = steps.max(1);
        let per_step = (duration_ms / steps as u64).max(1);
        let left = TouchContact::new(1, center_x - start_radius, center_y);
        let right = TouchContact::new(2, center_x + start_radius, center_y);
        self.cdp_send(cdp_touch_event(
            "touchStart",
            &[left.clone(), right.clone()],
        ))
        .await?;
        for idx in 1..=steps {
            let t = idx as f64 / steps as f64;
            let radius = start_radius + (end_radius - start_radius) * t;
            let c1 = TouchContact::new(1, center_x - radius, center_y);
            let c2 = TouchContact::new(2, center_x + radius, center_y);
            self.cdp_send(cdp_touch_event("touchMove", &[c1, c2]))
                .await?;
            tokio::time::sleep(Duration::from_millis(per_step)).await;
        }
        self.cdp_send(cdp_touch_event("touchEnd", &[])).await?;
        capture_action_trace(self, "page.touch.pinch").await;
        Ok(())
    }

    /// Implements `page.touch.rotate` (SPEC §12 U1).
    // CANCELLATION: cancellation may stop a rotation mid-motion, but no
    // background work survives and the gesture sequence remains bounded.
    pub async fn touch_rotate(
        &self,
        center_x: f64,
        center_y: f64,
        radius: f64,
        angle_deg: f64,
        steps: u32,
        duration_ms: u64,
    ) -> Result<()> {
        if radius <= 0.0 {
            return Err(anyhow!("rotation radius must be > 0"));
        }
        self.cdp_send(cdp_emulation::SetTouchEmulationEnabledParams {
            enabled: true,
            max_touch_points: Some(2),
        })
        .await?;
        let steps = steps.max(1);
        let per_step = (duration_ms / steps as u64).max(1);
        let first = TouchContact::new(1, center_x - radius, center_y);
        let second = TouchContact::new(2, center_x + radius, center_y);
        self.cdp_send(cdp_touch_event("touchStart", &[first, second]))
            .await?;
        for idx in 1..=steps {
            let t = idx as f64 / steps as f64;
            let angle = angle_deg.to_radians() * t;
            let c1 = rotated_contact(1, center_x, center_y, radius, angle);
            let c2 = rotated_contact(2, center_x, center_y, radius, angle + std::f64::consts::PI);
            self.cdp_send(cdp_touch_event("touchMove", &[c1, c2]))
                .await?;
            tokio::time::sleep(Duration::from_millis(per_step)).await;
        }
        self.cdp_send(cdp_touch_event("touchEnd", &[])).await?;
        capture_action_trace(self, "page.touch.rotate").await;
        Ok(())
    }

    /// Implements `page.pointer.press` (SPEC §12 U1).
    // CANCELLATION: emits a single pen-down event. Cancellation cannot leave
    // lingering tasks; follow-up move/release calls remain explicitly modeled.
    pub async fn pointer_press(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        pointer: PointerDetails,
    ) -> Result<()> {
        self.cdp_send(cdp_pointer_event(
            "mousePressed",
            x,
            y,
            Some(button),
            button.buttons_mask(),
            Some(click_count.max(1)),
            pointer,
        ))
        .await?;
        capture_action_trace(self, "page.pointer.press").await;
        Ok(())
    }

    /// Implements `page.pointer.move` (SPEC §12 U1).
    // CANCELLATION: emits a single pen-move event and does not retain any
    // background work after the current future is cancelled.
    pub async fn pointer_move(
        &self,
        x: f64,
        y: f64,
        buttons: i64,
        pointer: PointerDetails,
    ) -> Result<()> {
        self.cdp_send(cdp_pointer_event(
            "mouseMoved",
            x,
            y,
            None,
            buttons,
            None,
            pointer,
        ))
        .await?;
        capture_action_trace(self, "page.pointer.move").await;
        Ok(())
    }

    /// Implements `page.pointer.release` (SPEC §12 U1).
    // CANCELLATION: emits a single pen-up event. Button state is modeled by
    // the caller, so dropping the future only skips the release dispatch.
    pub async fn pointer_release(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        pointer: PointerDetails,
    ) -> Result<()> {
        self.cdp_send(cdp_pointer_event(
            "mouseReleased",
            x,
            y,
            Some(button),
            button.buttons_mask(),
            Some(click_count.max(1)),
            pointer,
        ))
        .await?;
        capture_action_trace(self, "page.pointer.release").await;
        Ok(())
    }

    /// Implements `page.gesture.pinch` (SPEC §12 U1).
    // CANCELLATION: delegates to the bounded pinch composer; see
    // [`Self::touch_pinch`] for the partial-gesture semantics.
    pub async fn gesture_pinch(
        &self,
        center_x: f64,
        center_y: f64,
        start_radius: f64,
        scale_factor: f64,
        steps: u32,
        duration_ms: u64,
    ) -> Result<()> {
        self.touch_pinch(
            center_x,
            center_y,
            start_radius,
            start_radius * scale_factor,
            steps,
            duration_ms,
        )
        .await?;
        capture_action_trace(self, "page.gesture.pinch").await;
        Ok(())
    }

    /// Implements `page.gesture.rotate` (SPEC §12 U1).
    // CANCELLATION: delegates to the bounded rotate composer; see
    // [`Self::touch_rotate`] for interruption behavior.
    pub async fn gesture_rotate(
        &self,
        center_x: f64,
        center_y: f64,
        radius: f64,
        angle_deg: f64,
        steps: u32,
        duration_ms: u64,
    ) -> Result<()> {
        self.touch_rotate(center_x, center_y, radius, angle_deg, steps, duration_ms)
            .await?;
        capture_action_trace(self, "page.gesture.rotate").await;
        Ok(())
    }

    /// Implements `page.gesture.longpress` (SPEC §12 U1).
    // CANCELLATION: delegates to a bounded touch tap with a long duration;
    // cancellation can interrupt before touchEnd but cannot leak tasks.
    pub async fn gesture_longpress(&self, x: f64, y: f64, duration_ms: u64) -> Result<()> {
        self.touch_tap(x, y, duration_ms.max(300), 1).await?;
        capture_action_trace(self, "page.gesture.longpress").await;
        Ok(())
    }

    /// Implements `page.drag.file_drop` (SPEC §12 U1).
    // CANCELLATION: the drag dispatch is a short, bounded sequence. If
    // cancelled mid-flight the page may see only part of the synthetic drop,
    // but no background work or intercepted-drag mode is left behind.
    pub async fn file_drop(&self, x: f64, y: f64, file_paths: &[String]) -> Result<()> {
        if file_paths.is_empty() {
            return Err(anyhow!("file_drop requires at least one path"));
        }
        let data = build_file_drag_data(file_paths);
        self.cdp_send(cdp_input::DispatchDragEventParams {
            r#type: "dragEnter".to_owned(),
            x,
            y,
            data: data.clone(),
            modifiers: None,
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.cdp_send(cdp_input::DispatchDragEventParams {
            r#type: "dragOver".to_owned(),
            x,
            y,
            data: data.clone(),
            modifiers: None,
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.cdp_send(cdp_input::DispatchDragEventParams {
            r#type: "drop".to_owned(),
            x,
            y,
            data,
            modifiers: None,
        })
        .await?;
        capture_action_trace(self, "page.drag.file_drop").await;
        Ok(())
    }

    /// Implements `page.keyboard.shortcut` (SPEC §12 U1).
    // CANCELLATION: emits one rawKeyDown/keyUp pair and never spawns
    // background work, so cancellation is equivalent to dropping the future
    // before the second dispatch.
    pub async fn keyboard_shortcut(&self, accel: &str) -> Result<()> {
        let accel =
            parse_accelerator(accel).map_err(|e| anyhow!("accelerator parse failed: {e}"))?;
        self.cdp_send(cdp_key_raw_down(&accel.key, &accel.code, accel.modifiers))
            .await?;
        self.cdp_send(cdp_key_up(&accel.key, &accel.code, accel.modifiers))
            .await?;
        capture_action_trace(self, "page.keyboard.shortcut").await;
        Ok(())
    }

    /// Implements `page.keyboard.ime` (SPEC §12 U1).
    // CANCELLATION: composition is applied synchronously. Cancellation may
    // leave the candidate string visible if it lands between set/clear calls,
    // but does not leave background tasks running.
    pub async fn keyboard_ime(&self, composition_string: &str, commit: &str) -> Result<()> {
        let len = composition_string.chars().count() as i64;
        self.cdp_send(cdp_input::ImeSetCompositionParams {
            text: composition_string.to_owned(),
            selection_start: len,
            selection_end: len,
            replacement_start: None,
            replacement_end: None,
        })
        .await?;
        if !commit.is_empty() {
            self.cdp_send(cdp_input::InsertTextParams {
                text: commit.to_owned(),
            })
            .await?;
        }
        self.cdp_send(cdp_input::ImeSetCompositionParams {
            text: String::new(),
            selection_start: 0,
            selection_end: 0,
            replacement_start: Some(0),
            replacement_end: Some(0),
        })
        .await?;
        capture_action_trace(self, "page.keyboard.ime").await;
        Ok(())
    }

    /// Implements `page.dead_key` (SPEC §12 U1).
    // CANCELLATION: the accent hint and committed composed character are sent
    // synchronously. Cancellation may stop before the final insertText, but no
    // background task survives beyond the current future.
    pub async fn dead_key(&self, accent: &str, base: char) -> Result<String> {
        let composed = compose_dead_key(accent, base)
            .ok_or_else(|| anyhow!("unsupported dead-key composition {accent:?}+{base:?}"))?;
        if let Some(accel) = dead_key_accent_hint(accent) {
            self.cdp_send(cdp_key_raw_down(&accel.key, &accel.code, accel.modifiers))
                .await?;
            self.cdp_send(cdp_key_up(&accel.key, &accel.code, accel.modifiers))
                .await?;
        }
        self.cdp_send(cdp_input::InsertTextParams {
            text: composed.clone(),
        })
        .await?;
        capture_action_trace(self, "page.dead_key").await;
        Ok(composed)
    }

    /// Implements `page.tab_traversal` (SPEC §12 U1).
    // CANCELLATION: traversal is a finite loop over `Tab` keypresses. Dropping
    // the future stops at the current focus position with no leaked tasks.
    pub async fn tab_traversal(&self, direction: &str, count: u32) -> Result<()> {
        let modifiers = match direction {
            "forward" => 0,
            "backward" => 8,
            other => return Err(anyhow!("unsupported tab traversal direction {other:?}")),
        };
        for _ in 0..count.max(1) {
            self.keypress_with_modifiers("Tab", modifiers).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        capture_action_trace(self, "page.tab_traversal").await;
        Ok(())
    }

    /// Implements `page.right_click_menu_navigate` (SPEC §12 U1).
    // CANCELLATION: the right-click and DOM-menu traversal are a bounded local
    // sequence. Cancellation may leave the menu open, but no external resource
    // or background task survives after the future is dropped.
    pub async fn right_click_menu_navigate(
        &self,
        elem: &Element,
        item_path: &[String],
    ) -> Result<()> {
        bbox_must_be_actionable(&elem.bbox)?;
        let (x, y) = (
            elem.bbox.x + elem.bbox.w / 2.0,
            elem.bbox.y + elem.bbox.h / 2.0,
        );
        self.cdp_send(cdp_mouse_move(x, y)).await?;
        self.cdp_send(cdp_mouse_press(x, y, MouseButton::Right, 1, true))
            .await?;
        self.cdp_send(cdp_mouse_press(x, y, MouseButton::Right, 1, false))
            .await?;
        tokio::time::sleep(Duration::from_millis(120)).await;

        let path_json = serde_json::to_string(item_path).context("serialize item_path")?;
        let expr = [
            "(() => {",
            &format!("const path = {path_json};"),
            "const visible = (el) => { const r = el.getBoundingClientRect(); const style = window.getComputedStyle(el); return !!style && style.display !== 'none' && style.visibility !== 'hidden' && r.width > 0 && r.height > 0; };",
            "const labelOf = (el) => (el.getAttribute('aria-label') || el.innerText || el.textContent || '').replace(/\\s+/g, ' ').trim();",
            "let last = null;",
            "for (const segment of path) {",
            "  const roots = Array.from(document.querySelectorAll('[role=menu],[data-menu-root],[data-context-menu-root],.context-menu,.menu')).filter(visible);",
            "  const searchRoots = roots.length ? roots : [document.body];",
            "  let found = null;",
            "  for (const root of searchRoots) {",
            "    const candidates = Array.from(root.querySelectorAll('[role=menuitem],[role=option],button,[data-menu-item],li,a,div,span')).filter((el) => visible(el) && labelOf(el) === segment);",
            "    if (candidates.length) { found = candidates[0]; break; }",
            "  }",
            "  if (!found) { return { ok: false, missing: segment }; }",
            "  for (const type of ['pointerover','mouseover','mouseenter']) { found.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window })); }",
            "  last = found;",
            "}",
            "if (!last) { return { ok: false, missing: '<empty>' }; }",
            "last.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true, view: window, button: 0, buttons: 1 }));",
            "last.click();",
            "last.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, cancelable: true, view: window, button: 0, buttons: 0 }));",
            "return { ok: true };",
            "})()",
        ]
        .join("\n");
        let result = self.eval(&expr, true).await?;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            capture_action_trace(self, "page.right_click_menu_navigate").await;
            return Ok(());
        }
        let missing = result
            .get("missing")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        Err(anyhow!("context-menu path segment not found: {missing}"))
    }

    /// Implements `page.eval` (SPEC §7).
    pub async fn eval(&self, expression: &str, return_by_value: bool) -> Result<Value> {
        let res = self
            .cdp_send(cdp_runtime::EvaluateParams {
                expression: expression.to_owned(),
                return_by_value: Some(return_by_value),
                await_promise: Some(true),
                ..Default::default()
            })
            .await
            .context("Runtime.evaluate")?;
        if let Some(exc) = &res.exception_details {
            return Err(anyhow!("eval threw: {exc}"));
        }
        Ok(res.result)
    }

    /// Implements `page.read_text` (SPEC §7).
    pub async fn read_text(&self, elem: Option<&Element>) -> Result<String> {
        let expr = match elem {
            None => "document.body && document.body.innerText || ''".to_owned(),
            Some(e) => format!(
                "(()=>{{const r=document.elementFromPoint({},{});return r ? r.innerText : '';}})()",
                e.bbox.x + e.bbox.w / 2.0,
                e.bbox.y + e.bbox.h / 2.0
            ),
        };
        let res = self
            .cdp_send(cdp_runtime::EvaluateParams {
                expression: expr,
                return_by_value: Some(true),
                ..Default::default()
            })
            .await?;
        Ok(res
            .result
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned())
    }

    /// Implements `page.screenshot` (SPEC §7).
    pub async fn screenshot(
        &self,
        format: &str,
        quality: Option<u8>,
        capture_beyond_viewport: bool,
        clip: Option<&BBox>,
    ) -> Result<String> {
        let res = self
            .cdp_send(cdp_page::CaptureScreenshotParams {
                format: Some(format.to_owned()),
                quality: quality.map(|q| q as i64),
                capture_beyond_viewport: Some(capture_beyond_viewport),
                clip: clip.map(|c| {
                    json!({
                        "x": c.x,
                        "y": c.y,
                        "width": c.w,
                        "height": c.h,
                        "scale": 1.0,
                    })
                }),
                ..Default::default()
            })
            .await
            .context("Page.captureScreenshot")?;
        Ok(res.data)
    }

    /// Implements `page.viewport` (SPEC §7).
    pub async fn set_viewport(
        &self,
        width: u32,
        height: u32,
        device_scale_factor: f64,
        mobile: bool,
    ) -> Result<()> {
        self.cdp_send(cdp_emulation::SetDeviceMetricsOverrideParams {
            width: width as i64,
            height: height as i64,
            device_scale_factor,
            mobile,
            ..Default::default()
        })
        .await?;
        Ok(())
    }

    /// Implements `page.user_agent` (SPEC §7).
    pub async fn set_user_agent(
        &self,
        ua: &str,
        accept_language: Option<&str>,
        platform: Option<&str>,
    ) -> Result<()> {
        self.cdp_send(cdp_network::SetUserAgentOverrideParams {
            user_agent: ua.to_owned(),
            accept_language: accept_language.map(str::to_owned),
            platform: platform.map(str::to_owned),
            ..Default::default()
        })
        .await?;
        Ok(())
    }

    /// Implements `page.geo` (SPEC §7).
    pub async fn set_geo(&self, lat: f64, lon: f64, accuracy: f64) -> Result<()> {
        self.cdp_send(cdp_emulation::SetGeolocationOverrideParams {
            latitude: Some(lat),
            longitude: Some(lon),
            accuracy: Some(accuracy),
            ..Default::default()
        })
        .await?;
        Ok(())
    }

    /// Implements `page.dark_mode` (SPEC §7).
    pub async fn set_dark_mode(&self, enabled: bool) -> Result<()> {
        let scheme = if enabled { "dark" } else { "light" };
        self.cdp_send(cdp_emulation::SetEmulatedMediaParams {
            features: Some(json!([{
                "name": "prefers-color-scheme",
                "value": scheme
            }])),
            ..Default::default()
        })
        .await?;
        Ok(())
    }
}

fn bbox_must_be_actionable(bbox: &BBox) -> Result<()> {
    if bbox.w <= 0.0 || bbox.h <= 0.0 {
        // Maps to SPEC -32005 ElementNotActionable in the broker.
        return Err(anyhow!("element has zero area"));
    }
    Ok(())
}

fn rotated_contact(id: i64, center_x: f64, center_y: f64, radius: f64, angle: f64) -> TouchContact {
    let mut contact = TouchContact::new(
        id,
        center_x + radius * angle.cos(),
        center_y + radius * angle.sin(),
    );
    contact.rotation_angle = angle.to_degrees();
    contact
}

async fn capture_action_trace(page: &Page, action: &str) {
    if let Some(sink) = page.browser().trace_sink() {
        if let Err(e) = capture_action_screenshot(page, &sink, action).await {
            tracing::debug!(error = %e, %action, "action trace screenshot failed (non-fatal)");
        }
    }
}

/// Parse a chord like "Cmd+Shift+A" → (modifiers_bitmask, "A").
/// CDP modifier bits: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub fn parse_chord(chord: &str) -> (u32, &str) {
    let mut bits = 0u32;
    let mut tail = chord;
    while let Some(idx) = tail.find('+') {
        let (mod_part, rest) = tail.split_at(idx);
        match mod_part.trim() {
            "Alt" | "Option" => bits |= 1,
            "Ctrl" | "Control" => bits |= 2,
            "Cmd" | "Meta" | "Command" => bits |= 4,
            "Shift" => bits |= 8,
            other => {
                // Not a modifier; leave it as part of the key name.
                tail = chord;
                let _ = other;
                return (bits, tail);
            }
        }
        tail = &rest[1..];
    }
    (bits, tail)
}

/// SPEC §10 M10 — capture a post-action screenshot, persist the PNG into
/// the session's trace dir, and emit a `Screenshot` trace event whose
/// `png_path` references the on-disk file (relative to the trace dir).
async fn capture_action_screenshot(
    page: &Page,
    sink: &Arc<dyn TraceSink>,
    action: &str,
) -> Result<()> {
    use base64::Engine;
    let png_b64 = page.screenshot("png", None, false, None).await?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(png_b64.as_bytes())
        .context("decoding base64 screenshot")?;
    let png_path = sink.save_screenshot_png(action, &png)?;
    sink.record(TraceEvent::Screenshot {
        ts_ms: sink.now_ms(),
        session_id: page.cdp_session_id().as_str().to_owned(),
        tab_id: page.tab_id().0.clone(),
        after_action: action.to_owned(),
        png_path,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chord_basic() {
        assert_eq!(parse_chord("Cmd+A"), (4, "A"));
        assert_eq!(parse_chord("Shift+Tab"), (8, "Tab"));
        assert_eq!(parse_chord("Cmd+Shift+K"), (12, "K"));
        assert_eq!(parse_chord("Enter"), (0, "Enter"));
    }
}
