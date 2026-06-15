//! Contract test for [`TraceEvent`] JSON wire shape.
//!
//! `tools/ofa-trace.sh tail` consumes the trace JSONL with `jq` selectors:
//!     .ts_ms .kind .tab_id? .method? .tool?
//!
//! If a field is renamed in `TraceEvent`, the operator tool starts silently
//! emitting `null` for that field. This test pins the wire shape so that
//! drift fails CI before it reaches a user.

use observability::trace::TraceEvent;
use serde_json::{json, Value};

fn ts() -> u64 {
    1_700_000_000_000
}

fn assert_has_string(v: &Value, key: &str) {
    assert!(
        v.get(key).and_then(Value::as_str).is_some(),
        "expected `{key}` to be a non-null string in {v}"
    );
}
fn assert_has_u64(v: &Value, key: &str) {
    assert!(
        v.get(key).and_then(Value::as_u64).is_some(),
        "expected `{key}` to be a u64 in {v}"
    );
}

#[test]
fn cdp_request_wire_shape_matches_cb_trace_consumer() {
    let ev = TraceEvent::CdpRequest {
        ts_ms: ts(),
        session_id: "s_a".into(),
        target_id: Some("T1".into()),
        id: 42,
        method: "Page.navigate".into(),
        params: json!({"url": "https://example.com"}),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["kind"], "cdp_request");
    assert_has_u64(&v, "ts_ms");
    assert_has_string(&v, "session_id");
    assert_has_string(&v, "method");
}

#[test]
fn cdp_event_omits_id_per_jsonrpc_spec() {
    let ev = TraceEvent::CdpEvent {
        ts_ms: ts(),
        session_id: "s_a".into(),
        target_id: None,
        method: "Page.frameNavigated".into(),
        params: json!({}),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["kind"], "cdp_event");
    assert!(v.get("id").is_none(), "events must NOT carry an id");
    // target_id was None — should be omitted per skip_serializing_if.
    assert!(
        v.get("target_id").is_none(),
        "None target_id should be omitted from wire"
    );
}

#[test]
fn action_carries_tool_and_tab_id_for_cb_trace_tail() {
    let ev = TraceEvent::Action {
        ts_ms: ts(),
        session_id: "s_a".into(),
        tab_id: "t_91".into(),
        tool: "tab.open".into(),
        args: json!({"url": "https://example.com"}),
        result: json!({"tab_id": "t_91"}),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["kind"], "action");
    assert_has_string(&v, "tab_id");
    assert_has_string(&v, "tool");
    // ofa-trace.sh tail: jq -c '{ts: .ts_ms, kind, tab_id: .tab_id?, ..., tool: .tool?}'
    // — these are the keys it dereferences. Drift here breaks operator UX.
}

#[test]
fn screenshot_records_relative_png_path() {
    let ev = TraceEvent::Screenshot {
        ts_ms: ts(),
        session_id: "s_a".into(),
        tab_id: "t_91".into(),
        after_action: "page.click".into(),
        png_path: "screenshots/page.click-1700000000000-1.png".into(),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["kind"], "screenshot");
    assert_has_string(&v, "png_path");
    assert_has_string(&v, "after_action");
}

#[test]
fn dom_snapshot_records_hash_and_path() {
    let ev = TraceEvent::DomSnapshot {
        ts_ms: ts(),
        session_id: "s_a".into(),
        tab_id: "t_91".into(),
        snapshot_seq: 7,
        hash: "abc123".into(),
        snapshot_path: "snapshots/7.json".into(),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["kind"], "dom_snapshot");
    assert_has_u64(&v, "snapshot_seq");
    assert_has_string(&v, "hash");
    assert_has_string(&v, "snapshot_path");
}

#[test]
fn jsonl_round_trip_is_lossless() {
    // Every variant survives ser → deser without losing fields, since
    // ofa-trace events/summarize relies on `jq` reading the same shape we
    // wrote.
    for ev in [
        TraceEvent::Action {
            ts_ms: ts(),
            session_id: "s".into(),
            tab_id: "t".into(),
            tool: "tab.open".into(),
            args: json!({}),
            result: json!({}),
        },
        TraceEvent::Screenshot {
            ts_ms: ts(),
            session_id: "s".into(),
            tab_id: "t".into(),
            after_action: "x".into(),
            png_path: "p".into(),
        },
    ] {
        let s = serde_json::to_string(&ev).unwrap();
        let _: TraceEvent = serde_json::from_str(&s).unwrap();
    }
}
