//! Camera snapshot capture.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tokio::time::timeout;

use crate::permission;
use crate::types::{SystemError, SystemResult};

fn require_imagesnap() -> SystemResult<()> {
    if std::path::Path::new("/opt/homebrew/bin/imagesnap").exists() {
        Ok(())
    } else {
        Err(SystemError::NotFound(
            "imagesnap not found at /opt/homebrew/bin/imagesnap".to_string(),
        ))
    }
}

pub async fn snapshot(path: &Path, device_id: Option<&str>) -> SystemResult<()> {
    require_imagesnap()?;
    permission::ensure_camera_granted()?;
    let path = path.to_path_buf();
    let device = device_id.map(str::to_string);
    let task = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("/opt/homebrew/bin/imagesnap");
        cmd.arg("-q");
        if let Some(device) = device.as_deref() {
            cmd.arg("-d").arg(device);
        }
        cmd.arg(path);
        cmd.output()
    });
    let output = timeout(Duration::from_secs(15), task)
        .await
        .map_err(|_| SystemError::Timeout("camera snapshot".to_string()))?
        .map_err(|e| SystemError::Internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| SystemError::Io(e.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(SystemError::Subprocess(if stderr.is_empty() {
            format!("imagesnap exited {:?}", output.status.code())
        } else {
            stderr
        }))
    }
}
