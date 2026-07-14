//! Permission helpers for SPEC §12 U8.
//!
//! Mirrors the native-control permission model: capability denial is handled
//! by the broker, while OS/TCC denial is surfaced here as a typed
//! [`SystemError::PermissionMissing`] with a stable deeplink.

use crate::types::{Capability, SystemError, SystemResult};

#[cfg(target_os = "macos")]
fn run_swift_probe(source: &str) -> SystemResult<bool> {
    use std::process::Command;

    let output = Command::new("/usr/bin/swift")
        .arg("-e")
        .arg(source)
        .output()
        .map_err(|e| SystemError::Io(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SystemError::Subprocess(if stderr.is_empty() {
            format!("swift exited {:?}", output.status.code())
        } else {
            stderr
        }));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "granted")
}

#[cfg(target_os = "macos")]
pub fn ensure_camera_granted() -> SystemResult<()> {
    let granted = run_swift_probe(
        r#"
import AVFoundation
let status = AVCaptureDevice.authorizationStatus(for: .video)
switch status {
case .authorized:
    print("granted")
default:
    print("denied")
}
"#,
    )?;
    if granted {
        Ok(())
    } else {
        Err(SystemError::PermissionMissing {
            capability: Capability::Camera,
            settings_url: Capability::Camera.settings_deeplink(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_camera_granted() -> SystemResult<()> {
    Err(SystemError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
pub fn ensure_microphone_granted() -> SystemResult<()> {
    let granted = run_swift_probe(
        r#"
import AVFoundation
let status = AVCaptureDevice.authorizationStatus(for: .audio)
switch status {
case .authorized:
    print("granted")
default:
    print("denied")
}
"#,
    )?;
    if granted {
        Ok(())
    } else {
        Err(SystemError::PermissionMissing {
            capability: Capability::Microphone,
            settings_url: Capability::Microphone.settings_deeplink(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_microphone_granted() -> SystemResult<()> {
    Err(SystemError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
pub fn ensure_screen_recording_granted() -> SystemResult<()> {
    use libc::{c_void, dlsym, RTLD_DEFAULT};
    use std::ffi::CString;
    use std::sync::OnceLock;

    type Probe = unsafe extern "C" fn() -> bool;
    static CACHED: OnceLock<Option<Probe>> = OnceLock::new();

    let f = CACHED.get_or_init(|| {
        let name = match CString::new("CGPreflightScreenCaptureAccess") {
            Ok(c) => c,
            Err(_) => return None,
        };
        let sym = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
        if sym.is_null() {
            None
        } else {
            let probe: Probe = unsafe { std::mem::transmute::<*mut c_void, Probe>(sym) };
            Some(probe)
        }
    });

    let granted = match f {
        Some(probe) => unsafe { probe() },
        None => false,
    };
    if granted {
        Ok(())
    } else {
        Err(SystemError::PermissionMissing {
            capability: Capability::Screen,
            settings_url: Capability::Screen.settings_deeplink(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_screen_recording_granted() -> SystemResult<()> {
    Err(SystemError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
pub fn ensure_bluetooth_granted() -> SystemResult<()> {
    let granted = run_swift_probe(
        r#"
import CoreBluetooth
final class Probe: NSObject, CBCentralManagerDelegate {
    var state: CBManagerState?
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        state = central.state
        if central.state == .unauthorized || central.state == .poweredOn || central.state == .poweredOff || central.state == .unsupported || central.state == .resetting || central.state == .unknown {
            CFRunLoopStop(CFRunLoopGetMain())
        }
    }
}
let delegate = Probe()
let manager = CBCentralManager(delegate: delegate, queue: nil)
let deadline = Date().addingTimeInterval(3.0)
while delegate.state == nil && Date() < deadline {
    RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
}
switch delegate.state ?? .unknown {
case .unauthorized:
    print("denied")
case .poweredOn, .poweredOff:
    print("granted")
default:
    print("denied")
}
_ = manager
"#,
    )?;
    if granted {
        Ok(())
    } else {
        Err(SystemError::PermissionMissing {
            capability: Capability::Bluetooth,
            settings_url: Capability::Bluetooth.settings_deeplink(),
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_bluetooth_granted() -> SystemResult<()> {
    Err(SystemError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeplinks_match_capabilities() {
        assert!(Capability::Camera
            .settings_deeplink()
            .contains("Privacy_Camera"));
        assert!(Capability::Microphone
            .settings_deeplink()
            .contains("Privacy_Microphone"));
        assert!(Capability::Screen
            .settings_deeplink()
            .contains("Privacy_ScreenCapture"));
        assert!(Capability::Bluetooth
            .settings_deeplink()
            .contains("Privacy_Bluetooth"));
    }
}
