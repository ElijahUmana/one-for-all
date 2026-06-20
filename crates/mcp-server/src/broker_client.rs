//! Connection to the broker (SPEC §2). The broker listens on a Unix domain
//! stream socket at `~/.one-for-all/broker.sock`, framed line-delimited JSON-RPC
//! 2.0, 16 MB cap per line.
//!
//! Per SPEC §D7, the broker is an opportunistic singleton elected by
//! `flock(~/.one-for-all/broker.lock)`. Tools call broker methods
//! **directly** (e.g. `tab.open`) — there is no `tool.call` wrapper.
//!
//! On startup this client:
//!   1. Connects (or kickstarts the launchd job and retries with backoff).
//!   2. Calls `session.register` and stores the broker-assigned `session_id`.
//!   3. Spawns a reader actor that demuxes responses → oneshot replies and
//!      `event/notify` notifications → broadcast channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedWriteHalf, UnixStream};
use tokio::sync::{broadcast, oneshot};
use tokio::time::{timeout, Instant};
use tracing::{debug, error, info, instrument, warn};

use crate::error::{jsonrpc_code, BridgeError};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_BACKOFF_MS: &[u64] = &[50, 100, 200, 400, 800, 1600];
const KICKSTART_LABEL: &str = "io.github.elijahumana.one-for-all";
// SPEC §10 / N2 — centralized in `observability::caps::NETWORK_OBSERVE_CAP`
// since this broadcast carries `net.observe` notifications. Kept as a local
// alias so the channel allocation reads naturally.
const NOTIFICATION_CHANNEL_CAP: usize = observability::caps::NETWORK_OBSERVE_CAP;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_REGISTER_CAPABILITIES: &[&str] =
    &["tools", "events", "binary-topics", "storage_state"];

/// Resolve the broker socket path: `$ONE_FOR_ALL_SOCK` overrides
/// `~/.one-for-all/broker.sock`.
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("ONE_FOR_ALL_SOCK") {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("HOME not set"))?
        .join(".one-for-all")
        .join("broker.sock"))
}

/// Resolve the broker session capability set for `session.register`.
///
/// Default is the minimal MCP transport set (`tools`, `events`,
/// `binary-topics`, `storage_state`). Extra broker-facing capabilities are opt-in via
/// `ONE_FOR_ALL_CAPABILITIES=native,eval,face_detect,system,...`.
/// `eval` is intentionally opt-in even though `page.eval` is exposed as a tool.
fn register_capabilities() -> Vec<String> {
    register_capabilities_from(std::env::var("ONE_FOR_ALL_CAPABILITIES").ok().as_deref())
}

fn register_capabilities_from(raw: Option<&str>) -> Vec<String> {
    let mut caps: Vec<String> = DEFAULT_REGISTER_CAPABILITIES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    if let Some(raw) = raw {
        for cap in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !caps.iter().any(|existing| existing == cap) {
                caps.push(cap.to_owned());
            }
        }
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_capabilities_defaults_minimally() {
        let caps = register_capabilities_from(None);
        assert_eq!(
            caps,
            vec!["tools", "events", "binary-topics", "storage_state"]
        );
    }

    #[test]
    fn register_capabilities_appends_unique_opt_ins() {
        let caps = register_capabilities_from(Some("native,eval,events,face_detect,system"));
        assert_eq!(
            caps,
            vec![
                "tools",
                "events",
                "binary-topics",
                "storage_state",
                "native",
                "eval",
                "face_detect",
                "system",
            ]
        );
    }

    #[test]
    fn binary_vision_frame_reemits_public_json_shape() {
        let ev = broker::protocol::VisionFrameEvent {
            session_id: "sess-1".into(),
            tab_id: "tab-1".into(),
            ts_ms: 1234,
            frame_seq: 42,
            captured_us: 1_234_567,
            frame_handle: broker::protocol::FrameHandle {
                ring_path: "/tmp/ofa-frames-sess-1-tab-1".into(),
                slot: 7,
                slot_seq: 42,
                offset: 65_536,
                len: 184_320,
                ts_us: 1_234_567,
            },
            viewport: broker::protocol::Viewport {
                offset_top: 0.0,
                page_scale_factor: 1.0,
                device_width: 1920.0,
                device_height: 1080.0,
                scroll_offset_x: 0.0,
                scroll_offset_y: 0.0,
                timestamp: 0.0,
            },
            changed_tiles: vec![broker::protocol::TileRect {
                tile_x: 12,
                tile_y: 4,
                x: 768,
                y: 256,
                w: 64,
                h: 64,
                prev_hash: 11,
                next_hash: 22,
            }],
            ocr_delta: vec![broker::protocol::OcrEntry {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
                text: "Search".into(),
                confidence: 0.93,
            }],
            stability: Some(0.98),
            state: Some("stable".into()),
        };

        let mut line = Vec::new();
        broker::protocol::encode_vision_frame_into(&mut line, &ev).expect("encode");
        let payload = decode_binary_vision_frame(&line[..line.len() - 1]).expect("decode");

        assert_eq!(payload["topic"], "vision.frame");
        assert_eq!(payload["session_id"], "sess-1");
        assert_eq!(payload["tab_id"], "tab-1");
        assert_eq!(payload["seq"], 42);
        assert_eq!(payload["captured_us"], 1_234_567);
        assert_eq!(payload["viewport"]["device_width"], 1920.0);
        assert_eq!(payload["viewport"]["page_scale_factor"], 1.0);
        assert_eq!(payload["frame"]["shm_path"], "/tmp/ofa-frames-sess-1-tab-1");
        assert_eq!(payload["frame"]["slot_seq"], 42);
        assert_eq!(payload["frame"]["slot_index"], 7);
        assert_eq!(payload["frame"]["ts_us"], 1_234_567);
        assert_eq!(payload["changed_tiles"][0]["tile_x"], 12);
        assert_eq!(payload["changed_tiles"][0]["bbox"]["x"], 768);
        assert_eq!(payload["changed_tiles"][0]["prev_hash"], 11);
        assert_eq!(payload["changed_tiles"][0]["next_hash"], 22);
        assert_eq!(payload["ocr_delta"][0]["bbox"]["w"], 3);
        assert_eq!(payload["ocr_delta"][0]["text"], "Search");
        let confidence = payload["ocr_delta"][0]["confidence"]
            .as_f64()
            .expect("ocr confidence as f64");
        assert!((confidence - 0.93).abs() < 1e-6, "confidence={confidence}");
        let stability = payload["stability"].as_f64().expect("stability as f64");
        assert!((stability - 0.98).abs() < 1e-6, "stability={stability}");
        assert_eq!(payload["state"], "stable");
    }
}

#[derive(Debug, Clone)]
pub struct CallOptions {
    pub timeout: Duration,
}
impl Default for CallOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a, P: Serialize> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Debug, Deserialize)]
struct JsonRpcFrame {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorBody>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize, bincode::Decode)]
struct BrokerFrameHandle {
    ring_path: String,
    slot: u32,
    slot_seq: u64,
    offset: u64,
    len: u32,
    ts_us: u64,
}

#[derive(Debug, Deserialize, bincode::Decode)]
struct BrokerViewport {
    offset_top: f64,
    page_scale_factor: f64,
    device_width: f64,
    device_height: f64,
    scroll_offset_x: f64,
    scroll_offset_y: f64,
    timestamp: f64,
}

#[derive(Debug, Deserialize, bincode::Decode)]
struct BrokerTileRect {
    tile_x: u32,
    tile_y: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    prev_hash: u64,
    next_hash: u64,
}

#[derive(Debug, Deserialize, bincode::Decode)]
struct BrokerOcrEntry {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    text: String,
    confidence: f32,
}

#[derive(Debug, Deserialize, bincode::Decode)]
struct BrokerVisionFrameEvent {
    session_id: String,
    tab_id: String,
    ts_ms: u64,
    frame_seq: u64,
    captured_us: u64,
    frame_handle: BrokerFrameHandle,
    viewport: BrokerViewport,
    changed_tiles: Vec<BrokerTileRect>,
    ocr_delta: Vec<BrokerOcrEntry>,
    stability: Option<f32>,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

type Inflight = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BridgeError>>>>>;

#[derive(Clone)]
pub struct BrokerClient {
    inner: Arc<Inner>,
}

struct Inner {
    socket_path: PathBuf,
    session_id: parking_lot::RwLock<Option<String>>,
    next_id: AtomicU64,
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    inflight: Inflight,
    notifications: broadcast::Sender<BrokerNotification>,
}

#[derive(Debug, Clone)]
pub struct BrokerNotification {
    /// SPEC §2 — the broker's `event/notify` method literal verbatim. Used
    /// by `mcp-server::mcp` to forward to MCP stdout as a notification.
    pub method: String,
    /// SPEC §2 — the broker-side params payload (a `topic` discriminator
    /// plus topic-specific fields). Binary `vision.frame` notifications are
    /// re-emitted into this same JSON shape before they leave the broker
    /// client so MCP stdout stays JSON-only.
    pub params: Value,
}

/// `session.register` reply per SPEC §2.
#[derive(Debug, Deserialize)]
struct RegisterResult {
    session_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    broker_version: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    supported_methods: Option<Vec<String>>,
}

impl BrokerClient {
    /// Connects, attaches the reader, and performs the `session.register`
    /// handshake. Returns a fully-armed client.
    ///
    // CANCELLATION: safe. Dropping the future before completion leaves no
    // observable side effects beyond a possibly-half-open UnixStream that the
    // OS reclaims; the reader task is only spawned after a successful dial,
    // and `session.register` is naturally retryable.
    pub async fn connect(socket_path: PathBuf, client_name: &str) -> Result<Self, BridgeError> {
        let (notifications, _) = broadcast::channel(NOTIFICATION_CHANNEL_CAP);
        let inner = Arc::new(Inner {
            socket_path,
            session_id: parking_lot::RwLock::new(None),
            next_id: AtomicU64::new(1),
            writer: tokio::sync::Mutex::new(None),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            notifications,
        });
        let client = Self { inner };
        client.dial_and_attach().await?;
        client.register_session(client_name).await?;
        Ok(client)
    }

    /// Subscribe to broker `event/notify` notifications. Each subscriber
    /// gets its own [`broadcast::Receiver`]; lagged consumers see
    /// `RecvError::Lagged`. SPEC §2 + N12 — `mcp-server::mcp` forwards
    /// these to MCP stdout as LSP-framed JSON-RPC notifications.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<BrokerNotification> {
        self.inner.notifications.subscribe()
    }

    #[allow(dead_code)] // public API; consumed by future event-stream tools
    pub fn session_id(&self) -> Option<String> {
        self.inner.session_id.read().clone()
    }

    /// Direct method dispatch (SPEC §2). `method` is the canonical tool name
    /// such as `"tab.open"`; `params` is the per-method params object.
    ///
    // CANCELLATION: conditional. Cancelling the future cleans up the inflight
    // entry on drop of the oneshot receiver — but the request was already
    // written to the broker, so the broker may still execute it. Callers that
    // need at-most-once semantics must follow up with a compensating call.
    #[instrument(skip(self, params), fields(method = %method))]
    pub async fn call(
        &self,
        method: &'static str,
        params: Value,
        opts: CallOptions,
    ) -> Result<Value, BridgeError> {
        self.send_with_timeout(method, params, opts.timeout).await
    }

    async fn register_session(&self, client_name: &str) -> Result<(), BridgeError> {
        let capabilities = register_capabilities();
        let requested_session_id = std::env::var("OFA_SESSION_ID")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let params = json!({
            "client_name": client_name,
            "client_version": env!("CARGO_PKG_VERSION"),
            "capabilities": capabilities,
            "session_id": requested_session_id,
        });
        let raw = self
            .send_with_timeout("session.register", params, REGISTER_TIMEOUT)
            .await?;
        let parsed: RegisterResult = serde_json::from_value(raw).map_err(|e| {
            BridgeError::Protocol(format!("session.register result malformed: {e}"))
        })?;
        *self.inner.session_id.write() = Some(parsed.session_id.clone());
        info!(session_id = %parsed.session_id, "broker session registered");
        Ok(())
    }

    async fn send_with_timeout(
        &self,
        method: &'static str,
        params: Value,
        per_call: Duration,
    ) -> Result<Value, BridgeError> {
        let started = Instant::now();
        let mut last_err: Option<BridgeError> = None;
        for attempt in 0..2 {
            match self.send_once(method, &params, per_call).await {
                Ok(v) => return Ok(v),
                Err(e @ BridgeError::Timeout(_)) => return Err(e),
                Err(e @ BridgeError::BrokerError { .. }) => return Err(e),
                Err(e @ BridgeError::Cancelled) => return Err(e),
                Err(BridgeError::BrokerUnavailable(msg)) if attempt == 0 => {
                    warn!(error = %msg, "broker call failed at transport; redialing");
                    if let Err(e2) = self.dial_and_attach().await {
                        last_err = Some(e2);
                    }
                }
                Err(other) => return Err(other),
            }
            if started.elapsed() >= per_call {
                return Err(BridgeError::Timeout(per_call));
            }
        }
        Err(last_err.unwrap_or_else(|| BridgeError::BrokerUnavailable("redial failed".into())))
    }

    async fn send_once(
        &self,
        method: &'static str,
        params: &Value,
        per_call: Duration,
    ) -> Result<Value, BridgeError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = serde_json::to_vec(&JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })
        .map_err(|e| BridgeError::Internal(format!("serialize: {e}")))?;
        if frame.len() > MAX_LINE_BYTES {
            return Err(BridgeError::Protocol(format!(
                "outbound frame {} bytes exceeds 16MB cap",
                frame.len()
            )));
        }

        let (tx, rx) = oneshot::channel();
        self.inner.inflight.lock().insert(id, tx);

        {
            let mut guard = self.inner.writer.lock().await;
            let writer = guard
                .as_mut()
                .ok_or_else(|| BridgeError::BrokerUnavailable("writer absent".into()))?;
            if let Err(e) = write_line(writer, &frame).await {
                self.inner.inflight.lock().remove(&id);
                *guard = None;
                return Err(BridgeError::BrokerUnavailable(format!("write: {e}")));
            }
        }

        match timeout(per_call, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => {
                self.inner.inflight.lock().remove(&id);
                Err(BridgeError::BrokerUnavailable("inflight dropped".into()))
            }
            Err(_) => {
                self.inner.inflight.lock().remove(&id);
                Err(BridgeError::Timeout(per_call))
            }
        }
    }

    async fn dial_and_attach(&self) -> Result<(), BridgeError> {
        let stream = self.dial_with_kickstart().await?;
        let (read, write) = stream.into_split();
        *self.inner.writer.lock().await = Some(write);

        let inflight = Arc::clone(&self.inner.inflight);
        let notifications = self.inner.notifications.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(64 * 1024, read);
            let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => {
                        debug!("broker socket EOF");
                        break;
                    }
                    Ok(_) => {
                        if buf.len() > MAX_LINE_BYTES {
                            warn!(bytes = buf.len(), "broker frame exceeds 16MB cap; dropping");
                            continue;
                        }
                        if let Some(b'\n') = buf.last().copied() {
                            buf.pop();
                        }
                        if let Some(b'\r') = buf.last().copied() {
                            buf.pop();
                        }
                        if buf.is_empty() {
                            continue;
                        }
                        if buf[0] == 0x01 {
                            dispatch_binary_message(&buf, &notifications);
                            continue;
                        }
                        let line = match std::str::from_utf8(&buf) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(error = %e, "broker frame not utf-8");
                                continue;
                            }
                        };
                        match serde_json::from_str::<JsonRpcFrame>(line) {
                            Ok(msg) => dispatch_json_message(msg, &inflight, &notifications),
                            Err(e) => {
                                warn!(error = %e, raw = %line, "broker frame parse failed")
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "broker reader error");
                        break;
                    }
                }
            }
            // Reader exit → fail every inflight call.
            let mut map = inflight.lock();
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(BridgeError::BrokerUnavailable("socket closed".into())));
            }
        });

        Ok(())
    }

    async fn dial_with_kickstart(&self) -> Result<UnixStream, BridgeError> {
        if let Ok(s) = UnixStream::connect(&self.inner.socket_path).await {
            return Ok(s);
        }
        kickstart_launchd().await;
        for &ms in CONNECT_BACKOFF_MS {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            match UnixStream::connect(&self.inner.socket_path).await {
                Ok(s) => {
                    info!(path = %self.inner.socket_path.display(), "broker connected after kickstart");
                    return Ok(s);
                }
                Err(e) => debug!(error = %e, after_ms = ms, "broker dial retry"),
            }
        }
        Err(BridgeError::BrokerUnavailable(format!(
            "could not connect to {} after kickstart",
            self.inner.socket_path.display()
        )))
    }
}

fn broker_vision_frame_to_json(ev: BrokerVisionFrameEvent) -> Value {
    let mut payload = json!({
        "topic": "vision.frame",
        "session_id": ev.session_id,
        "tab_id": ev.tab_id,
        "seq": ev.frame_seq,
        "captured_us": ev.captured_us,
        "viewport": {
            "offset_top": ev.viewport.offset_top,
            "page_scale_factor": ev.viewport.page_scale_factor,
            "device_width": ev.viewport.device_width,
            "device_height": ev.viewport.device_height,
            "scroll_offset_x": ev.viewport.scroll_offset_x,
            "scroll_offset_y": ev.viewport.scroll_offset_y,
            "timestamp": ev.viewport.timestamp,
        },
        "frame": {
            "shm_path": ev.frame_handle.ring_path,
            "slot_seq": ev.frame_handle.slot_seq,
            "slot_index": ev.frame_handle.slot,
            "offset": ev.frame_handle.offset,
            "len": ev.frame_handle.len,
            "ts_us": ev.frame_handle.ts_us,
        },
        "changed_tiles": ev.changed_tiles.into_iter().map(|tile| json!({
            "tile_x": tile.tile_x,
            "tile_y": tile.tile_y,
            "bbox": {
                "x": tile.x,
                "y": tile.y,
                "w": tile.w,
                "h": tile.h,
            },
            "prev_hash": tile.prev_hash,
            "next_hash": tile.next_hash,
        })).collect::<Vec<_>>(),
        "ocr_delta": ev.ocr_delta.into_iter().map(|entry| json!({
            "bbox": {
                "x": entry.x,
                "y": entry.y,
                "w": entry.w,
                "h": entry.h,
            },
            "text": entry.text,
            "confidence": entry.confidence,
        })).collect::<Vec<_>>(),
    });
    if let Some(stability) = ev.stability {
        payload["stability"] = json!(stability);
    }
    if let Some(state) = ev.state {
        payload["state"] = json!(state);
    }
    payload
}

fn dispatch_json_message(
    msg: JsonRpcFrame,
    inflight: &Inflight,
    notifications: &broadcast::Sender<BrokerNotification>,
) {
    match (msg.id, msg.method) {
        (Some(id), _) => {
            if let Some(tx) = inflight.lock().remove(&id) {
                let result = if let Some(err) = msg.error {
                    Err(BridgeError::BrokerError {
                        code: err.code,
                        message: err.message,
                        data: err.data,
                    })
                } else {
                    Ok(msg.result.unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            } else {
                warn!(id, "unmatched broker response");
            }
        }
        (None, Some(method)) => {
            let params = msg.params.unwrap_or(Value::Null);
            let _ = notifications.send(BrokerNotification { method, params });
        }
        (None, None) => warn!("broker frame had neither id nor method"),
    }
}

fn dispatch_binary_message(line: &[u8], notifications: &broadcast::Sender<BrokerNotification>) {
    let payload = match decode_binary_vision_frame(line) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "broker binary frame parse failed");
            return;
        }
    };
    let _ = notifications.send(BrokerNotification {
        method: "event/notify".to_string(),
        params: payload,
    });
}

fn decode_binary_vision_frame(line: &[u8]) -> Result<Value, BridgeError> {
    let first = line
        .first()
        .copied()
        .ok_or_else(|| BridgeError::Protocol("empty broker frame".into()))?;
    if first != 0x01 {
        return Err(BridgeError::Protocol("not a binary broker frame".into()));
    }
    if line.len() < 5 {
        return Err(BridgeError::Protocol(
            "binary broker frame shorter than header".into(),
        ));
    }
    let len = u32::from_le_bytes([line[1], line[2], line[3], line[4]]) as usize;
    if line.len() != 5 + len {
        return Err(BridgeError::Protocol(
            "binary broker frame length mismatch".into(),
        ));
    }
    let payload = &line[5..];
    let cfg = bincode::config::standard();
    let (ev, _read) = bincode::decode_from_slice::<BrokerVisionFrameEvent, _>(payload, cfg)
        .map_err(|e| BridgeError::Protocol(format!("binary vision.frame decode: {e}")))?;
    Ok(broker_vision_frame_to_json(ev))
}

fn dispatch_message(
    msg: JsonRpcFrame,
    inflight: &Inflight,
    notifications: &broadcast::Sender<BrokerNotification>,
) {
    dispatch_json_message(msg, inflight, notifications);
}

async fn write_line(w: &mut OwnedWriteHalf, frame: &[u8]) -> anyhow::Result<()> {
    w.write_all(frame).await.context("write frame")?;
    w.write_all(b"\n").await.context("write newline")?;
    w.flush().await.context("flush")?;
    Ok(())
}

async fn kickstart_launchd() {
    let uid = current_euid();
    let target = format!("gui/{uid}/{KICKSTART_LABEL}");
    let _ = tokio::process::Command::new("launchctl")
        .arg("kickstart")
        .arg("-k")
        .arg(&target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

extern "C" {
    fn geteuid() -> u32;
}
fn current_euid() -> u32 {
    // SAFETY: geteuid is async-signal-safe and has no preconditions.
    unsafe { geteuid() }
}

// Keep the unused `jsonrpc_code` import alive for downstream consumers.
#[allow(dead_code)]
fn _retain_codes() -> i64 {
    jsonrpc_code::BROKER_UNAVAILABLE
}
