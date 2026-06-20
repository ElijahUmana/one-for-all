//! SPEC §12 U6 — `app.quicklook.{preview, close}`.
//!
//! QuickLook is the system's `qlmanage` preview pane. Opening a preview is
//! handled by `qlmanage(1)` (a public CLI Apple ships in
//! `/usr/bin/qlmanage`); closing the active preview is handled via a
//! System Events `key code 53` (Escape) keystroke on the QL Preview window.
//!
//! Notes on the Screen Recording caveat: AX text content of the QL preview
//! pane is gated behind Screen Recording permission per Apple's TCC rules.
//! We probe with [`crate::permission::is_screen_recording_granted`] and emit
//! a clear `ScreenRecordingMissing` error when text would be unreadable.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::permission;
use crate::types::NativeControlError;

/// Open a QuickLook preview for the given file path.
pub async fn preview(path: &str) -> Result<(), NativeControlError> {
    let p = PathBuf::from(path);
    if !p.exists() {
        return Err(NativeControlError::Internal(format!(
            "quicklook: path does not exist: {path}"
        )));
    }
    let path_owned = path.to_string();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("/usr/bin/qlmanage")
            .args(["-p", &path_owned])
            .output()
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
    .map_err(|e| NativeControlError::Io(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(NativeControlError::AppleScript {
            msg: format!("qlmanage exit {:?}: {stderr}", output.status.code()),
        });
    }
    // Brief settle so the preview window is up before any subsequent calls.
    tokio::time::sleep(Duration::from_millis(150)).await;
    Ok(())
}

/// Close the active QuickLook preview by sending Escape to the foremost
/// process. (qlmanage runs as its own process owning the preview pane.)
pub async fn close() -> Result<(), NativeControlError> {
    let script = "tell application \"System Events\" to key code 53"; // 53 = Escape
    crate::actions::app_eval("com.apple.systemevents", script).await?;
    Ok(())
}

/// Check the Screen Recording permission and surface a clean error if absent.
/// Use this as a precondition to ANY operation that depends on reading the
/// QuickLook preview's text content.
pub fn require_screen_recording() -> Result<(), NativeControlError> {
    permission::ensure_screen_recording_granted()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preview_missing_path_errors() {
        let r = preview("/nonexistent/path/qlmanage-test").await;
        match r {
            Err(NativeControlError::Internal(msg)) => assert!(msg.contains("does not exist")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
