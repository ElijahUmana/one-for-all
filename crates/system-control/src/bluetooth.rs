//! Bluetooth scan/connect/disconnect helpers.
//!
//! Implemented via short Swift snippets against CoreBluetooth because the repo
//! does not currently carry Rust Bluetooth bindings and the system contract only
//! needs JSON responses, not long-lived object handles.

use std::process::Command;
use std::time::Duration;

use tokio::time::timeout;

use crate::permission;
use crate::types::{BluetoothDevice, SystemError, SystemResult};

async fn run_swift_json(source: &str, label: &'static str) -> SystemResult<serde_json::Value> {
    let source = source.to_string();
    let task = tokio::task::spawn_blocking(move || {
        Command::new("/usr/bin/swift")
            .arg("-e")
            .arg(source)
            .output()
    });
    let output = timeout(Duration::from_secs(20), task)
        .await
        .map_err(|_| SystemError::Timeout(label.to_string()))?
        .map_err(|e| SystemError::Internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| SystemError::Io(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SystemError::Subprocess(if stderr.is_empty() {
            format!("swift exited {:?}", output.status.code())
        } else {
            stderr
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| SystemError::Internal(format!("parse bluetooth json: {e}")))
}

pub async fn scan(timeout_ms: u64) -> SystemResult<Vec<BluetoothDevice>> {
    permission::ensure_bluetooth_granted()?;
    let timeout_s = (timeout_ms.max(500) as f64) / 1000.0;
    let source = format!(
        r#"
import Foundation
import CoreBluetooth
final class Delegate: NSObject, CBCentralManagerDelegate {{
    var manager: CBCentralManager!
    var results = [String: [String: Any]]()
    let deadline = Date().addingTimeInterval({timeout_s})
    override init() {{
        super.init()
        manager = CBCentralManager(delegate: self, queue: nil)
    }}
    func centralManagerDidUpdateState(_ central: CBCentralManager) {{
        switch central.state {{
        case .poweredOn:
            central.scanForPeripherals(withServices: nil, options: [CBCentralManagerScanOptionAllowDuplicatesKey: false])
        case .unauthorized:
            print("[]")
            fflush(stdout)
            CFRunLoopStop(CFRunLoopGetMain())
        default:
            if Date() >= deadline {{ CFRunLoopStop(CFRunLoopGetMain()) }}
        }}
    }}
    func centralManager(_ central: CBCentralManager, didDiscover peripheral: CBPeripheral, advertisementData: [String : Any], rssi RSSI: NSNumber) {{
        let id = peripheral.identifier.uuidString
        results[id] = [
            "address": id,
            "name": peripheral.name as Any,
            "rssi": RSSI.intValue,
            "paired": false,
            "connected": peripheral.state == .connected
        ]
        if Date() >= deadline {{
            central.stopScan()
            finish()
        }}
    }}
    func finish() {{
        let arr = Array(results.values)
        let data = try! JSONSerialization.data(withJSONObject: arr, options: [])
        FileHandle.standardOutput.write(data)
        CFRunLoopStop(CFRunLoopGetMain())
    }}
}}
let delegate = Delegate()
while Date() < delegate.deadline {{
    RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
}}
delegate.manager.stopScan()
delegate.finish()
"#
    );
    let json = run_swift_json(&source, "bluetooth scan").await?;
    let arr = json.as_array().ok_or_else(|| {
        SystemError::Internal("bluetooth scan output was not an array".to_string())
    })?;
    arr.iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SystemError::Internal(format!("decode bluetooth device: {e}")))
}

pub async fn connect(address: &str) -> SystemResult<bool> {
    permission::ensure_bluetooth_granted()?;
    let source = format!(
        r#"
import Foundation
import CoreBluetooth
let target = UUID(uuidString: "{address}")!
final class Delegate: NSObject, CBCentralManagerDelegate {{
    var manager: CBCentralManager!
    var done = false
    override init() {{
        super.init()
        manager = CBCentralManager(delegate: self, queue: nil)
    }}
    func centralManagerDidUpdateState(_ central: CBCentralManager) {{
        guard central.state == .poweredOn else {{ return }}
        let peripherals = central.retrievePeripherals(withIdentifiers: [target])
        if let p = peripherals.first {{
            central.connect(p, options: nil)
        }} else {{
            print("{{\"connected\":false}}")
            done = true
            CFRunLoopStop(CFRunLoopGetMain())
        }}
    }}
    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {{
        print("{{\"connected\":true}}")
        done = true
        CFRunLoopStop(CFRunLoopGetMain())
    }}
    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {{
        print("{{\"connected\":false}}")
        done = true
        CFRunLoopStop(CFRunLoopGetMain())
    }}
}}
let delegate = Delegate()
let deadline = Date().addingTimeInterval(10)
while !delegate.done && Date() < deadline {{
    RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
}}
if !delegate.done {{{{ print("{{\"connected\":false}}") }}}}
"#
    );
    let json = run_swift_json(&source, "bluetooth connect").await?;
    Ok(json
        .get("connected")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

pub async fn disconnect(address: &str) -> SystemResult<bool> {
    permission::ensure_bluetooth_granted()?;
    let source = format!(
        r#"
import Foundation
import CoreBluetooth
let target = UUID(uuidString: "{address}")!
final class Delegate: NSObject, CBCentralManagerDelegate {{
    var manager: CBCentralManager!
    var done = false
    override init() {{
        super.init()
        manager = CBCentralManager(delegate: self, queue: nil)
    }}
    func centralManagerDidUpdateState(_ central: CBCentralManager) {{
        guard central.state == .poweredOn else {{ return }}
        let peripherals = central.retrievePeripherals(withIdentifiers: [target])
        if let p = peripherals.first {{
            central.cancelPeripheralConnection(p)
            print("{{\"disconnected\":true}}")
        }} else {{
            print("{{\"disconnected\":false}}")
        }}
        done = true
        CFRunLoopStop(CFRunLoopGetMain())
    }}
}}
let delegate = Delegate()
let deadline = Date().addingTimeInterval(5)
while !delegate.done && Date() < deadline {{
    RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.05))
}}
if !delegate.done {{{{ print("{{\"disconnected\":false}}") }}}}
"#
    );
    let json = run_swift_json(&source, "bluetooth disconnect").await?;
    Ok(json
        .get("disconnected")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}
