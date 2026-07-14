//! Audio device enumeration and volume/mute control.
//!
//! This implementation intentionally uses built-in macOS inventory commands and
//! AppleScript volume controls so the broker can ship without adding new Rust
//! bindings. Capture paths delegate to [`crate::audio_capture`].

use std::process::Command;

use serde_json::Value;

use crate::audio_capture;
use crate::permission;
use crate::types::{AudioDevice, AudioDirection, Capability, SystemError, SystemResult};

fn parse_audio_devices(v: Value) -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    let Some(items) = v
        .get("SPAudioDataType")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("_items"))
        .and_then(Value::as_array)
    else {
        return devices;
    };

    let mut next_id: u32 = 1;
    for item in items {
        let name = item
            .get("_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let manufacturer = item
            .get("coreaudio_device_manufacturer")
            .and_then(Value::as_str)
            .unwrap_or("");
        let uid = if manufacturer.is_empty() {
            name.clone()
        } else {
            format!("{manufacturer}:{name}")
        };
        let sample_rate = item
            .get("coreaudio_device_srate")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        if let Some(channels) = item.get("coreaudio_device_input").and_then(Value::as_u64) {
            devices.push(AudioDevice {
                id: next_id,
                uid: uid.clone(),
                name: name.clone(),
                direction: AudioDirection::Input,
                is_default: item
                    .get("coreaudio_default_audio_input_device")
                    .and_then(Value::as_str)
                    == Some("spaudio_yes"),
                sample_rate,
                channels: channels as u32,
            });
            next_id = next_id.saturating_add(1);
        }
        if let Some(channels) = item.get("coreaudio_device_output").and_then(Value::as_u64) {
            devices.push(AudioDevice {
                id: next_id,
                uid,
                name,
                direction: AudioDirection::Output,
                is_default: item
                    .get("coreaudio_default_audio_output_device")
                    .and_then(Value::as_str)
                    == Some("spaudio_yes"),
                sample_rate,
                channels: channels as u32,
            });
            next_id = next_id.saturating_add(1);
        }
    }
    devices
}

pub fn devices() -> SystemResult<Vec<AudioDevice>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/sbin/system_profiler")
            .args(["SPAudioDataType", "-json"])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let json: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| SystemError::Internal(format!("parse audio inventory: {e}")))?;
        Ok(parse_audio_devices(json))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn inputs() -> SystemResult<Vec<AudioDevice>> {
    Ok(devices()?
        .into_iter()
        .filter(|d| matches!(d.direction, AudioDirection::Input))
        .collect())
}

pub fn outputs() -> SystemResult<Vec<AudioDevice>> {
    Ok(devices()?
        .into_iter()
        .filter(|d| matches!(d.direction, AudioDirection::Output))
        .collect())
}

pub fn select(direction: AudioDirection, uid: &str) -> SystemResult<AudioDevice> {
    let devices = devices()?;
    let selected = devices
        .into_iter()
        .find(|d| d.uid == uid && d.direction == direction)
        .ok_or_else(|| SystemError::NotFound(format!("audio device uid {uid:?}")))?;
    if matches!(selected.direction, AudioDirection::Input) {
        permission::ensure_microphone_granted()?;
    }
    // Selection is inventory-backed only for now; return the chosen device and
    // let the broker surface explicit capability gating. We do not fake a state
    // mutation beyond confirming the device exists.
    Ok(selected)
}

#[cfg(target_os = "macos")]
fn run_osascript(script: &str) -> SystemResult<String> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| SystemError::Io(e.to_string()))?;
    if !output.status.success() {
        return Err(SystemError::Subprocess(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn volume() -> SystemResult<u8> {
    #[cfg(target_os = "macos")]
    {
        let stdout = run_osascript("output volume of (get volume settings)")?;
        let level: u8 = stdout
            .parse()
            .map_err(|e| SystemError::Internal(format!("parse volume: {e}")))?;
        Ok(level)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn set_volume(level: u8) -> SystemResult<u8> {
    #[cfg(target_os = "macos")]
    {
        let clamped = level.min(100);
        let script =
            format!("set volume output volume {clamped}\noutput volume of (get volume settings)");
        let stdout = run_osascript(&script)?;
        let parsed: u8 = stdout
            .lines()
            .last()
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|e| SystemError::Internal(format!("parse output volume: {e}")))?;
        Ok(parsed)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = level;
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn muted() -> SystemResult<bool> {
    #[cfg(target_os = "macos")]
    {
        let stdout = run_osascript("output muted of (get volume settings)")?;
        Ok(stdout == "true")
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}

pub fn set_muted(value: bool) -> SystemResult<bool> {
    #[cfg(target_os = "macos")]
    {
        let script = if value {
            "set volume with output muted\noutput muted of (get volume settings)"
        } else {
            "set volume without output muted\noutput muted of (get volume settings)"
        };
        let stdout = run_osascript(script)?;
        Ok(stdout.lines().last().unwrap_or("false").trim() == "true")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = value;
        Err(SystemError::UnsupportedPlatform)
    }
}

pub async fn capture_to_file(path: &std::path::Path, duration_ms: u64) -> SystemResult<()> {
    permission::ensure_screen_recording_granted()?;
    audio_capture::capture_system_audio(path, duration_ms).await
}

pub fn required_capability_for_direction(direction: AudioDirection) -> Capability {
    match direction {
        AudioDirection::Input => Capability::Microphone,
        AudioDirection::Output => Capability::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_audio_inventory_into_directional_devices() {
        let parsed = parse_audio_devices(json!({
            "SPAudioDataType": [{
                "_items": [{
                    "_name": "Mic",
                    "coreaudio_device_manufacturer": "Apple",
                    "coreaudio_device_input": 1,
                    "coreaudio_default_audio_input_device": "spaudio_yes",
                    "coreaudio_device_srate": 48000
                }, {
                    "_name": "Speaker",
                    "coreaudio_device_manufacturer": "Apple",
                    "coreaudio_device_output": 2,
                    "coreaudio_default_audio_output_device": "spaudio_yes",
                    "coreaudio_device_srate": 44100
                }]
            }]
        }));
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0].direction, AudioDirection::Input));
        assert!(matches!(parsed[1].direction, AudioDirection::Output));
    }
}
