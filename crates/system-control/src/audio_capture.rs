//! Audio and microphone capture helpers.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tokio::time::timeout;

use crate::permission;
use crate::types::{SystemError, SystemResult};

async fn run_ffmpeg(
    args: Vec<String>,
    label: &'static str,
    duration: Duration,
) -> SystemResult<()> {
    let task = tokio::task::spawn_blocking(move || {
        Command::new("/opt/homebrew/bin/ffmpeg").args(args).output()
    });
    let output = timeout(duration + Duration::from_secs(10), task)
        .await
        .map_err(|_| SystemError::Timeout(label.to_string()))?
        .map_err(|e| SystemError::Internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| SystemError::Io(e.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(SystemError::Subprocess(if stderr.is_empty() {
            format!("ffmpeg exited {:?}", output.status.code())
        } else {
            stderr
        }))
    }
}

fn require_ffmpeg() -> SystemResult<()> {
    if std::path::Path::new("/opt/homebrew/bin/ffmpeg").exists() {
        Ok(())
    } else {
        Err(SystemError::NotFound(
            "ffmpeg not found at /opt/homebrew/bin/ffmpeg".to_string(),
        ))
    }
}

pub async fn capture_microphone(path: &Path, duration_ms: u64) -> SystemResult<()> {
    require_ffmpeg()?;
    permission::ensure_microphone_granted()?;
    if duration_ms == 0 {
        return Err(SystemError::InvalidArgument(
            "duration_ms must be > 0".to_string(),
        ));
    }
    let args = vec![
        "-y".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-t".to_string(),
        format!("{:.3}", duration_ms as f64 / 1000.0),
        "-i".to_string(),
        ":0".to_string(),
        path.to_string_lossy().to_string(),
    ];
    run_ffmpeg(args, "mic capture", Duration::from_millis(duration_ms)).await
}

pub async fn capture_system_audio(path: &Path, duration_ms: u64) -> SystemResult<()> {
    require_ffmpeg()?;
    permission::ensure_screen_recording_granted()?;
    if duration_ms == 0 {
        return Err(SystemError::InvalidArgument(
            "duration_ms must be > 0".to_string(),
        ));
    }
    let args = vec![
        "-y".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-t".to_string(),
        format!("{:.3}", duration_ms as f64 / 1000.0),
        "-i".to_string(),
        ":0".to_string(),
        path.to_string_lossy().to_string(),
    ];
    run_ffmpeg(
        args,
        "system audio capture",
        Duration::from_millis(duration_ms),
    )
    .await
}
