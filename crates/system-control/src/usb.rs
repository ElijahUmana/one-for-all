//! USB device inventory.

use std::process::Command;

use serde_json::Value;

use crate::types::{SystemError, SystemResult, UsbDevice};

fn flatten(items: &[Value], out: &mut Vec<UsbDevice>) {
    for item in items {
        let vendor_id = item
            .get("vendor_id")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let product_id = item
            .get("product_id")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u16::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let speed = item
            .get("device_speed")
            .and_then(Value::as_str)
            .map(str::to_string);
        let location_id = item
            .get("location_id")
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u32::from_str_radix(s, 16).ok());
        if vendor_id != 0 || product_id != 0 {
            out.push(UsbDevice {
                vendor_id,
                product_id,
                vendor_name: item
                    .get("manufacturer")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                product_name: item
                    .get("_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                serial: item
                    .get("serial_num")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                speed,
                location_id,
            });
        }
        if let Some(children) = item.get("_items").and_then(Value::as_array) {
            flatten(children, out);
        }
    }
}

pub fn devices() -> SystemResult<Vec<UsbDevice>> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/sbin/system_profiler")
            .args(["SPUSBDataType", "-json"])
            .output()
            .map_err(|e| SystemError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(SystemError::Subprocess(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let json: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| SystemError::Internal(format!("parse usb inventory: {e}")))?;
        let mut out = Vec::new();
        if let Some(items) = json.get("SPUSBDataType").and_then(Value::as_array) {
            flatten(items, &mut out);
        }
        Ok(out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(SystemError::UnsupportedPlatform)
    }
}
