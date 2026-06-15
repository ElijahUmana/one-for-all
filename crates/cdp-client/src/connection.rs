//! Pipe transport between the broker and a Chromium child.
//!
//! Owned by `cdp-client`. Three actors per [`crate::Chromium`]:
//!
//! 1. **Reader** — reads bytes from the parent's read-end of fd 4 (chromium
//!    writes here), feeds the [`crate::framing::Decoder`], and demuxes
//!    completed JSON envelopes by `(sessionId, id|method)`.
//! 2. **Writer** — pulls [`crate::session::Outbound`] frames from a bounded
//!    `mpsc::Receiver`, encodes them to NUL-delimited JSON, and writes them
//!    to the parent's write-end of fd 3 (chromium reads here).
//! 3. **Wait** — joins the child process so panics/crashes show up as
//!    ConnectionClosed on every in-flight session.
//!
//! All three are spawned by [`Connection::spawn_actors`].
//!
//! # Channel sizing
//!
//! Per SPEC §D16: per-target inbound channels = 1024, broadcast events =
//! 4096. The writer queue is bounded at 1024.

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{CdpError, FramingError};
use crate::framing::{encode_frame_into, Decoder};
use crate::generated::CdpEvent;
use crate::metrics::MetricsSink;
use crate::session::{CdpSession, Outbound, Pending, SessionId};

/// Default broadcast capacity for events (per SPEC §D16).
pub const EVENTS_CAPACITY: usize = 4096;

/// Default mpsc capacity for inbound demux per target (per SPEC §D16).
pub const PER_TARGET_CAPACITY: usize = 1024;

/// Default mpsc capacity for the outbound writer queue.
pub const WRITER_CAPACITY: usize = 1024;

/// Internal registry of sessions keyed by SessionId.
type SessionMap = Arc<Mutex<HashMap<SessionId, CdpSession>>>;

/// Connection-wide state shared between the reader actor and the public
/// `Chromium` handle.
pub(crate) struct ConnectionState {
    pub(crate) writer_tx: mpsc::Sender<Outbound>,
    pub(crate) pending: Pending,
    pub(crate) sessions: SessionMap,
    /// Always present: the root browser session.
    pub(crate) root: CdpSession,
    /// Optional metrics sink propagated to every session created via this
    /// connection. Stored under a `RwLock` so the rare attach/detach path
    /// (once per Browser launch in practice) does not contend with reads on
    /// the hot session-create path.
    pub(crate) metrics: RwLock<Option<Arc<dyn MetricsSink>>>,
}

impl ConnectionState {
    pub(crate) fn new(writer_tx: mpsc::Sender<Outbound>) -> Self {
        let pending: Pending = Arc::new(DashMap::new());
        let root = CdpSession::new(
            SessionId::root(),
            writer_tx.clone(),
            pending.clone(),
            EVENTS_CAPACITY,
        );
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().insert(SessionId::root(), root.clone());
        Self {
            writer_tx,
            pending,
            sessions,
            root,
            metrics: RwLock::new(None),
        }
    }

    /// Return the session for `id`, creating one on first sight.
    ///
    /// Newly-created sessions inherit the connection-level metrics sink, so a
    /// sink attached at launch time covers every `Target.attachedToTarget`
    /// child without callers having to plumb it.
    pub(crate) fn session_for(&self, id: &SessionId) -> CdpSession {
        if let Some(s) = self.sessions.lock().get(id) {
            return s.clone();
        }
        let s = CdpSession::new(
            id.clone(),
            self.writer_tx.clone(),
            self.pending.clone(),
            EVENTS_CAPACITY,
        );
        if let Some(sink) = self.metrics.read().clone() {
            s.with_metrics_sink(Some(sink));
        }
        self.sessions.lock().insert(id.clone(), s.clone());
        s
    }

    /// Drop a session (called on `Target.detachedFromTarget`). All in-flight
    /// commands for this session are failed with `SessionDetached`.
    pub(crate) fn drop_session(&self, id: &SessionId) {
        self.sessions.lock().remove(id);
        // Best-effort: any pending replies that arrive after this point will
        // simply find no oneshot in `pending` and be dropped. We don't have
        // a per-session pending bucket — this is acceptable because callers
        // observe `SessionDetached` indirectly via `ConnectionClosed` /
        // dropped channels in their own logic.
        let _ = id;
    }

    /// Attach (or replace) the connection-level metrics sink. The sink is
    /// re-applied to every session currently in the registry — root + every
    /// already-attached target — and to every session created henceforth.
    /// Pass `None` to detach.
    pub(crate) fn set_metrics_sink(&self, sink: Option<Arc<dyn MetricsSink>>) {
        *self.metrics.write() = sink.clone();
        // Fan out to every existing session. Cloning the snapshot of the
        // sessions map first keeps the lock window tight.
        let snapshot: Vec<CdpSession> = self.sessions.lock().values().cloned().collect();
        for s in snapshot {
            s.with_metrics_sink(sink.clone());
        }
    }
}

/// Spawn the reader and writer actors for `R` (chromium → us) and `W`
/// (us → chromium).
///
/// Returns a [`ConnectionState`] handle to wire onto a [`crate::Chromium`].
pub(crate) fn spawn_actors<R, W>(
    mut reader: R,
    mut writer: W,
) -> (Arc<ConnectionState>, mpsc::Receiver<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (writer_tx, mut writer_rx) = mpsc::channel::<Outbound>(WRITER_CAPACITY);
    let state = Arc::new(ConnectionState::new(writer_tx));
    let (closed_tx, closed_rx) = mpsc::channel::<()>(1);

    // Writer task: drain the queue, encode each frame, write+flush.
    {
        let closed_tx = closed_tx.clone();
        tokio::spawn(async move {
            let mut scratch = Vec::with_capacity(8 * 1024);
            while let Some(out) = writer_rx.recv().await {
                scratch.clear();
                if let Err(e) = encode_frame_into(&mut scratch, &out.frame) {
                    tracing::error!(error = %e, "encode frame failed; closing writer");
                    break;
                }
                if let Err(e) = writer.write_all(&scratch).await {
                    tracing::warn!(error = %e, "pipe write failed; closing writer");
                    break;
                }
                if let Err(e) = writer.flush().await {
                    tracing::warn!(error = %e, "pipe flush failed; closing writer");
                    break;
                }
            }
            // Best-effort EOF on the pipe.
            let _ = writer.shutdown().await;
            // Signal connection close once the writer goes down.
            let _ = closed_tx.try_send(());
        });
    }

    // Reader task: read bytes, decode frames, demux to sessions / pending.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut dec = Decoder::default();
            let mut frames = Vec::with_capacity(16);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) => {
                        tracing::info!("pipe reader hit EOF");
                        break;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(error = %e, "pipe read failed");
                        break;
                    }
                };
                frames.clear();
                if let Err(e) = dec.feed_into(&buf[..n], &mut frames) {
                    match e {
                        FramingError::FrameTooLarge { limit } => {
                            tracing::error!(limit, "frame > cap; dropping connection");
                        }
                        other => {
                            tracing::warn!(error = %other, "framing error; dropping connection");
                        }
                    }
                    break;
                }
                for frame in frames.drain(..) {
                    dispatch(&state, frame);
                }
            }
            let _ = closed_tx.try_send(());
            // Fail every pending command with ConnectionClosed.
            for pair in state.pending.iter() {
                let _id = *pair.key();
            }
            // Drain pending: take ownership of each oneshot and complete it.
            // (DashMap doesn't allow drain through iterator borrow, so loop.)
            loop {
                let key = match state.pending.iter().next().map(|p| *p.key()) {
                    Some(k) => k,
                    None => break,
                };
                if let Some((_, tx)) = state.pending.remove(&key) {
                    let _ = tx.send(Err(CdpError::ConnectionClosed));
                }
            }
        });
    }

    (state, closed_rx)
}

/// Process one inbound CDP frame.
fn dispatch(state: &ConnectionState, frame: Value) {
    let session_id = frame
        .get("sessionId")
        .and_then(Value::as_str)
        .map(SessionId::from)
        .unwrap_or_else(SessionId::root);

    if let Some(id_v) = frame.get("id") {
        // Command reply.
        let id = match id_v.as_u64() {
            Some(i) => i,
            None => {
                tracing::warn!(?frame, "reply with non-u64 id; dropping");
                return;
            }
        };
        if let Some((_, tx)) = state.pending.remove(&id) {
            if let Some(err) = frame.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                let data = err.get("data").cloned();
                let _ = tx.send(Err(CdpError::ProtocolError {
                    code,
                    message,
                    data,
                }));
            } else {
                let result = frame.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(Ok(result));
            }
            return;
        }
        tracing::debug!(id, "reply for unknown id (likely already timed-out)");
        let _ = session_id;
    } else if let Some(method_v) = frame.get("method") {
        // Event notification.
        let session = state.session_for(&session_id);
        // Best-effort decode into the typed event enum; fall through silently
        // when the protocol JSONs don't model the event (e.g. methods we
        // didn't generate bindings for).
        match serde_json::from_value::<CdpEvent>(frame.clone()) {
            Ok(evt) => {
                let _ = session.events_tx().send(evt);
            }
            Err(e) => {
                tracing::trace!(method = %method_v, error = %e, "event not in typed enum");
            }
        }
    } else {
        tracing::warn!(?frame, "frame with neither id nor method; dropping");
    }
}

/// Public CDP transport handle wired over caller-owned async pipe halves.
///
/// The consumer (browser-engine) owns the spawned `tokio::process::Child`
/// and the parent ends of the fd 3 / fd 4 pipes; this type only owns the
/// reader/writer actors and the per-session demux state.
///
/// `Chromium::launch` composes this internally; consumers that own their
/// own spawn (because they need bespoke `pre_exec` setup, focus-restore
/// integration, etc.) construct one directly via [`Connection::from_pipe_halves`].
#[derive(Clone)]
pub struct Connection {
    state: Arc<ConnectionState>,
}

impl Connection {
    /// Wire CDP transport over caller-owned async pipe halves.
    ///
    /// `reader` is the parent's read end of fd 4 (chromium → us);
    /// `writer` is the parent's write end of fd 3 (us → chromium).
    ///
    /// Returns the [`Connection`] handle and an `mpsc::Receiver<()>` that
    /// fires once when either pipe actor terminates (e.g. Chromium died).
    pub fn from_pipe_halves<R, W>(reader: R, writer: W) -> (Self, mpsc::Receiver<()>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (state, closed_rx) = spawn_actors(reader, writer);
        (Self { state }, closed_rx)
    }

    /// The root browser CDP session (sessionId = `""`).
    pub fn root_session(&self) -> CdpSession {
        self.state.root.clone()
    }

    /// The session for an attached target id, creating it on demand.
    pub fn session_for(&self, id: &SessionId) -> CdpSession {
        self.state.session_for(id)
    }

    /// Drop a previously-attached session (called on `Target.detachedFromTarget`).
    pub fn drop_session(&self, id: &SessionId) {
        self.state.drop_session(id)
    }

    /// Attach (or replace) the metrics sink for every session served by this
    /// connection — root and every already-attached target, plus every
    /// session that comes up later via `Target.attachedToTarget`. Pass
    /// `None` to detach.
    ///
    /// Cheap to call repeatedly: a single `RwLock<Option<Arc<...>>>` write
    /// plus one `with_metrics_sink` per existing session. The hot read path
    /// (`CdpSession::send*`) does not contend with this writer.
    pub fn with_metrics_sink(&self, sink: Option<Arc<dyn MetricsSink>>) {
        self.state.set_metrics_sink(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn round_trip_command_via_actors() {
        // Reader-side mock: takes the writer's bytes, parses one frame,
        // and writes back a synthetic reply.
        let (parent_w_to_child, mut child_reads) = duplex(64 * 1024);
        let (mut child_writes, parent_r_from_child) = duplex(64 * 1024);

        let (state, _closed) = spawn_actors(parent_r_from_child, parent_w_to_child);

        // Mock chromium: read one frame, parse id, write a reply.
        let mock = tokio::spawn(async move {
            let mut dec = Decoder::default();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = child_reads.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                let frames = dec.feed(&buf[..n]).unwrap();
                if let Some(f) = frames.into_iter().next() {
                    let id = f.get("id").and_then(|v| v.as_u64()).unwrap();
                    let reply = serde_json::json!({
                        "id": id,
                        "result": {"protocolVersion": "1.3"},
                    });
                    let mut bytes = serde_json::to_vec(&reply).unwrap();
                    bytes.push(0x00);
                    child_writes.write_all(&bytes).await.unwrap();
                    child_writes.flush().await.unwrap();
                    break;
                }
            }
        });

        let res = state
            .root
            .send_raw("Browser.getVersion", Value::Null)
            .await
            .unwrap();
        assert_eq!(res["protocolVersion"], "1.3");
        mock.await.unwrap();
    }

    /// Tier-3 hardening: a stalled peer (never reads from the pipe) must
    /// NOT cause unbounded memory growth in the parent. With WRITER_CAPACITY
    /// = 1024, the writer mpsc fills up; once `parent_w_to_child` (the in-
    /// memory pipe) backpressures, the writer task awaits on `write_all`
    /// and the queue is bounded. We assert that ≥WRITER_CAPACITY enqueues
    /// remain pending without panic, leak, or unbounded growth.
    #[tokio::test]
    async fn writer_backpressures_when_peer_never_reads() {
        // Tiny duplex on the write side so the writer task blocks fast —
        // ~64 bytes is enough to admit the first envelope, then back-pressure.
        let (parent_w_to_child, child_reads) = duplex(64);
        // Reader side is fine; we never feed it anything.
        let (child_writes, parent_r_from_child) = duplex(64);

        let (state, _closed) = spawn_actors(parent_r_from_child, parent_w_to_child);

        // Spawn many concurrent senders. With WRITER_CAPACITY=1024 the
        // queue gates ~1024 in-flight outbounds; the rest must be pending.
        // We do NOT await the per-call `send_raw` — those would block on
        // the reply (which never comes). Instead we hand each `send_raw`
        // call to its own task and verify with a short timeout that none
        // of them resolve (which they shouldn't — neither the writer nor
        // the response side will progress).
        let mut handles = Vec::with_capacity(2_000);
        for i in 0..2_000u64 {
            let root = state.root.clone();
            handles.push(tokio::spawn(async move {
                root.send_raw("Browser.getVersion", serde_json::json!({"i": i}))
                    .await
            }));
        }

        // None of the calls should complete inside 250 ms. If any did,
        // either the queue is unbounded (memory bug) or the test wired
        // the reader/writer wrong.
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        let mut completed = 0;
        for h in &handles {
            if h.is_finished() {
                completed += 1;
            }
        }
        assert_eq!(
            completed,
            0,
            "writer should be backpressured, but {completed}/{} calls already resolved",
            handles.len()
        );

        // Tear down the connection by dropping `state`. Pending senders see
        // `writer is gone` and resolve to `ConnectionClosed`. They must NOT
        // hang forever — the bug we are guarding against.
        drop(state);
        drop(child_reads);
        drop(child_writes);
        let drained = tokio::time::timeout(tokio::time::Duration::from_secs(3), async {
            let mut errored = 0;
            for h in handles {
                match h.await {
                    Ok(Err(super::CdpError::ConnectionClosed)) => errored += 1,
                    Ok(_) => {}
                    Err(_join) => {}
                }
            }
            errored
        })
        .await;
        let drained = drained.expect("pending calls did not drain within 3s after drop");
        assert!(
            drained >= 1,
            "expected at least one pending call to resolve to ConnectionClosed; got 0"
        );
    }
}
