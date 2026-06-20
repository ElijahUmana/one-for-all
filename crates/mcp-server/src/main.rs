//! `one-for-all-mcp` — the stdio MCP server `claude` auto-spawns.
//!
//! Per SPEC §D8, each `claude` CLI session spawns its own MCP server. This
//! process connects to the broker (auto-kickstarting the launchd job if down)
//! and forwards `tools/call` to per-method JSON-RPC requests on the broker
//! socket. Broker-facing capability registration defaults to
//! `tools,events,binary-topics` and can be widened explicitly with
//! `ONE_FOR_ALL_CAPABILITIES=native,eval,face_detect,system,...`.
//! If the broker is genuinely unreachable, the server stays up and returns
//! crashing the MCP child.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

mod broker_client;
mod error;
mod mcp;
mod schema;
mod tools;

use anyhow::Result;
use tracing::{error, info};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let _guard = observability::init("mcp")?;

    let socket_path = broker_client::default_socket_path()?;
    info!(
        socket = %socket_path.display(),
        version = env!("CARGO_PKG_VERSION"),
        "starting one-for-all-mcp"
    );

    let broker =
        match broker_client::BrokerClient::connect(socket_path.clone(), "one-for-all-mcp").await {
            Ok(b) => b,
            Err(e) => {
                error!(error = %e, "broker unavailable; running with stubbed connection");
                return run_brokerless(e).await;
            }
        };

    if let Err(e) = mcp::run(broker).await {
        error!(error = %e, "mcp loop terminated with error");
        return Err(e);
    }
    Ok(())
}

/// Fallback loop when the broker is unreachable. Still answers `initialize`
/// and `tools/list`, returns structured errors for `tools/call`.
async fn run_brokerless(err: error::BridgeError) -> Result<()> {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex as AsyncMutex;

    async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = r.read(&mut byte).await?;
            if n == 0 {
                return Ok(None);
            }
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
            if header.len() > 8 * 1024 {
                return Err(anyhow::anyhow!("header too long"));
            }
        }
        let h = std::str::from_utf8(&header)?;
        let mut len: usize = 0;
        for line in h.split("\r\n") {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse()?;
                }
            }
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    let stdin = tokio::io::stdin();
    let stdout = Arc::new(AsyncMutex::new(tokio::io::stdout()));
    let mut reader = BufReader::new(stdin);

    while let Some(body) = read_frame(&mut reader).await? {
        let v: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = v.get("id").cloned();
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let resp = match method {
            "initialize" => Some(serde_json::json!({
                "jsonrpc":"2.0","id":id,
                "result":{
                    "protocolVersion":"2024-11-05",
                    "serverInfo":{"name":"one-for-all","version":env!("CARGO_PKG_VERSION")},
                    "capabilities":{"tools":{"listChanged":false}}
                }
            })),
            "ping" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}})),
            "tools/list" => Some(serde_json::json!({
                "jsonrpc":"2.0","id":id,"result":{"tools":tools::list()}
            })),
            "tools/call" => Some(serde_json::json!({
                "jsonrpc":"2.0","id":id,"error":error::to_jsonrpc_error(&err)
            })),
            _ if id.is_none() => None,
            _ => Some(serde_json::json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":-32601,"message":format!("unknown method: {method}")}
            })),
        };
        if let Some(r) = resp {
            let body = serde_json::to_vec(&r)?;
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            let mut g = stdout.lock().await;
            g.write_all(header.as_bytes()).await?;
            g.write_all(&body).await?;
            g.flush().await?;
        }
    }
    Ok(())
}
