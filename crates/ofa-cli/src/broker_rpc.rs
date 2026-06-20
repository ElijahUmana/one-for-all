//! Minimal NDJSON JSON-RPC client for the broker socket. Per SPEC §2 every
//! line is one full JSON object terminated by `\n`. We use the
//! current-thread runtime so the CLI starts in <50 ms cold.
//!
//! This is a deliberately lightweight peer to `crates/mcp-server/src/broker_client.rs`:
//! the MCP server runs as a long-lived process inside Claude Code, so it
//! invests in reconnect logic, broadcast notifications, and per-call
//! cancellation. The CLI is a fire-and-forget short-lived process, so we
//! optimize for boot time and clarity over robustness.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct CliBrokerClient {
    stream: BufReader<UnixStream>,
    next_id: AtomicU64,
}

impl CliBrokerClient {
    /// Connect, perform `session.register`, return a ready client.
    pub(crate) async fn connect(socket_path: &Path) -> Result<Self> {
        let s = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connecting to broker socket {}", socket_path.display()))?;
        let mut client = Self {
            stream: BufReader::new(s),
            next_id: AtomicU64::new(1),
        };
        let _reg = client
            .call(
                "session.register",
                json!({
                    "client_name": format!("ofa/{}", env!("CARGO_PKG_VERSION")),
                    "client_version": env!("CARGO_PKG_VERSION"),
                    "capabilities": [],
                    "trace": false,
                }),
            )
            .await
            .context("session.register")?;
        Ok(client)
    }

    /// Issue one JSON-RPC call and wait for its matched response. Drops
    /// notifications and unrelated-id responses.
    pub(crate) async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut wire = serde_json::to_string(&req).context("encode request")?;
        wire.push('\n');
        self.stream
            .get_mut()
            .write_all(wire.as_bytes())
            .await
            .with_context(|| format!("writing {method} request"))?;

        loop {
            let mut buf = String::new();
            let n = timeout(DEFAULT_TIMEOUT, self.stream.read_line(&mut buf))
                .await
                .map_err(|_| anyhow!("broker reply timed out for {method}"))?
                .with_context(|| format!("reading reply for {method}"))?;
            if n == 0 {
                bail!("broker closed connection during {method}");
            }
            let m: Value = serde_json::from_str(buf.trim_end())
                .with_context(|| format!("parsing reply: {buf}"))?;
            // ignore notifications (no id) and mismatched-id responses
            let reply_id = m.get("id").and_then(Value::as_u64);
            if reply_id != Some(id) {
                continue;
            }
            if let Some(err) = m.get("error") {
                bail!("{method} returned error: {err}");
            }
            return Ok(m.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}
