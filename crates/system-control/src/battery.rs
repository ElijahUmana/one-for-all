//! Battery inventory.

use std::process::Command;

use crate::types::{BatteryState, SystemError, SystemResult};

pub fn state() -> SystemResult<BatteryState> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/pmset")
            .args(["-g", "batt"])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut state = BatteryState {
            has_battery: stdout.contains("InternalBattery"),
            level: None,
            is_charging: None,
            is_charged: None,
            time_to_empty_min: None,
            time_to_full_min: None,
            source: None,
        };
        for line in stdout.lines() {
            if let Some(rest) = line.split('\t').nth(1) {
                if let Some(percent) = rest.split(';').next() {
                    state.level = percent
                        .trim()
                        .strip_suffix('%')
                        .and_then(|s| s.parse::<f32>().ok())
                        .map(|v| v / 100.0);
                }
                let lower = rest.to_lowercase();
                state.is_charging =
                    Some(lower.contains("charging") || lower.contains("finishing charge"));
                state.is_charged = Some(lower.contains("charged"));
                if let Some(last) = rest.split(';').next_back() {
                    let trimmed = last.trim();
                    if trimmed != "no estimate" && trimmed != "(no estimate)" {
                        let parts: Vec<_> = trimmed.split(':').collect();
                        if parts.len() == 2 {
                            if let (Ok(h), Ok(m)) =
                                (parts[0].parse::<i32>(), parts[1].parse::<i32>())
                            {
                                let total = h * 60 + m;
                                if state.is_charging == Some(true) {
                                    state.time_to_full_min = Some(total);
                                } else {
                                    state.time_to_empty_min = Some(total);
                                }
                            }
                        }
                    }
                }
            }
            if line.contains("Now drawing from") {
                state.source = line.split('"').nth(1).map(str::to_string);
            }
        }
        Ok(state)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}
