//! Non-macOS stubs for system-control.

use std::path::Path;

use serde_json::Value;

use crate::types::{
    AudioDevice, AudioDirection, BatteryState, BluetoothDevice, Display, NetworkConnection,
    NetworkInterface, NetworkRoute, ProcessInfo, ProcessSummary, SystemError, SystemResult,
    UsbDevice,
};

pub fn unsupported<T>() -> SystemResult<T> {
    Err(SystemError::UnsupportedPlatform)
}

pub fn audio_devices() -> SystemResult<Vec<AudioDevice>> {
    unsupported()
}
pub fn audio_inputs() -> SystemResult<Vec<AudioDevice>> {
    unsupported()
}
pub fn audio_outputs() -> SystemResult<Vec<AudioDevice>> {
    unsupported()
}
pub fn audio_select(_direction: AudioDirection, _uid: &str) -> SystemResult<AudioDevice> {
    unsupported()
}
pub fn audio_volume() -> SystemResult<u8> {
    unsupported()
}
pub fn audio_set_volume(_level: u8) -> SystemResult<u8> {
    unsupported()
}
pub fn audio_muted() -> SystemResult<bool> {
    unsupported()
}
pub fn audio_set_muted(_value: bool) -> SystemResult<bool> {
    unsupported()
}
pub async fn audio_capture_to_file(_path: &Path, _duration_ms: u64) -> SystemResult<()> {
    unsupported()
}
pub async fn mic_capture(_path: &Path, _duration_ms: u64) -> SystemResult<()> {
    unsupported()
}
pub async fn camera_snapshot(_path: &Path, _device_id: Option<&str>) -> SystemResult<()> {
    unsupported()
}
pub fn screen_list_displays() -> SystemResult<Vec<Display>> {
    unsupported()
}
pub fn screen_capture_region(
    _path: &Path,
    _display_id: Option<u32>,
    _x: i32,
    _y: i32,
    _width: u32,
    _height: u32,
) -> SystemResult<String> {
    unsupported()
}
pub async fn bluetooth_scan(_timeout_ms: u64) -> SystemResult<Vec<BluetoothDevice>> {
    unsupported()
}
pub async fn bluetooth_connect(_address: &str) -> SystemResult<bool> {
    unsupported()
}
pub async fn bluetooth_disconnect(_address: &str) -> SystemResult<bool> {
    unsupported()
}
pub fn usb_devices() -> SystemResult<Vec<UsbDevice>> {
    unsupported()
}
pub fn battery_state() -> SystemResult<BatteryState> {
    unsupported()
}
pub fn network_interfaces() -> SystemResult<Vec<NetworkInterface>> {
    unsupported()
}
pub fn network_routes() -> SystemResult<Vec<NetworkRoute>> {
    unsupported()
}
pub fn network_connections() -> SystemResult<Vec<NetworkConnection>> {
    unsupported()
}
pub fn process_list() -> SystemResult<Vec<ProcessSummary>> {
    unsupported()
}
pub fn process_info(_pid: i32) -> SystemResult<ProcessInfo> {
    unsupported()
}
pub fn process_signal(_pid: i32, _signal: i32) -> SystemResult<()> {
    unsupported()
}
pub fn spotlight_query(_q: &str) -> SystemResult<Vec<String>> {
    unsupported()
}
pub fn metadata_for(_path: &Path) -> SystemResult<Value> {
    unsupported()
}
