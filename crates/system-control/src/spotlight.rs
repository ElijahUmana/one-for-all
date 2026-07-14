//! Spotlight search helpers.

use std::process::Command;

use crate::types::{SystemError, SystemResult};

pub fn query(q: &str) -> SystemResult<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/mdfind")
            .arg(q)
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = q;
        Err(SystemError::UnsupportedPlatform)
    }
}
