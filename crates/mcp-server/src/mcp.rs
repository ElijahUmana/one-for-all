//! MCP stdio protocol layer.
//!
//! Per SPEC §2 the claude ↔ MCP transport uses LSP-style framing:
//!
//!     Content-Length: <bytes>\r\n
//!     \r\n
//!     <body>
//!
//! Body is JSON-RPC 2.0 UTF-8.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::broker_client::BrokerClient;
use crate::error::{to_jsonrpc_error, BridgeError, JsonRpcError};
use crate::tools;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "one-for-all";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Frame {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ResponseOk<'a> {
    jsonrpc: &'a str,
    id: Value,
    result: Value,
}

#[derive(Debug, Serialize)]
struct ResponseErr<'a> {
    jsonrpc: &'a str,
    id: Value,
    error: JsonRpcError,
}

/// Run the MCP stdio loop until stdin closes.
///
// CANCELLATION: safe. The loop only borrows stdin/stdout; cancelling drops
// in-flight per-request tasks via the JoinSet, and we drain (with a deadline)
// any still-running tasks at shutdown so client responses are not lost when
// stdin EOFs mid-call.
pub async fn run(broker: BrokerClient) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = Arc::new(AsyncMutex::new(tokio::io::stdout()));
    let mut reader = BufReader::with_capacity(64 * 1024, stdin);
    let cancellations: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut tasks: JoinSet<()> = JoinSet::new();

    info!(version = SERVER_VERSION, "mcp stdio loop started");

    // SPEC §2 + N12 — forward every broker `event/notify` to MCP stdout as an
    // LSP-framed JSON-RPC notification (no `id`). The reader half is owned by
    // this task; the broker's reader actor delivers each notify via the
    // broadcast channel exposed by `subscribe_notifications`. Without this
    // forwarder, every server-side topic (session.recovered,
    // console.message, page.exception, network.*, dialog.*, download.*,
    // tab.*) is dropped on the floor.
    let forwarder_stdout = Arc::clone(&stdout);
    let mut forwarder_rx = broker.subscribe_notifications();
    tokio::spawn(async move {
        loop {
            match forwarder_rx.recv().await {
                Ok(n) => {
                    if let Err(e) = write_notify(&forwarder_stdout, &n.method, &n.params).await {
                        warn!(error = %e, method = %n.method, "forwarding broker notify failed");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "broker notify backlog: skipped events");
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    loop {
        let body = match read_lsp_frame(&mut reader).await? {
            Some(b) => b,
            None => {
                info!("stdin EOF; shutting down");
                break;
            }
        };
        let frame: Frame = match serde_json::from_slice(&body) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "client sent unparseable frame");
                continue;
            }
        };

        let id = frame.id.clone();
        let method = frame.method.clone().unwrap_or_default();
        let params = frame.params.unwrap_or(Value::Null);

        let stdout = Arc::clone(&stdout);
        let broker = broker.clone();
        let cancellations = Arc::clone(&cancellations);
        // Drain finished tasks opportunistically so the JoinSet doesn't grow
        // unbounded on a long-lived connection.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let Some(id) = id else {
                handle_notification(&method, params, &cancellations).await;
                return;
            };
            let result = handle_request(&broker, &method, params, &id, &cancellations).await;
            if let Err(e) = write_response(&stdout, &id, result).await {
                error!(error = %e, "failed to write response");
            }
        });
    }

    // Drain in-flight tool calls with a hard deadline; clients prefer late
    // structured replies over silently-lost work.
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        warn!(
            remaining = tasks.len(),
            "shutdown drain timeout; aborting in-flight tasks"
        );
        tasks.abort_all();
    }
    Ok(())
}

/// Read one LSP-framed JSON body from `r`. Returns `Ok(None)` on clean EOF.
async fn read_lsp_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    // Read headers terminated by \r\n\r\n.
    let mut header = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte).await?;
        if n == 0 {
            // EOF before any bytes → done. Mid-frame EOF → also done; we treat
            // the partial frame as EOF.
            if header.is_empty() {
                return Ok(None);
            }
            return Ok(None);
        }
        header.push(byte[0]);
        if header.len() >= 4 && header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 8 * 1024 {
            return Err(anyhow::anyhow!("LSP header exceeds 8KB"));
        }
    }
    let header_str = std::str::from_utf8(&header)?;
    let mut content_length: Option<usize> = None;
    for line in header_str.split("\r\n") {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    v.trim()
                        .parse()
                        .map_err(|e| anyhow::anyhow!("bad Content-Length: {e}"))?,
                );
            }
        }
    }
    let len = content_length.ok_or_else(|| anyhow::anyhow!("missing Content-Length"))?;
    if len > MAX_BODY_BYTES {
        return Err(anyhow::anyhow!("body length {len} exceeds 16MB"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn handle_request(
    broker: &BrokerClient,
    method: &str,
    params: Value,
    id: &Value,
    cancellations: &Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
) -> Result<Value, BridgeError> {
    debug!(method, "request");
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            "capabilities": { "tools": { "listChanged": false } },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list() })),
        "tools/call" => handle_tools_call(broker, params, id, cancellations).await,
        other => Err(BridgeError::Protocol(format!("unknown method: {other}"))),
    }
}

async fn handle_tools_call(
    broker: &BrokerClient,
    params: Value,
    id: &Value,
    cancellations: &Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
) -> Result<Value, BridgeError> {
    #[derive(Deserialize)]
    struct CallParams {
        name: String,
        #[serde(default)]
        arguments: Value,
    }
    let p: CallParams = serde_json::from_value(params)?;

    let id_str = match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let (cancel_tx, cancel_rx) = oneshot::channel();
    cancellations.lock().insert(id_str.clone(), cancel_tx);

    let dispatch = tools::dispatch(broker, &p.name, p.arguments);
    let result = tokio::select! {
        biased;
        _ = cancel_rx => {
            cancellations.lock().remove(&id_str);
            return Err(BridgeError::Cancelled);
        }
        r = dispatch => r,
    };
    cancellations.lock().remove(&id_str);

    let raw = result?;
    Ok(into_tool_result(&p.name, raw))
}

/// Wrap a raw broker result in the MCP `tools/call` response envelope.
fn into_tool_result(tool: &str, raw: Value) -> Value {
    if matches!(
        tool,
        "page.screenshot" | "system.camera.snapshot" | "system.screen.capture_region"
    ) {
        let format = raw
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("png")
            .to_owned();
        let data = raw
            .get("data_base64")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return json!({
            "content": [{
                "type": "image",
                "data": data,
                "mimeType": format!("image/{format}"),
            }]
        });
    }
    let pretty = serde_json::to_string_pretty(&raw).unwrap_or_else(|_| raw.to_string());
    json!({
        "content": [{ "type": "text", "text": pretty }]
    })
}

async fn handle_notification(
    method: &str,
    params: Value,
    cancellations: &Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
) {
    match method {
        "notifications/initialized" => debug!("client initialized"),
        "notifications/cancelled" => {
            #[derive(Deserialize)]
            struct CancelParams {
                #[serde(rename = "requestId")]
                request_id: Value,
                #[serde(default)]
                reason: Option<String>,
            }
            if let Ok(p) = serde_json::from_value::<CancelParams>(params) {
                let id_str = match p.request_id {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                if let Some(tx) = cancellations.lock().remove(&id_str) {
                    let _ = tx.send(());
                    info!(id = %id_str, reason = ?p.reason, "request cancelled by client");
                }
            }
        }
        other => debug!(method = other, "ignoring notification"),
    }
}

async fn write_response(
    stdout: &Arc<AsyncMutex<tokio::io::Stdout>>,
    id: &Value,
    result: Result<Value, BridgeError>,
) -> Result<()> {
    let body = match result {
        Ok(v) => serde_json::to_vec(&ResponseOk {
            jsonrpc: "2.0",
            id: id.clone(),
            result: v,
        })?,
        Err(e) => serde_json::to_vec(&ResponseErr {
            jsonrpc: "2.0",
            id: id.clone(),
            error: to_jsonrpc_error(&e),
        })?,
    };
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut g = stdout.lock().await;
    g.write_all(header.as_bytes()).await?;
    g.write_all(&body).await?;
    g.flush().await?;
    Ok(())
}

/// SPEC §2 + N12 — write a server-side JSON-RPC notification to stdout in
/// LSP framing. `method` is the broker's `event/notify` method literal,
/// `params` is the broker-side payload object verbatim. No `id` field per
/// JSON-RPC 2.0 notification semantics.
async fn write_notify(
    stdout: &Arc<AsyncMutex<tokio::io::Stdout>>,
    method: &str,
    params: &Value,
) -> Result<()> {
    let bytes = frame_notify_bytes(method, params)?;
    let mut g = stdout.lock().await;
    g.write_all(&bytes).await?;
    g.flush().await?;
    Ok(())
}

/// Pure helper — build the full LSP frame (header + body) for a JSON-RPC
/// notification. Pulled out so a unit test can assert on the byte shape
/// without needing to capture real stdout.
fn frame_notify_bytes(method: &str, params: &Value) -> Result<Vec<u8>> {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&frame)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn read_lsp_roundtrip() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut buf = Vec::new();
        buf.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        buf.extend_from_slice(body);
        let mut r = Cursor::new(buf);
        let got = read_lsp_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(&got, body);
    }

    #[tokio::test]
    async fn read_lsp_clean_eof() {
        let mut r = Cursor::new(Vec::<u8>::new());
        assert!(read_lsp_frame(&mut r).await.unwrap().is_none());
    }

    /// N12 — broker `event/notify` envelopes round-trip cleanly through the
    /// MCP stdio frame layer. The forwarder builds the bytes here; the
    /// reader proves they decode back to the same notification shape with
    /// no `id`, the broker's method verbatim, and the params object intact.
    #[tokio::test]
    async fn frame_notify_round_trip() {
        let params = serde_json::json!({
            "topic": "session.recovered",
            "session_id": "s_42",
            "previous_tab_ids": ["t_a", "t_b"],
            "new_tab_ids": ["t_c"],
            "payload": {},
        });
        let bytes = frame_notify_bytes("event/notify", &params).unwrap();
        let mut r = Cursor::new(bytes);
        let body = read_lsp_frame(&mut r).await.unwrap().unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
        assert_eq!(
            v.get("method").and_then(Value::as_str),
            Some("event/notify")
        );
        assert!(v.get("id").is_none(), "notifications must omit id");
        assert_eq!(v.get("params"), Some(&params));
    }
}
