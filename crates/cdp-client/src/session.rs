//! Per-session CDP API.
//!
//! Owned by `cdp-client`. A [`CdpSession`] is the user-facing handle for one
//! CDP session — either the root browser session (sessionId=`""`) or an
//! attached target session.
//!
//! # Threading
//!
//! Each session owns:
//! - a `tokio::sync::mpsc::Sender<Outbound>` cloned from the connection's
//!   writer queue,
//! - a `tokio::sync::broadcast::Sender<CdpEvent>` for inbound events,
//! - a shared `DashMap<u64, oneshot::Sender<Result<Value>>>` for in-flight
//!   command replies (keyed by JSON-RPC `id`).
//!
//! `send::<C>(params)` allocates a fresh `id`, parks a oneshot, writes the
//! envelope, and awaits the reply. The connection's reader actor demuxes
//! inbound frames by `(sessionId, id|method)` and resolves the right oneshot
//! or fans out to the broadcast channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::error::{CdpError, Result};
use crate::generated::CdpEvent;
use crate::metrics::{MetricsSink, Outcome};
use crate::retry::{is_transient, RetryPolicy};
use crate::Command;

/// CDP session id. The root browser session uses an empty string per
/// SPEC D5 / Puppeteer convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    /// The root browser session.
    pub fn root() -> Self {
        Self(String::new())
    }
    /// True if this is the root session (`""`).
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S: Into<String>> From<S> for SessionId {
    fn from(s: S) -> Self {
        Self(s.into())
    }
}

/// One outbound envelope queued for the writer task.
#[derive(Debug)]
pub(crate) struct Outbound {
    pub(crate) frame: Value,
}

/// Reply rendezvous shared between sessions and the connection reader.
pub(crate) type Pending = Arc<DashMap<u64, oneshot::Sender<Result<Value>>>>;

/// Default timeout for `send`: no timeout. Callers wrap in `tokio::time::timeout`
/// when they need one (the engine layer always does).
const DEFAULT_NO_TIMEOUT: Option<Duration> = None;

/// Inner shared state of a session.
pub(crate) struct SessionInner {
    pub(crate) id: SessionId,
    pub(crate) writer: mpsc::Sender<Outbound>,
    pub(crate) pending: Pending,
    pub(crate) next_id: AtomicU64,
    pub(crate) events_tx: broadcast::Sender<CdpEvent>,
    /// Optional metrics sink. Hot-swappable so consumers can attach
    /// observability after the session is built (e.g. once the broker
    /// finishes wiring its global histogram registry). `RwLock` so the
    /// hot read path (every `send`) is uncontested across cores.
    pub(crate) metrics: RwLock<Option<Arc<dyn MetricsSink>>>,
}

/// Public handle for one CDP session.
#[derive(Clone)]
pub struct CdpSession {
    inner: Arc<SessionInner>,
}

impl CdpSession {
    pub(crate) fn new(
        id: SessionId,
        writer: mpsc::Sender<Outbound>,
        pending: Pending,
        events_capacity: usize,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(events_capacity);
        Self {
            inner: Arc::new(SessionInner {
                id,
                writer,
                pending,
                next_id: AtomicU64::new(1),
                events_tx,
                metrics: RwLock::new(None),
            }),
        }
    }

    /// The session id (`""` for root browser).
    pub fn id(&self) -> &SessionId {
        &self.inner.id
    }

    /// Subscribe to inbound events on this session.
    pub fn events(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events_tx.subscribe()
    }

    pub(crate) fn events_tx(&self) -> &broadcast::Sender<CdpEvent> {
        &self.inner.events_tx
    }

    /// Attach (or replace) the metrics sink. Pass `None` to detach.
    ///
    /// Cheap to call repeatedly: stores an `Arc<dyn MetricsSink>` behind a
    /// reader-writer lock. The same sink is shared across every clone of
    /// this session because `Arc<SessionInner>`.
    pub fn with_metrics_sink(&self, sink: Option<Arc<dyn MetricsSink>>) {
        *self.inner.metrics.write() = sink;
    }

    /// Currently-attached metrics sink, if any.
    pub fn metrics_sink(&self) -> Option<Arc<dyn MetricsSink>> {
        self.inner.metrics.read().clone()
    }

    /// Send a typed command and await its typed reply.
    pub async fn send<C>(&self, params: C) -> Result<C::Returns>
    where
        C: Command,
    {
        self.send_with_timeout(params, DEFAULT_NO_TIMEOUT).await
    }

    /// Send a typed command with an optional timeout.
    ///
    /// Records a single metrics datapoint through the attached sink (if any)
    /// covering the entire wall-clock time from queuing the outbound frame
    /// to receiving the reply (or surfacing the transport error).
    pub async fn send_with_timeout<C>(
        &self,
        params: C,
        timeout: Option<Duration>,
    ) -> Result<C::Returns>
    where
        C: Command,
    {
        let started = Instant::now();
        let result = self.send_inner::<C>(params, timeout).await;
        if let Some(sink) = self.metrics_sink() {
            sink.record_call(
                C::METHOD,
                started.elapsed(),
                Self::outcome_for_result(&result),
                1,
            );
        }
        result
    }

    /// Send with the codegen-default retry policy when the command is
    /// declared idempotent ([`Command::IDEMPOTENT`] is `true`); falls
    /// through to a single-shot send otherwise.
    ///
    /// Use this for read-only commands where transient pipe flap shouldn't
    /// surface to the caller. For commands you know are safe to retry but
    /// the codegen flagged as side-effecting, use
    /// [`send_with_retry_policy`] with an explicit [`RetryPolicy`].
    ///
    /// [`send_with_retry_policy`]: Self::send_with_retry_policy
    /// [`Command::IDEMPOTENT`]: crate::Command::IDEMPOTENT
    pub async fn send_with_retry<C>(&self, params: C) -> Result<C::Returns>
    where
        C: Command + Clone,
    {
        let policy = if C::IDEMPOTENT {
            RetryPolicy::default_idempotent()
        } else {
            RetryPolicy::disabled()
        };
        self.send_with_retry_policy(params, policy).await
    }

    /// Send with an explicit retry policy. Retries on transient transport
    /// errors only ([`CdpError::ConnectionClosed`], [`CdpError::Timeout`],
    /// [`CdpError::SessionDetached`]); protocol errors are surfaced
    /// immediately because retrying produces the same error.
    ///
    /// Records one metrics datapoint covering the full retry sequence —
    /// `attempts` reflects the actual count.
    pub async fn send_with_retry_policy<C>(
        &self,
        params: C,
        policy: RetryPolicy,
    ) -> Result<C::Returns>
    where
        C: Command + Clone,
    {
        let started = Instant::now();
        let mut attempt: u32 = 0;
        let mut last_err: Option<CdpError> = None;

        while attempt < policy.max_attempts {
            attempt += 1;
            if attempt > 1 {
                tokio::time::sleep(policy.backoff_before(attempt)).await;
            }
            match self
                .send_inner::<C>(params.clone(), DEFAULT_NO_TIMEOUT)
                .await
            {
                Ok(v) => {
                    if let Some(sink) = self.metrics_sink() {
                        sink.record_call(C::METHOD, started.elapsed(), Outcome::Ok, attempt);
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if !is_transient(&e) {
                        if let Some(sink) = self.metrics_sink() {
                            let outcome = match &e {
                                CdpError::ProtocolError { .. } => Outcome::ProtocolError,
                                _ => Outcome::Internal,
                            };
                            sink.record_call(C::METHOD, started.elapsed(), outcome, attempt);
                        }
                        return Err(e);
                    }
                    tracing::debug!(
                        method = C::METHOD,
                        attempt,
                        error = %e,
                        "transient cdp error; retrying"
                    );
                    last_err = Some(e);
                }
            }
        }

        let err = last_err.unwrap_or(CdpError::ConnectionClosed);
        if let Some(sink) = self.metrics_sink() {
            sink.record_call(C::METHOD, started.elapsed(), Outcome::Transport, attempt);
        }
        Err(err)
    }

    /// Inner send with no metrics instrumentation. Shared by both the
    /// metrics-emitting wrappers (`send_with_timeout`,
    /// `send_with_retry_policy`) so each path emits exactly one datapoint
    /// per call.
    async fn send_inner<C>(&self, params: C, timeout: Option<Duration>) -> Result<C::Returns>
    where
        C: Command,
    {
        let result_value = self.send_inner_value(C::METHOD, params, timeout).await?;
        let typed: C::Returns = serde_json::from_value(result_value)?;
        Ok(typed)
    }

    async fn send_inner_value<P>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
    ) -> Result<Value>
    where
        P: serde::Serialize,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if !self.inner.id.is_root() {
            frame["sessionId"] = Value::String(self.inner.id.0.clone());
        }
        let (tx, rx) = oneshot::channel();
        self.inner.pending.insert(id, tx);

        if self.inner.writer.send(Outbound { frame }).await.is_err() {
            self.inner.pending.remove(&id);
            return Err(CdpError::ConnectionClosed);
        }

        match timeout {
            Some(d) => match tokio::time::timeout(d, rx).await {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => {
                    self.inner.pending.remove(&id);
                    Err(CdpError::ConnectionClosed)
                }
                Err(_) => {
                    self.inner.pending.remove(&id);
                    Err(CdpError::Timeout)
                }
            },
            None => match rx.await {
                Ok(v) => v,
                Err(_) => {
                    self.inner.pending.remove(&id);
                    Err(CdpError::ConnectionClosed)
                }
            },
        }
    }

    fn outcome_for_result<T>(result: &Result<T>) -> Outcome {
        match result {
            Ok(_) => Outcome::Ok,
            Err(e)
                if matches!(
                    e,
                    CdpError::ConnectionClosed | CdpError::Timeout | CdpError::SessionDetached
                ) =>
            {
                Outcome::Transport
            }
            Err(CdpError::ProtocolError { .. }) => Outcome::ProtocolError,
            Err(_) => Outcome::Internal,
        }
    }

    /// Send an untyped command (escape hatch — useful when the typed
    /// bindings haven't been generated for an experimental method, or when
    /// callers need to forward through opaque payloads).
    pub async fn send_raw(&self, method: &str, params: Value) -> Result<Value> {
        let started = Instant::now();
        let result = self
            .send_inner_value(method, params, DEFAULT_NO_TIMEOUT)
            .await;
        if let Some(sink) = self.metrics_sink() {
            sink.record_dynamic_call(
                method,
                started.elapsed(),
                Self::outcome_for_result(&result),
                1,
            );
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct CountingSink {
        calls: AtomicU64,
        last_dynamic_method: parking_lot::Mutex<Option<String>>,
        last_outcome: parking_lot::Mutex<Option<Outcome>>,
    }

    impl CountingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU64::new(0),
                last_dynamic_method: parking_lot::Mutex::new(None),
                last_outcome: parking_lot::Mutex::new(None),
            })
        }
    }

    impl MetricsSink for CountingSink {
        fn record_call(
            &self,
            _method: &'static str,
            _latency: Duration,
            outcome: Outcome,
            _attempts: u32,
        ) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last_dynamic_method.lock() = None;
            *self.last_outcome.lock() = Some(outcome);
        }

        fn record_dynamic_call(
            &self,
            method: &str,
            _latency: Duration,
            outcome: Outcome,
            _attempts: u32,
        ) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last_dynamic_method.lock() = Some(method.to_owned());
            *self.last_outcome.lock() = Some(outcome);
        }
    }

    #[tokio::test]
    async fn session_id_root_helpers() {
        let s = SessionId::root();
        assert!(s.is_root());
        let other: SessionId = "abc".into();
        assert!(!other.is_root());
        assert_eq!(other.as_str(), "abc");
    }

    #[tokio::test]
    async fn send_returns_connection_closed_when_writer_dropped() {
        let (writer_tx, writer_rx) = mpsc::channel::<Outbound>(8);
        let pending: Pending = Arc::new(DashMap::new());
        let session = CdpSession::new(SessionId::root(), writer_tx, pending, 16);

        // Drop the writer end so any send() fails.
        drop(writer_rx);

        let res = session.send_raw("Browser.getVersion", Value::Null).await;
        assert!(matches!(res, Err(CdpError::ConnectionClosed)));
    }

    #[tokio::test]
    async fn send_raw_records_dynamic_metrics() {
        let (writer_tx, mut writer_rx) = mpsc::channel::<Outbound>(8);
        let pending: Pending = Arc::new(DashMap::new());
        let session = CdpSession::new(SessionId::root(), writer_tx, pending.clone(), 16);
        let sink = CountingSink::new();
        session.with_metrics_sink(Some(Arc::clone(&sink) as Arc<dyn MetricsSink>));

        let responder = tokio::spawn(async move {
            let outbound = writer_rx.recv().await.expect("outbound frame queued");
            let id = outbound
                .frame
                .get("id")
                .and_then(Value::as_u64)
                .expect("id present");
            let (_, tx) = pending.remove(&id).expect("pending entry present");
            tx.send(Ok(serde_json::json!({"ok": true})))
                .expect("reply delivered");
        });

        let res = session
            .send_raw("Runtime.evaluate", serde_json::json!({"expression": "1+1"}))
            .await;
        assert_eq!(res.unwrap()["ok"], true);
        responder.await.expect("responder task");
        assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            *sink.last_dynamic_method.lock(),
            Some("Runtime.evaluate".to_string())
        );
        assert_eq!(*sink.last_outcome.lock(), Some(Outcome::Ok));
    }
}
