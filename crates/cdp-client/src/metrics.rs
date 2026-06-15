//! Per-CDP-call metrics hook.
//!
//! The CDP layer is a hot path — every snapshot, action, and event flows
//! through it. To produce p50/p95/p99 latencies and per-method counters
//! without making `cdp-client` depend on `observability` (or any specific
//! metrics crate), we expose a thin trait. Consumers — typically
//! `observability` — implement it on their side and inject an
//! `Arc<dyn MetricsSink>` into the [`CdpSession`] via
//! [`CdpSession::with_metrics_sink`].
//!
//! No-op default: when no sink is set, every call is one `Option::is_none`
//! check on the hot path. No allocations, no atomics, no broadcast.
//!
//! [`CdpSession`]: crate::CdpSession
//! [`CdpSession::with_metrics_sink`]: crate::CdpSession::with_metrics_sink

use std::time::Duration;

/// Outcome classification for a single CDP send.
///
/// Splits transport from protocol so consumers can chart "Chromium said no"
/// (`ProtocolError`) separately from "the pipe died" (`Transport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Successful round-trip — wire reply contained `result`.
    Ok,
    /// Wire reply contained `error` (CDP `-326xx` codes etc).
    ProtocolError,
    /// Transport died before the reply arrived — `ConnectionClosed`,
    /// `Timeout`, `SessionDetached`. These are the candidates for retry
    /// (see [`crate::retry`]).
    Transport,
    /// Internal error: deserialization mismatch, bug, etc.
    Internal,
}

/// Receiver for per-call CDP metrics. One method invocation yields one
/// [`record_call`] call — implementations should be cheap (a histogram
/// observe + a counter increment, nothing that blocks).
///
/// [`record_call`]: MetricsSink::record_call
pub trait MetricsSink: Send + Sync {
    /// Record one CDP send for a codegen-known method.
    ///
    /// - `method` — wire method name (e.g. `"Page.captureScreenshot"`).
    /// - `latency` — wall time from queuing the outbound frame to
    ///   resolving the oneshot (or to the transport error firing).
    /// - `outcome` — see [`Outcome`].
    /// - `attempts` — number of send attempts including retries. `1` for
    ///   the common case; `>1` only when the call went through
    ///   [`crate::CdpSession::send_with_retry`] and a transient error
    ///   forced a retry.
    fn record_call(&self, method: &'static str, latency: Duration, outcome: Outcome, attempts: u32);

    /// Record one CDP send for a dynamically-named method.
    ///
    /// Raw CDP callers (`send_raw`) pass borrowed method names here so the
    /// common typed fast path keeps the `&'static str` contract and stays
    /// allocation-free. Implementations may intern or copy the name on first
    /// sight; repeated calls for the same method must not require caller-side
    /// allocation.
    fn record_dynamic_call(
        &self,
        method: &str,
        latency: Duration,
        outcome: Outcome,
        attempts: u32,
    ) {
        let _ = (method, latency, outcome, attempts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Tiny sink used by the integration tests in `session.rs`.
    pub(crate) struct CountingSink {
        pub calls: AtomicU64,
        pub last_method: parking_lot::Mutex<Option<&'static str>>,
        pub last_dynamic_method: parking_lot::Mutex<Option<String>>,
        pub last_outcome: parking_lot::Mutex<Option<Outcome>>,
    }

    impl CountingSink {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU64::new(0),
                last_method: parking_lot::Mutex::new(None),
                last_dynamic_method: parking_lot::Mutex::new(None),
                last_outcome: parking_lot::Mutex::new(None),
            })
        }
    }

    impl MetricsSink for CountingSink {
        fn record_call(
            &self,
            method: &'static str,
            _latency: Duration,
            outcome: Outcome,
            _attempts: u32,
        ) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last_method.lock() = Some(method);
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
            *self.last_method.lock() = None;
            *self.last_dynamic_method.lock() = Some(method.to_owned());
            *self.last_outcome.lock() = Some(outcome);
        }
    }

    #[test]
    fn counting_sink_records() {
        let s = CountingSink::new();
        s.record_call("Page.navigate", Duration::from_millis(5), Outcome::Ok, 1);
        assert_eq!(s.calls.load(Ordering::Relaxed), 1);
        assert_eq!(*s.last_method.lock(), Some("Page.navigate"));
        assert_eq!(*s.last_outcome.lock(), Some(Outcome::Ok));
    }
}
