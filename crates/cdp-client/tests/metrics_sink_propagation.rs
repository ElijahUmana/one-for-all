//! Integration test: `Connection::with_metrics_sink` propagates a
//! `MetricsSink` to the root session and to every session created later
//! via `Target.attachedToTarget`. The test simulates a CDP peer over an
//! in-memory duplex pair so we can drive replies + events deterministically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cdp_client::framing::Decoder;
use cdp_client::generated::domains::browser as cdp_browser;
use cdp_client::{Connection, MetricsSink, Outcome, SessionId};
use parking_lot::Mutex;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

/// Shared sink that records every call so we can assert which session
/// emitted what.
#[derive(Default)]
struct CountingSink {
    calls: AtomicU64,
    last_method: Mutex<Option<&'static str>>,
    last_outcome: Mutex<Option<Outcome>>,
    last_attempts: AtomicU64,
}

impl MetricsSink for CountingSink {
    fn record_call(
        &self,
        method: &'static str,
        _latency: Duration,
        outcome: Outcome,
        attempts: u32,
    ) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        *self.last_method.lock() = Some(method);
        *self.last_outcome.lock() = Some(outcome);
        self.last_attempts
            .store(u64::from(attempts), Ordering::Relaxed);
    }
}

#[tokio::test]
async fn metrics_sink_records_root_session_calls() {
    // duplex pair: parent <-> mock chromium
    let (parent_w_to_child, mut child_reads) = duplex(64 * 1024);
    let (mut child_writes, parent_r_from_child) = duplex(64 * 1024);

    let (cdp, _closed) = Connection::from_pipe_halves(parent_r_from_child, parent_w_to_child);

    let sink = Arc::new(CountingSink::default());
    cdp.with_metrics_sink(Some(Arc::clone(&sink) as Arc<dyn MetricsSink>));

    // Mock chromium: read one frame, write a synthetic reply.
    let mock = tokio::spawn(async move {
        let mut dec = Decoder::default();
        let mut buf = vec![0u8; 16 * 1024];
        let n = child_reads.read(&mut buf).await.expect("read");
        let frames = dec.feed(&buf[..n]).expect("decode");
        let f = frames.into_iter().next().expect("got one frame");
        let id = f.get("id").and_then(|v| v.as_u64()).expect("id present");
        let reply = serde_json::json!({
            "id": id,
            "result": {"protocolVersion": "1.3", "product": "Chromium", "revision": "0", "userAgent": "test", "jsVersion": "0"},
        });
        let mut bytes = serde_json::to_vec(&reply).expect("encode");
        bytes.push(0x00);
        child_writes.write_all(&bytes).await.expect("write");
        child_writes.flush().await.expect("flush");
    });

    let res = cdp
        .root_session()
        .send(cdp_browser::GetVersionParams::default())
        .await;
    assert!(res.is_ok(), "send returned: {res:?}");
    mock.await.expect("mock task");

    assert_eq!(sink.calls.load(Ordering::Relaxed), 1, "sink saw one call");
    assert_eq!(*sink.last_method.lock(), Some("Browser.getVersion"));
    assert_eq!(*sink.last_outcome.lock(), Some(Outcome::Ok));
    assert_eq!(sink.last_attempts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn metrics_sink_propagates_to_child_sessions() {
    // duplex pair: parent <-> mock chromium. We won't drive a wire reply
    // here — we just want to confirm `session_for(...)` on a fresh session
    // id installs the connection-level sink.
    let (parent_w_to_child, _child_reads) = duplex(64 * 1024);
    let (_child_writes, parent_r_from_child) = duplex(64 * 1024);

    let (cdp, _closed) = Connection::from_pipe_halves(parent_r_from_child, parent_w_to_child);

    let sink = Arc::new(CountingSink::default());
    cdp.with_metrics_sink(Some(Arc::clone(&sink) as Arc<dyn MetricsSink>));

    // Spawn a child session (simulates Target.attachedToTarget arriving).
    let child = cdp.session_for(&SessionId::from("CHILD_1"));
    let child_sink = child.metrics_sink().expect("child session inherited sink");
    // Pointer-equality of the trait object would require a downcast; the
    // simpler check is to record one call via the child sink and confirm
    // it lands on the same `CountingSink` instance.
    child_sink.record_call("Test.fromChild", Duration::from_micros(1), Outcome::Ok, 1);
    assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
    assert_eq!(*sink.last_method.lock(), Some("Test.fromChild"));
}

#[tokio::test]
async fn metrics_sink_attach_after_session_created() {
    // Sink installed AFTER a child session already exists must reach
    // the existing session.
    let (parent_w_to_child, _child_reads) = duplex(64 * 1024);
    let (_child_writes, parent_r_from_child) = duplex(64 * 1024);

    let (cdp, _closed) = Connection::from_pipe_halves(parent_r_from_child, parent_w_to_child);

    let pre_existing = cdp.session_for(&SessionId::from("EARLY"));
    assert!(pre_existing.metrics_sink().is_none(), "no sink yet");

    let sink = Arc::new(CountingSink::default());
    cdp.with_metrics_sink(Some(Arc::clone(&sink) as Arc<dyn MetricsSink>));

    let installed = pre_existing
        .metrics_sink()
        .expect("sink fanned out to pre-existing session");
    installed.record_call("Test.late", Duration::from_micros(1), Outcome::Ok, 1);
    assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
    assert_eq!(*sink.last_method.lock(), Some("Test.late"));
}

#[tokio::test]
async fn detach_clears_sink_on_existing_sessions() {
    let (parent_w_to_child, _child_reads) = duplex(64 * 1024);
    let (_child_writes, parent_r_from_child) = duplex(64 * 1024);

    let (cdp, _closed) = Connection::from_pipe_halves(parent_r_from_child, parent_w_to_child);
    let sink = Arc::new(CountingSink::default());
    cdp.with_metrics_sink(Some(Arc::clone(&sink) as Arc<dyn MetricsSink>));
    let s = cdp.session_for(&SessionId::from("S"));
    assert!(s.metrics_sink().is_some());

    cdp.with_metrics_sink(None);
    assert!(
        s.metrics_sink().is_none(),
        "detach reaches existing sessions"
    );
}
