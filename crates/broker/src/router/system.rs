//! SPEC §12 U8 — `system.*` broker handlers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};
use system_control::{Capability, SystemError};

use crate::protocol::ErrorCode;
use crate::registry::SessionEntry;

use super::{required_str, RouterError, ToolResult};

static NEXT_WATCH_ID: AtomicU64 = AtomicU64::new(1);

fn session_root(entry: &SessionEntry) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".one-for-all")
        .join("sessions")
        .join(&entry.session_id)
}

fn make_watch_id() -> String {
    let id = NEXT_WATCH_ID.fetch_add(1, Ordering::Relaxed);
    format!("fsw-{id:x}")
}

fn require_system(session: &SessionEntry, capability: Capability) -> Result<(), RouterError> {
    if !session.has_capability(Capability::None.as_str()) {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: "session lacks 'system' capability — pass capabilities: [\"system\"] to session.register".to_string(),
            data: Some(json!({"capability": Capability::None.as_str()})),
        });
    }
    if !matches!(capability, Capability::None) && !session.has_capability(capability.as_str()) {
        return Err(RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!(
                "operation requires capabilities:[\"{}\"] in session.register",
                capability.as_str()
            ),
            data: Some(json!({"capability": capability.as_str()})),
        });
    }
    Ok(())
}

fn map_system_err(e: SystemError) -> RouterError {
    match e {
        SystemError::PermissionMissing {
            capability,
            settings_url,
        } => RouterError {
            code: ErrorCode::PermissionDenied,
            message: format!("{} permission missing", capability.as_str()),
            data: Some(json!({
                "capability": capability.as_str(),
                "settings_url": settings_url,
            })),
        },
        SystemError::NotFound(s) => RouterError {
            code: ErrorCode::TabNotFound,
            message: s,
            data: None,
        },
        SystemError::InvalidArgument(s) => RouterError::invalid_params(s),
        SystemError::Os { domain, code } => RouterError {
            code: ErrorCode::InternalError,
            message: format!("{domain} error: {code}"),
            data: Some(json!({"domain": domain, "code": code})),
        },
        SystemError::Io(s) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("io: {s}"),
            data: None,
        },
        SystemError::Subprocess(s) => RouterError {
            code: ErrorCode::InternalError,
            message: format!("subprocess: {s}"),
            data: None,
        },
        SystemError::Timeout(s) => RouterError {
            code: ErrorCode::Timeout,
            message: s,
            data: None,
        },
        SystemError::Internal(s) => RouterError {
            code: ErrorCode::InternalError,
            message: s,
            data: None,
        },
        SystemError::UnsupportedPlatform => RouterError {
            code: ErrorCode::InternalError,
            message: "system-control unsupported on this platform".to_string(),
            data: None,
        },
    }
}

struct BrokerFsSink {
    entry: Arc<SessionEntry>,
}

impl system_control::NotificationSink for BrokerFsSink {
    fn notify(&self, payload: serde_json::Value) {
        use crate::ServerEvent;
        let ev = ServerEvent {
            jsonrpc: "2.0".into(),
            method: "event/notify".into(),
            params: serde_json::json!({
                "topic": "system.fsevents",
                "session_id": self.entry.session_id,
                "payload": payload,
            }),
        };
        let _ = self.entry.try_push(ev);
    }
}

pub(super) async fn system_dispatch(
    session: &Arc<SessionEntry>,
    method: &str,
    params: Value,
) -> ToolResult {
    match method {
        "system.audio.output" => {
            require_system(session, Capability::None)?;
            let devices = system_control::audio::outputs().map_err(map_system_err)?;
            Ok(json!({"devices": devices}))
        }
        "system.audio.input" => {
            require_system(session, Capability::Microphone)?;
            let devices = system_control::audio::inputs().map_err(map_system_err)?;
            Ok(json!({"devices": devices}))
        }
        "system.audio.select" => {
            let direction = match required_str(&params, "direction")? {
                "input" => system_control::types::AudioDirection::Input,
                "output" => system_control::types::AudioDirection::Output,
                other => {
                    return Err(RouterError::invalid_params(format!(
                        "invalid direction {other:?}; expected input|output"
                    )))
                }
            };
            require_system(
                session,
                system_control::audio::required_capability_for_direction(direction),
            )?;
            let uid = required_str(&params, "uid")?;
            let device = system_control::audio::select(direction, uid).map_err(map_system_err)?;
            Ok(json!({"device": device}))
        }
        "system.audio.volume" => {
            require_system(session, Capability::None)?;
            let level = params.get("level").and_then(Value::as_u64).map(|v| v as u8);
            let volume = if let Some(level) = level {
                system_control::audio::set_volume(level).map_err(map_system_err)?
            } else {
                system_control::audio::volume().map_err(map_system_err)?
            };
            Ok(json!({"level": volume}))
        }
        "system.audio.mute" => {
            require_system(session, Capability::None)?;
            let value = params.get("value").and_then(Value::as_bool);
            let muted = if let Some(value) = value {
                system_control::audio::set_muted(value).map_err(map_system_err)?
            } else {
                system_control::audio::muted().map_err(map_system_err)?
            };
            Ok(json!({"muted": muted}))
        }
        "system.audio.capture_to_file" => {
            require_system(session, Capability::Screen)?;
            let rel = required_str(&params, "path")?;
            let duration_ms = params
                .get("duration_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| RouterError::invalid_params("missing duration_ms"))?;
            let path = session_root(session).join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RouterError::internal(format!("create capture dir: {e}")))?;
            }
            system_control::audio::capture_to_file(&path, duration_ms)
                .await
                .map_err(map_system_err)?;
            Ok(json!({"path": path.to_string_lossy(), "duration_ms": duration_ms}))
        }
        "system.mic.capture" => {
            require_system(session, Capability::Microphone)?;
            let rel = required_str(&params, "path")?;
            let duration_ms = params
                .get("duration_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| RouterError::invalid_params("missing duration_ms"))?;
            let path = session_root(session).join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RouterError::internal(format!("create capture dir: {e}")))?;
            }
            system_control::audio_capture::capture_microphone(&path, duration_ms)
                .await
                .map_err(map_system_err)?;
            Ok(json!({"path": path.to_string_lossy(), "duration_ms": duration_ms}))
        }
        "system.camera.snapshot" => {
            require_system(session, Capability::Camera)?;
            let rel = required_str(&params, "path")?;
            let device_id = params.get("device_id").and_then(Value::as_str);
            let path = session_root(session).join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RouterError::internal(format!("create snapshot dir: {e}")))?;
            }
            system_control::camera::snapshot(&path, device_id)
                .await
                .map_err(map_system_err)?;
            let bytes = std::fs::read(&path)
                .map_err(|e| RouterError::internal(format!("read snapshot: {e}")))?;
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(json!({
                "path": path.to_string_lossy(),
                "format": "jpeg",
                "data_base64": data_base64,
            }))
        }
        "system.screen.list_displays" => {
            require_system(session, Capability::None)?;
            let displays = system_control::screen::list_displays().map_err(map_system_err)?;
            Ok(json!({"displays": displays}))
        }
        "system.screen.capture_region" => {
            require_system(session, Capability::Screen)?;
            let rel = required_str(&params, "path")?;
            let x = params
                .get("x")
                .and_then(Value::as_i64)
                .ok_or_else(|| RouterError::invalid_params("missing x"))?
                as i32;
            let y = params
                .get("y")
                .and_then(Value::as_i64)
                .ok_or_else(|| RouterError::invalid_params("missing y"))?
                as i32;
            let width = params
                .get("width")
                .and_then(Value::as_u64)
                .ok_or_else(|| RouterError::invalid_params("missing width"))?
                as u32;
            let height = params
                .get("height")
                .and_then(Value::as_u64)
                .ok_or_else(|| RouterError::invalid_params("missing height"))?
                as u32;
            let display_id = params
                .get("display_id")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let path = session_root(session).join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| RouterError::internal(format!("create screenshot dir: {e}")))?;
            }
            let data_base64 =
                system_control::screen::capture_region(&path, display_id, x, y, width, height)
                    .map_err(map_system_err)?;
            Ok(json!({
                "path": path.to_string_lossy(),
                "format": "png",
                "data_base64": data_base64,
                "width": width,
                "height": height,
            }))
        }
        "system.bluetooth.scan" => {
            require_system(session, Capability::Bluetooth)?;
            let timeout_ms = params
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5_000);
            let devices = system_control::bluetooth::scan(timeout_ms)
                .await
                .map_err(map_system_err)?;
            Ok(json!({"devices": devices}))
        }
        "system.bluetooth.connect" => {
            require_system(session, Capability::Bluetooth)?;
            let address = required_str(&params, "address")?;
            let connected = system_control::bluetooth::connect(address)
                .await
                .map_err(map_system_err)?;
            Ok(json!({"connected": connected}))
        }
        "system.bluetooth.disconnect" => {
            require_system(session, Capability::Bluetooth)?;
            let address = required_str(&params, "address")?;
            let disconnected = system_control::bluetooth::disconnect(address)
                .await
                .map_err(map_system_err)?;
            Ok(json!({"disconnected": disconnected}))
        }
        "system.usb.devices" => {
            require_system(session, Capability::RawUsb)?;
            let devices = system_control::usb::devices().map_err(map_system_err)?;
            Ok(json!({"devices": devices}))
        }
        "system.battery" => {
            require_system(session, Capability::None)?;
            let state = system_control::battery::state().map_err(map_system_err)?;
            Ok(json!({"battery": state}))
        }
        "system.network.interfaces" => {
            require_system(session, Capability::None)?;
            let interfaces = system_control::network::interfaces().map_err(map_system_err)?;
            Ok(json!({"interfaces": interfaces}))
        }
        "system.network.routes" => {
            require_system(session, Capability::None)?;
            let routes = system_control::network::routes().map_err(map_system_err)?;
            Ok(json!({"routes": routes}))
        }
        "system.network.connections" => {
            require_system(session, Capability::None)?;
            let connections = system_control::network::connections().map_err(map_system_err)?;
            Ok(json!({"connections": connections}))
        }
        "system.process.list" => {
            require_system(session, Capability::None)?;
            let processes = system_control::process::list().map_err(map_system_err)?;
            Ok(json!({"processes": processes}))
        }
        "system.process.info" => {
            require_system(session, Capability::None)?;
            let pid = params
                .get("pid")
                .and_then(Value::as_i64)
                .ok_or_else(|| RouterError::invalid_params("missing pid"))?
                as i32;
            let info = system_control::process::info(pid).map_err(map_system_err)?;
            Ok(json!({"process": info}))
        }
        "system.process.signal" => {
            require_system(session, Capability::None)?;
            let pid = params
                .get("pid")
                .and_then(Value::as_i64)
                .ok_or_else(|| RouterError::invalid_params("missing pid"))?
                as i32;
            let signal = params
                .get("signal")
                .and_then(Value::as_i64)
                .ok_or_else(|| RouterError::invalid_params("missing signal"))?
                as i32;
            system_control::process::signal(pid, signal).map_err(map_system_err)?;
            Ok(json!({"ok": true}))
        }
        "system.fsevents.watch" => {
            require_system(session, Capability::None)?;
            let paths: Vec<PathBuf> = params
                .get("paths")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect::<Vec<_>>()
                })
                .ok_or_else(|| RouterError::invalid_params("missing paths: array"))?;
            let watch_id = make_watch_id();
            let sink: Arc<dyn system_control::NotificationSink> = Arc::new(BrokerFsSink {
                entry: Arc::clone(session),
            });
            let handle = system_control::fsevents::watch(&paths, watch_id.clone(), sink)
                .map_err(map_system_err)?;
            session.register_system_watch(handle);
            Ok(json!({"watch_id": watch_id}))
        }
        "system.spotlight.query" => {
            require_system(session, Capability::None)?;
            let q = required_str(&params, "q")?;
            let results = system_control::spotlight::query(q).map_err(map_system_err)?;
            Ok(json!({"results": results}))
        }
        "system.metadata" => {
            require_system(session, Capability::None)?;
            let path = PathBuf::from(required_str(&params, "path")?);
            let metadata = system_control::metadata::metadata(&path).map_err(map_system_err)?;
            Ok(json!({"metadata": metadata}))
        }
        _ => Err(RouterError::method_not_found(method.to_string())),
    }
}
