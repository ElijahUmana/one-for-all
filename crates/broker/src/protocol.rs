//! Wire-protocol types — byte-exact match for SPEC §2.
//!
//! ## Binary topics (SPEC §11 V5)
//!
//! The broker socket is JSON-RPC line-delimited by default. SPEC §11 V5
//! adds an opt-in escape: any line whose first byte is `0x01` is a
//! length-prefixed bincode frame, not JSON. The single supported binary
//! topic today is `vision.frame`.
//!
//! Frame layout:
//! ```text
//! 0x01 | u32 little-endian payload_len | <bincode bytes…> | 0x0A (newline)
//! ```
//! `0x01` is invalid as the first byte of any JSON-RPC line (which must
//! start with `{`), so coexistence is byte-unambiguous and forward-
//! compatible. Clients that do not advertise `binary-topics` in
//! `session.register.capabilities` continue to receive `vision.frame` as
//! JSON; mcp-server advertises the capability and decodes on the way out
//! to the MCP stdio (LSP) transport, which requires JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// SPEC §2 wire envelope: `{jsonrpc, id, method, params}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// `None` indicates a notification (no response expected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Either `result` or `error` is `Some`, never both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// SPEC §2 server→client notification envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEvent {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(
        id: Value,
        code: ErrorCode,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: code.into(),
                message: message.into(),
                data,
            }),
        }
    }
}

/// SPEC D17 error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Standard JSON-RPC.
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    // Server-defined (SPEC D17).
    SessionNotFound,
    TabNotFound,
    ContextNotFound,
    ElementStale,
    ElementNotActionable,
    NavigationFailed,
    Timeout,
    ChromiumLaunchFailed,
    PermissionDenied,
    ProtocolError,
    BrokerUnavailable,
    SessionLimitExceeded,
}

impl From<ErrorCode> for i64 {
    fn from(c: ErrorCode) -> i64 {
        match c {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::SessionNotFound => -32001,
            ErrorCode::TabNotFound => -32002,
            ErrorCode::ContextNotFound => -32003,
            ErrorCode::ElementStale => -32004,
            ErrorCode::ElementNotActionable => -32005,
            ErrorCode::NavigationFailed => -32006,
            ErrorCode::Timeout => -32007,
            ErrorCode::ChromiumLaunchFailed => -32008,
            ErrorCode::PermissionDenied => -32009,
            ErrorCode::ProtocolError => -32010,
            ErrorCode::BrokerUnavailable => -32011,
            ErrorCode::SessionLimitExceeded => -32012,
        }
    }
}

/// SPEC §2.1 — `session.register` request params.
///
/// SPEC §11 V3 extension: optional `inherit` array — see `sandbox::parse_inherit_keys`
/// for accepted values. When omitted, the broker applies `sandbox::default_allowlist()`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRegisterParams {
    pub client_name: String,
    pub client_version: String,
    /// Optional existing session id to bind this connection to instead of
    /// creating a fresh per-session Chromium. Used by `ofa spawn -- <cmd>` via
    /// `OFA_SESSION_ID=<id>` so the child Claude session reuses the detached
    /// broker session that `agent.spawn_subagent` already created.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// SPEC §11 V3 — host-state inheritance allowlist.
    /// `["cookies", "downloads", "ssh-readonly", "/abs/path", ...]`.
    #[serde(default)]
    pub inherit: Option<Vec<String>>,
    /// SPEC §11 V3 — when false, the per-session sandbox profile denies
    /// all network. Default `true`. The pf-anchor isolation is a separate
    /// V-R6 Phase 2.5 milestone.
    #[serde(default)]
    pub network_outbound: Option<bool>,
    /// SPEC §10 M10 — when `true`, broker writes a structured trace JSONL
    /// file under `~/.one-for-all/sessions/<id>/trace/` capturing every
    /// CDP request/response, screenshot-on-action, and DOM snapshot every
    /// 500 ms for the lifetime of this session.
    #[serde(default)]
    pub trace: bool,
    /// SPEC §10 M10 — per-session PII redaction patterns. Each entry is a
    /// regex; matched substrings inside string-typed JSON nodes of `params`
    /// / `args` / `result` are replaced with `<REDACTED>` BEFORE the trace
    /// line hits disk. Cookies, auth headers, and other deny-listed keys
    /// are also scrubbed structurally regardless of pattern. Empty by
    /// default. Only consulted when `trace=true`.
    #[serde(default)]
    pub redact_patterns: Vec<String>,
    /// SPEC §10 M10 — optional HMAC-SHA256 key for the trace manifest.
    /// When set, `<trace_dir>/manifest.json` carries an `hmac` field over
    /// the canonical JSON of the rest of the doc; `ofa-replay verify`
    /// recomputes and asserts. Hex-encoded if the value starts with
    /// `hex:`, otherwise raw bytes of the string. Only consulted when
    /// `trace=true`.
    #[serde(default)]
    pub trace_hmac_key: Option<String>,
    /// SPEC §12 U13 — substring-any list of bundle ids the session is
    /// forbidden to touch via any `app.*` call. Empty = no blocklist.
    #[serde(default)]
    pub app_blocklist: Vec<String>,
}

/// SPEC §2.1 — `session.register` response.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRegisterResult {
    pub session_id: String,
    pub broker_version: String,
    pub supported_methods: Vec<String>,
    pub supported_events: Vec<String>,
}

// ---------------------------------------------------------------------------
// SPEC §11 V5 — binary topic envelope
// ---------------------------------------------------------------------------

/// SPEC §11 V5 magic prefix byte. A line whose first byte is `0x01` is a
/// length-prefixed bincode envelope; otherwise it is JSON-RPC. `0x01` is
/// not a valid first byte for any JSON-RPC line (which must begin with
/// `{`), so the two paths are byte-unambiguous.
pub const BINARY_TOPIC_MAGIC: u8 = 0x01;

/// SPEC §11 V5 — `vision.frame` event payload, encoded with bincode 2.0
/// for the high-frequency screencast path. JSON re-emission is performed
/// in `mcp-server` for the MCP stdio (LSP) transport.
///
/// The byte-shape is bincode-canonical. On the MCP stdio path,
/// `mcp-server::broker_client::broker_vision_frame_to_json` remaps this
/// binary payload into the public JSON `vision.frame` shape documented in
/// SPEC §7 / `docs/PROTOCOL.md`.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct VisionFrameEvent {
    pub session_id: String,
    pub tab_id: String,
    pub ts_ms: u64,
    pub frame_seq: u64,
    pub captured_us: u64,
    pub frame_handle: FrameHandle,
    pub viewport: Viewport,
    pub changed_tiles: Vec<TileRect>,
    pub ocr_delta: Vec<OcrEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// SPEC §11 V4/V5 — handle to a frame in the shared-memory ring (frame
/// payload is in `~/.one-for-all/sessions/<id>/frame-ring.bin`).
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct FrameHandle {
    pub ring_path: String,
    pub slot: u32,
    pub slot_seq: u64,
    pub offset: u64,
    pub len: u32,
    pub ts_us: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct Viewport {
    pub offset_top: f64,
    pub page_scale_factor: f64,
    pub device_width: f64,
    pub device_height: f64,
    pub scroll_offset_x: f64,
    pub scroll_offset_y: f64,
    pub timestamp: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct TileRect {
    pub tile_x: u32,
    pub tile_y: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub prev_hash: u64,
    pub next_hash: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct OcrEntry {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub text: String,
    pub confidence: f32,
}

/// Decoded socket line — either control-plane JSON-RPC or a binary topic.
#[derive(Debug, Clone)]
pub enum WireFrame {
    Json(JsonRpcRequest),
    VisionFrame(VisionFrameEvent),
}

/// Errors from `decode_line` / `encode_vision_frame_into`.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("empty line")]
    Empty,
    #[error("malformed binary header: {0}")]
    MalformedBinary(&'static str),
    #[error("bincode decode: {0}")]
    Bincode(String),
    #[error("json decode: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Try to decode one logical socket line into a [`WireFrame`].
///
/// `bytes` must be the line content WITHOUT the trailing newline (the
/// reader is expected to have already split on `\n`).
pub fn decode_line(bytes: &[u8]) -> std::result::Result<WireFrame, WireError> {
    let first = match bytes.first() {
        Some(b) => *b,
        None => return Err(WireError::Empty),
    };
    if first == BINARY_TOPIC_MAGIC {
        // 0x01 | u32 LE len | <payload>
        if bytes.len() < 5 {
            return Err(WireError::MalformedBinary("line shorter than header"));
        }
        let len = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        if bytes.len() != 5 + len {
            return Err(WireError::MalformedBinary("declared length mismatch"));
        }
        let payload = &bytes[5..5 + len];
        let cfg = bincode::config::standard();
        let (ev, _read) = bincode::decode_from_slice::<VisionFrameEvent, _>(payload, cfg)
            .map_err(|e| WireError::Bincode(e.to_string()))?;
        Ok(WireFrame::VisionFrame(ev))
    } else {
        let req: JsonRpcRequest = serde_json::from_slice(bytes)?;
        Ok(WireFrame::Json(req))
    }
}

/// Encode a [`VisionFrameEvent`] into the caller's scratch buffer using the
/// SPEC §11 V5 binary framing. Appends `0x01 | u32 LE len | payload | 0x0A`.
///
/// Hot path — must not allocate beyond what `scratch.reserve` already did.
/// Callers (broker writer task) hold a per-conn `Vec<u8>` and `clear()` it
/// between frames.
pub fn encode_vision_frame_into(
    scratch: &mut Vec<u8>,
    ev: &VisionFrameEvent,
) -> std::result::Result<(), WireError> {
    scratch.push(BINARY_TOPIC_MAGIC);
    // Reserve the length slot; we'll write it after we know the size.
    let len_pos = scratch.len();
    scratch.extend_from_slice(&[0u8; 4]);
    let payload_start = scratch.len();
    let cfg = bincode::config::standard();
    bincode::encode_into_std_write(ev, scratch, cfg)
        .map_err(|e| WireError::Bincode(e.to_string()))?;
    let payload_len = scratch.len() - payload_start;
    let len_bytes = (payload_len as u32).to_le_bytes();
    scratch[len_pos..len_pos + 4].copy_from_slice(&len_bytes);
    scratch.push(b'\n');
    Ok(())
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn ev() -> VisionFrameEvent {
        VisionFrameEvent {
            session_id: "s1".into(),
            tab_id: "t1".into(),
            ts_ms: 12345,
            frame_seq: 7,
            captured_us: 12345000,
            frame_handle: FrameHandle {
                ring_path: "/tmp/ofa-frames-s1.bin".into(),
                slot: 3,
                slot_seq: 7,
                offset: 1536,
                len: 4096,
                ts_us: 12345000,
            },
            viewport: Viewport {
                offset_top: 0.0,
                page_scale_factor: 2.0,
                device_width: 1280.0,
                device_height: 720.0,
                scroll_offset_x: 0.0,
                scroll_offset_y: 0.0,
                timestamp: 0.0,
            },
            changed_tiles: vec![TileRect {
                tile_x: 0,
                tile_y: 0,
                x: 0,
                y: 0,
                w: 64,
                h: 64,
                prev_hash: 0xABCD1234,
                next_hash: 0xDEADBEEF,
            }],
            ocr_delta: vec![OcrEntry {
                x: 10,
                y: 20,
                w: 100,
                h: 24,
                text: "hello".into(),
                confidence: 0.95,
            }],
            stability: Some(0.99),
            state: Some("stable".into()),
        }
    }

    #[test]
    fn binary_round_trip_via_scratch() {
        let mut scratch = Vec::with_capacity(256);
        let original = ev();
        encode_vision_frame_into(&mut scratch, &original).unwrap();
        // Last byte is newline; strip and decode.
        assert_eq!(scratch.last().copied(), Some(b'\n'));
        let line = &scratch[..scratch.len() - 1];
        match decode_line(line).unwrap() {
            WireFrame::VisionFrame(decoded) => {
                assert_eq!(decoded.session_id, original.session_id);
                assert_eq!(decoded.tab_id, original.tab_id);
                assert_eq!(decoded.ts_ms, original.ts_ms);
                assert_eq!(decoded.frame_seq, original.frame_seq);
                assert_eq!(
                    decoded.frame_handle.slot_seq,
                    original.frame_handle.slot_seq
                );
                assert_eq!(decoded.frame_handle.ts_us, original.frame_handle.ts_us);
                assert_eq!(
                    decoded.viewport.device_width,
                    original.viewport.device_width
                );
                assert_eq!(
                    decoded.viewport.page_scale_factor,
                    original.viewport.page_scale_factor
                );
                assert_eq!(
                    decoded.changed_tiles[0].tile_x,
                    original.changed_tiles[0].tile_x
                );
                assert_eq!(
                    decoded.changed_tiles[0].prev_hash,
                    original.changed_tiles[0].prev_hash
                );
                assert_eq!(
                    decoded.changed_tiles[0].next_hash,
                    original.changed_tiles[0].next_hash
                );
                assert_eq!(decoded.ocr_delta[0].text, "hello");
                assert_eq!(
                    decoded.ocr_delta[0].confidence,
                    original.ocr_delta[0].confidence
                );
            }
            _ => panic!("expected VisionFrame variant"),
        }
    }

    #[test]
    fn json_line_decodes_as_json() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"page.snapshot","params":{}}"#;
        match decode_line(line).unwrap() {
            WireFrame::Json(req) => {
                assert_eq!(req.method, "page.snapshot");
                assert_eq!(req.id, Some(serde_json::json!(1)));
            }
            _ => panic!("expected Json variant"),
        }
    }

    #[test]
    fn empty_line_is_error() {
        let err = decode_line(&[]).unwrap_err();
        assert!(matches!(err, WireError::Empty));
    }

    #[test]
    fn binary_with_truncated_header_errors() {
        let line = [BINARY_TOPIC_MAGIC, 0u8];
        let err = decode_line(&line).unwrap_err();
        assert!(matches!(err, WireError::MalformedBinary(_)));
    }

    #[test]
    fn binary_with_length_mismatch_errors() {
        let line = [BINARY_TOPIC_MAGIC, 99, 0, 0, 0]; // claims 99 bytes, has 0
        let err = decode_line(&line).unwrap_err();
        assert!(matches!(err, WireError::MalformedBinary(_)));
    }

    #[test]
    fn bincode_is_dramatically_smaller_than_json_for_vision_frame() {
        // SPEC §11 V5 — the entire reason we did this work.
        let original = ev();
        let mut binc = Vec::new();
        encode_vision_frame_into(&mut binc, &original).unwrap();
        let jsn = serde_json::to_vec(&original).unwrap();
        // bincode header (1 + 4 + payload + 1) vs JSON; ratio depends on
        // payload but for realistic frames bincode is materially smaller.
        // Assert at least 30% smaller for our test event.
        assert!(
            binc.len() * 100 < jsn.len() * 70,
            "bincode {} bytes vs json {} bytes",
            binc.len(),
            jsn.len()
        );
    }

    #[test]
    fn scratch_buffer_is_reusable() {
        let mut scratch = Vec::with_capacity(256);
        let cap_initial = scratch.capacity();
        for i in 0..16u64 {
            scratch.clear();
            let mut e = ev();
            e.frame_seq = i;
            encode_vision_frame_into(&mut scratch, &e).unwrap();
        }
        // Capacity preserved (Vec never shrinks on `clear()`).
        assert!(scratch.capacity() >= cap_initial);
    }
}
