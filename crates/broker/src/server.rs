//! Unix-socket server: accept loop + per-connection JSON-RPC reader/writer.
//!
//! Per SPEC §2 framing: newline-delimited JSON, 16 MB cap per line. UTF-8.
//! No length prefix, no LSP headers — that framing is reserved for the
//! stdio MCP layer one hop away.
//!
//! ## SPEC §11 V5 — binary topic on the write side
//!
//! Outbound `event/notify` events whose topic is `vision.frame` are
//! serialized as length-prefixed bincode (`0x01 | u32 LE len | payload |
//! 0x0A`) instead of JSON; everything else stays JSON. The receive side
//! is JSON-only — the broker does not currently ingest binary frames
//! from clients (vision.frame flows broker → client). The
//! `protocol::decode_line` helper supports both directions for future
//! symmetry and is exercised by unit tests.
//!
//! The writer task holds a per-conn scratch `Vec<u8>` so encode is
//! zero-allocation after warm-up.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::events::ClientEvent;
use crate::lifecycle::SessionLifecycle;
use crate::protocol::{ErrorCode, JsonRpcRequest, JsonRpcResponse};
use crate::registry::SessionEntry;
use crate::router;
use crate::State;

const LINE_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Bind the unix socket at `socket_path` with mode 0600. Cleans up any
/// stale socket file the previous broker may have left behind.
pub fn bind_socket(socket_path: &Path) -> Result<UnixListener> {
    if socket_path.exists() {
        // Use the SYNC std connect to probe whether anything is bound.
        // tokio::net::UnixStream::connect is async; std's is blocking but
        // that's fine for a one-shot startup probe.
        match std::os::unix::net::UnixStream::connect(socket_path) {
            Ok(_) => {
                return Err(anyhow!(
                    "socket {} is already bound — another broker is running",
                    socket_path.display()
                ));
            }
            Err(_) => {
                debug!(path = %socket_path.display(), "removing stale socket");
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    chmod_0600(socket_path)?;
    Ok(listener)
}

#[cfg(unix)]
fn chmod_0600(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(p, perms).context("chmod 0600 socket")?;
    Ok(())
}

#[cfg(not(unix))]
fn chmod_0600(_p: &Path) -> Result<()> {
    Ok(())
}

pub async fn run(state: Arc<State>, listener: UnixListener) -> Result<()> {
    info!("broker accept loop started");
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(state, stream).await {
                debug!(error = %e, "conn handler exited with error");
            }
        });
    }
}

async fn handle_conn(state: Arc<State>, stream: UnixStream) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    // Per-conn writer task drains a bounded mpsc; producers are: the router
    // (returning a Reply for each request) and the event bus (Notify /
    // VisionFrame).
    //
    // SPEC §11 V5 — the task holds a per-conn scratch `Vec<u8>` so that
    // every encode after warm-up is zero-allocation. Replies/Notifies use
    // serde_json::to_writer-into-scratch; VisionFrame uses
    // `protocol::encode_vision_frame_into` (bincode + 0x01 magic prefix).
    let (out_tx, mut out_rx) = mpsc::channel::<ClientEvent>(256);
    let writer_task = tokio::spawn(async move {
        let mut scratch: Vec<u8> = Vec::with_capacity(8 * 1024);
        while let Some(ev) = out_rx.recv().await {
            scratch.clear();
            let encode_res: std::result::Result<(), String> = match &ev {
                ClientEvent::Reply(r) => serde_json::to_writer(&mut scratch, r)
                    .map(|()| {
                        scratch.push(b'\n');
                    })
                    .map_err(|e| e.to_string()),
                ClientEvent::Notify(n) => serde_json::to_writer(&mut scratch, n)
                    .map(|()| {
                        scratch.push(b'\n');
                    })
                    .map_err(|e| e.to_string()),
                ClientEvent::VisionFrame(vf) => {
                    crate::protocol::encode_vision_frame_into(&mut scratch, vf)
                        .map_err(|e| e.to_string())
                }
            };
            if let Err(e) = encode_res {
                warn!(error = %e, kind = ?std::mem::discriminant(&ev), "encode failed; dropping event");
                continue;
            }
            if let Err(e) = wr.write_all(&scratch).await {
                warn!(error = %e, "writer task: write failed");
                break;
            }
        }
    });

    let mut bound_session: Option<Arc<SessionEntry>> = None;
    let mut bound_lifecycle: Option<SessionLifecycle> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .context("read_line on broker socket")?;
        if read == 0 {
            // Client disconnected.
            debug!("client disconnected");
            break;
        }
        if line.len() > LINE_CAP_BYTES {
            send_parse_err(
                &out_tx,
                format!("line {} bytes exceeds 16 MB cap", line.len()),
            )
            .await;
            continue;
        }
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                send_parse_err(&out_tx, format!("parse error: {e}")).await;
                continue;
            }
        };

        // Route the request. If it's session.register, capture the assigned
        // session and bind it to this connection.
        let was_register = req.method == "session.register";
        let id_for_bind = req.id.clone();
        let resp = router::dispatch(state.clone(), bound_session.clone(), req).await;
        let Some(resp) = resp else { continue };

        if was_register {
            if let Some(result) = resp.result.as_ref() {
                if let Some(sid) = result.get("session_id").and_then(|v| v.as_str()) {
                    if let Some(entry) = state.registry.get(sid) {
                        let lifecycle = {
                            let mut slot = entry.lifecycle.lock();
                            if let Some(existing) = slot.as_ref() {
                                existing.clone()
                            } else {
                                let created = SessionLifecycle::spawn(
                                    Arc::clone(&state.registry),
                                    Arc::clone(&entry),
                                    state.idle,
                                );
                                *slot = Some(created.clone());
                                created
                            }
                        };
                        lifecycle.notify_connected().await;
                        entry.bind_conn(out_tx.clone());
                        bound_session = Some(entry);
                        bound_lifecycle = Some(lifecycle);
                    }
                }
            }
            let _ = id_for_bind;
        }

        if out_tx.send(ClientEvent::Reply(resp)).await.is_err() {
            warn!("writer queue closed; dropping conn");
            break;
        }
    }

    // Disconnect cleanup.
    if let Some(s) = &bound_session {
        s.unbind_conn();
    }
    if let Some(l) = bound_lifecycle.take() {
        l.notify_disconnected().await;
    }
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

async fn send_parse_err(out_tx: &mpsc::Sender<ClientEvent>, msg: String) {
    let resp = JsonRpcResponse::err(serde_json::Value::Null, ErrorCode::ParseError, msg, None);
    let _ = out_tx.send(ClientEvent::Reply(resp)).await;
}
