//! HiDPI / multi-monitor coordinate-space helpers (SPEC §11 V4 — coordinate
//! discipline). Internal pipeline math runs in *device* pixels (the units
//! produced by `Page.startScreencast`); every public bbox returned through
//! a `vision.*` tool MUST be expressed in *CSS* pixels so agents can
//! roundtrip with `page.click {x, y}` and friends without scale-factor
//! drift.
//!
//! `display_id` plumbing rides alongside every public bbox so multi-monitor
//! captures (one ring per display) stay distinguishable.
//!
//! All conversions are total — they clamp to the legal pixel grid rather
//! than panicking on degenerate inputs (`page_scale_factor <= 0`,
//! overflow). This honours the crate's `clippy::unwrap_used` deny.

use serde::{Deserialize, Serialize};

use crate::diff::Bbox;

/// Default display id used by the in-process broker when only one monitor
/// is being captured per session.
pub const DEFAULT_DISPLAY_ID: u32 = 0;

/// CSS-pixel rectangle returned by every public `vision.*` tool. Carries
/// `display_id` so multi-monitor consumers can tell two `(0,0)` origins
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CssBbox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    #[serde(default)]
    pub display_id: u32,
}

impl CssBbox {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self {
            x,
            y,
            w,
            h,
            display_id: DEFAULT_DISPLAY_ID,
        }
    }
    pub fn with_display(mut self, display_id: u32) -> Self {
        self.display_id = display_id;
        self
    }
    pub fn to_device(self, scale: f64) -> Bbox {
        let scale = sanitize_scale(scale);
        Bbox {
            x: scale_round(self.x as f64 * scale),
            y: scale_round(self.y as f64 * scale),
            w: scale_round(self.w as f64 * scale),
            h: scale_round(self.h as f64 * scale),
        }
    }
}

/// Clamp `page_scale_factor` to a strictly-positive, finite range. CDP
/// emits 1.0 by default and 2.0/3.0 on Retina; anything else is treated
/// as 1.0 to avoid divide-by-zero / NaN propagation downstream.
pub fn sanitize_scale(scale: f64) -> f64 {
    if !scale.is_finite() || scale <= 0.0 {
        1.0
    } else {
        scale.clamp(0.25, 8.0)
    }
}

fn scale_round(v: f64) -> u32 {
    if !v.is_finite() || v < 0.0 {
        0
    } else if v > u32::MAX as f64 {
        u32::MAX
    } else {
        v.round() as u32
    }
}

/// Convert a device-px [`Bbox`] to a CSS-px [`CssBbox`] using the page's
/// `page_scale_factor`. `display_id` defaults to `DEFAULT_DISPLAY_ID` —
/// callers that own a multi-monitor surface stamp it after.
pub fn to_css_bbox(bbox: Bbox, page_scale_factor: f64) -> CssBbox {
    let scale = sanitize_scale(page_scale_factor);
    CssBbox {
        x: scale_round(bbox.x as f64 / scale),
        y: scale_round(bbox.y as f64 / scale),
        w: scale_round(bbox.w as f64 / scale),
        h: scale_round(bbox.h as f64 / scale),
        display_id: DEFAULT_DISPLAY_ID,
    }
}

/// Convert a CSS-px [`CssBbox`] back to device px so internal callers can
/// hit pixel buffers / OCR caches that index by device px.
pub fn to_device_bbox(bbox: CssBbox, page_scale_factor: f64) -> Bbox {
    bbox.to_device(page_scale_factor)
}

/// Clamp a device-px [`Bbox`] to the image's pixel rect. Returns `None`
/// if the intersection is empty (out-of-bounds region).
pub fn clamp_to_image(bbox: Bbox, w: u32, h: u32) -> Option<Bbox> {
    let x0 = bbox.x.min(w);
    let y0 = bbox.y.min(h);
    let x1 = bbox.x.saturating_add(bbox.w).min(w);
    let y1 = bbox.y.saturating_add(bbox.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Bbox {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_px_round_trips_under_hidpi() {
        for scale in [1.0_f64, 2.0, 3.0] {
            let device = Bbox {
                x: (10.0 * scale) as u32,
                y: (20.0 * scale) as u32,
                w: (100.0 * scale) as u32,
                h: (40.0 * scale) as u32,
            };
            let css = to_css_bbox(device, scale);
            assert_eq!(css.x, 10);
            assert_eq!(css.y, 20);
            assert_eq!(css.w, 100);
            assert_eq!(css.h, 40);
            let back = to_device_bbox(css, scale);
            assert_eq!(back, device);
        }
    }

    #[test]
    fn sanitize_scale_handles_garbage_inputs() {
        assert_eq!(sanitize_scale(0.0), 1.0);
        assert_eq!(sanitize_scale(-2.0), 1.0);
        assert_eq!(sanitize_scale(f64::NAN), 1.0);
        assert_eq!(sanitize_scale(f64::INFINITY), 1.0);
        assert_eq!(sanitize_scale(2.0), 2.0);
        assert_eq!(sanitize_scale(100.0), 8.0);
    }

    #[test]
    fn clamp_to_image_handles_out_of_bounds() {
        let img = (200, 100);
        assert!(clamp_to_image(
            Bbox {
                x: 300,
                y: 0,
                w: 10,
                h: 10
            },
            img.0,
            img.1
        )
        .is_none());
        let c = clamp_to_image(
            Bbox {
                x: 190,
                y: 0,
                w: 50,
                h: 10,
            },
            img.0,
            img.1,
        )
        .expect("clamp");
        assert_eq!(c.w, 10);
        assert_eq!(c.h, 10);
    }

    #[test]
    fn display_id_default_and_override() {
        let mut b = CssBbox::new(0, 0, 10, 10);
        assert_eq!(b.display_id, DEFAULT_DISPLAY_ID);
        b = b.with_display(2);
        assert_eq!(b.display_id, 2);
    }
}
