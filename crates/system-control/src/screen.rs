//! Screen capture and display inventory.

use std::path::Path;
use std::process::Command;

use base64::Engine as _;
use core_graphics::display::CGDisplay;
use core_graphics::geometry::CGRect;

use crate::permission;
use crate::types::{Display, SystemError, SystemResult};

pub fn list_displays() -> SystemResult<Vec<Display>> {
    #[cfg(target_os = "macos")]
    {
        let active = CGDisplay::active_displays().map_err(|e| SystemError::Os {
            domain: "CoreGraphics",
            code: i64::from(e),
        })?;
        let main = CGDisplay::main();
        let mut displays = Vec::with_capacity(active.len());
        for id in active {
            let display_ref = CGDisplay::new(id);
            let bounds = display_ref.bounds();
            let width_px = display_ref.pixels_wide();
            let _height_px = display_ref.pixels_high();
            let logical_width = bounds.size.width.max(1.0);
            let scale = width_px as f64 / logical_width;
            displays.push(Display {
                id,
                width: bounds.size.width.max(0.0).round() as u32,
                height: bounds.size.height.max(0.0).round() as u32,
                origin_x: bounds.origin.x.round() as i32,
                origin_y: bounds.origin.y.round() as i32,
                is_main: id == main.id,
                scale,
            });
        }
        Ok(displays)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn validate_region(x: i32, y: i32, width: u32, height: u32) -> SystemResult<()> {
    if width == 0 || height == 0 {
        return Err(SystemError::InvalidArgument(
            "capture region width/height must be > 0".to_string(),
        ));
    }
    let _ = (x, y);
    Ok(())
}

pub fn capture_region(
    path: &Path,
    display_id: Option<u32>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> SystemResult<String> {
    permission::ensure_screen_recording_granted()?;
    validate_region(x, y, width, height)?;
    #[cfg(target_os = "macos")]
    {
        let displays = list_displays()?;
        if let Some(id) = display_id {
            if !displays.iter().any(|d| d.id == id) {
                return Err(SystemError::NotFound(format!("display_id {id}")));
            }
        }
        let display = if let Some(id) = display_id {
            displays.into_iter().find(|d| d.id == id)
        } else {
            displays.into_iter().find(|d| d.is_main)
        }
        .ok_or_else(|| SystemError::NotFound("no matching display".to_string()))?;

        let rel_x = x - display.origin_x;
        let rel_y = y - display.origin_y;
        if rel_x >= display.width as i32 || rel_y >= display.height as i32 {
            return Err(SystemError::InvalidArgument(
                "capture region starts outside display bounds".to_string(),
            ));
        }
        let clamped_width = width.min(display.width.saturating_sub(rel_x.max(0) as u32));
        let clamped_height = height.min(display.height.saturating_sub(rel_y.max(0) as u32));
        if clamped_width == 0 || clamped_height == 0 {
            return Err(SystemError::InvalidArgument(
                "capture region lies outside display bounds".to_string(),
            ));
        }

        let output = Command::new("/usr/sbin/screencapture")
            .args([
                "-x",
                "-R",
                &format!("{},{},{},{}", x, y, clamped_width, clamped_height),
                path.to_string_lossy().as_ref(),
            ])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let bytes = std::fs::read(path).map_err(|e| SystemError::Io(e.to_string()))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, display_id, x, y, width, height);
        Err(SystemError::UnsupportedPlatform)
    }
}

#[allow(dead_code)]
fn _bounds_for_display(display_id: u32) -> CGRect {
    CGDisplay::new(display_id).bounds()
}
