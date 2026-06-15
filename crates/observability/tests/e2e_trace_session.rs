//! SPEC §10 M10 — end-to-end-ish trace session test.
//!
//! Drives the public [`observability::trace`] surface the way the broker +
//! browser-engine drive it for a single traced session: simulate a 5-action
//! agent run (5 actions + 5 screenshots + 1 DOM snapshot + a handful of
//! CDP request/response pairs), then "kill" the writer and assert the
//! on-disk transcript matches the requirement (`<seq>.jsonl` exists, action
//! count = 5, screenshot count = 5, ≥1 dom_snapshot, screenshots/ has 5
//! PNGs, snapshots/ has 1 JSON).
//!
//! This intentionally does NOT spin up a live Chromium — it exercises the
//! full record path through a stubbed action loop, which is what the M10
//! audit-trail-correctness story actually requires.

use std::sync::Arc;
use std::time::Duration;

use observability::trace::{TraceEvent, TraceOptions, TraceSink, TraceWriter};
use serde_json::{json, Value};
use tempfile::TempDir;

#[tokio::test]
async fn traced_session_5_actions_yields_5_screenshots_and_dom_snapshot() {
    let tmp = TempDir::new().unwrap();
    let writer =
        Arc::new(TraceWriter::start_in_dir("s_e2e", tmp.path(), TraceOptions::default()).unwrap());

    // Erase the type so this matches what real callers (broker / browser-
    // engine) hold: an `Arc<dyn TraceSink>`.
    let sink: Arc<dyn TraceSink> = writer.clone();

    let session_id = "s_e2e".to_string();
    let tab_id = "t_e2e".to_string();

    // 5 fake actions. Each:
    //   1. emit CDP request
    //   2. emit CDP response
    //   3. emit Action
    //   4. capture screenshot via save_screenshot_png + record Screenshot
    let actions = [
        "tab.open",
        "page.click",
        "page.type",
        "page.click",
        "tab.navigate",
    ];
    for (i, tool) in actions.iter().enumerate() {
        let id = i as i64;
        sink.record(TraceEvent::CdpRequest {
            ts_ms: sink.now_ms(),
            session_id: session_id.clone(),
            target_id: Some("T".into()),
            id,
            method: "Page.dispatchSomething".into(),
            params: json!({"i": i}),
        });
        sink.record(TraceEvent::CdpResponse {
            ts_ms: sink.now_ms(),
            session_id: session_id.clone(),
            target_id: Some("T".into()),
            id,
            result: Some(json!({"ok": true})),
            error: None,
        });
        sink.record(TraceEvent::Action {
            ts_ms: sink.now_ms(),
            session_id: session_id.clone(),
            tab_id: tab_id.clone(),
            tool: (*tool).into(),
            args: json!({"i": i}),
            result: json!({"ok": true}),
        });

        // Fake PNG bytes — the writer doesn't crack them open, just writes.
        let png: Vec<u8> = {
            let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
            v.extend_from_slice(format!("fake-{i}").as_bytes());
            v
        };
        let png_path = sink.save_screenshot_png(tool, &png).unwrap();
        assert!(
            png_path.starts_with("screenshots/"),
            "screenshot path must be relative to trace dir: {png_path}"
        );
        sink.record(TraceEvent::Screenshot {
            ts_ms: sink.now_ms(),
            session_id: session_id.clone(),
            tab_id: tab_id.clone(),
            after_action: (*tool).into(),
            png_path,
        });
    }

    // One 500 ms-cadence DOM snapshot, persisted to disk + recorded.
    let snapshot_payload = json!({
        "documents": [{"baseURL": "https://example.com"}],
        "strings": ["body", "div", "span"],
    });
    let (snapshot_path, hash) = sink.save_snapshot_json(1, &snapshot_payload).unwrap();
    sink.record(TraceEvent::DomSnapshot {
        ts_ms: sink.now_ms(),
        session_id: session_id.clone(),
        tab_id: tab_id.clone(),
        snapshot_seq: 1,
        hash,
        snapshot_path,
    });

    // Wait for the writer to drain, then "kill".
    assert!(writer.flush_for_test(Duration::from_secs(5)).await);
    writer.shutdown().await.unwrap();

    // ------ asserts ------

    // (a) trace file exists at <seq>.jsonl.
    let trace_jsonl = tmp.path().join("0000.jsonl");
    let bytes = std::fs::read(&trace_jsonl).expect("0000.jsonl must exist");
    let content = String::from_utf8(bytes).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // 5 cdp_request + 5 cdp_response + 5 action + 5 screenshot + 1 dom_snapshot = 21
    assert_eq!(
        lines.len(),
        21,
        "expected 21 records (5 cdp_req + 5 cdp_resp + 5 action + 5 screenshot + 1 dom_snapshot), got {}",
        lines.len()
    );

    // (b) every line is valid JSON, kind is one of our expected variants.
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for line in &lines {
        let v: Value = serde_json::from_str(line).expect("valid JSONL");
        let kind = v["kind"]
            .as_str()
            .expect("every record has kind")
            .to_owned();
        *counts.entry(kind).or_insert(0) += 1;
        assert!(v["ts_ms"].is_u64(), "every record has u64 ts_ms");
        assert!(v["session_id"].is_string(), "every record has session_id");
    }
    assert_eq!(counts.get("cdp_request").copied().unwrap_or(0), 5);
    assert_eq!(counts.get("cdp_response").copied().unwrap_or(0), 5);
    assert_eq!(counts.get("action").copied().unwrap_or(0), 5);
    assert_eq!(
        counts.get("screenshot").copied().unwrap_or(0),
        5,
        "5-action transcript must have exactly 5 screenshot records"
    );
    assert!(
        counts.get("dom_snapshot").copied().unwrap_or(0) >= 1,
        "expected ≥1 dom_snapshot record"
    );

    // (c) screenshots/ directory has exactly 5 PNG files.
    let screenshots_dir = tmp.path().join("screenshots");
    let pngs: Vec<_> = std::fs::read_dir(&screenshots_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "png").unwrap_or(false))
        .collect();
    assert_eq!(
        pngs.len(),
        5,
        "expected 5 PNG files on disk under screenshots/, got {}",
        pngs.len()
    );

    // (d) snapshots/ has at least one JSON file.
    let snapshots_dir = tmp.path().join("snapshots");
    let snap_files: Vec<_> = std::fs::read_dir(&snapshots_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    assert!(
        !snap_files.is_empty(),
        "expected ≥1 snapshot JSON file on disk"
    );

    // (e) post-shutdown the writer drops further records (no panics).
    sink.record(TraceEvent::Action {
        ts_ms: sink.now_ms(),
        session_id,
        tab_id,
        tool: "post.shutdown".into(),
        args: Value::Null,
        result: Value::Null,
    });
    // The dropped counter advances (or, alternatively, the writer is
    // already gone — both are acceptable post-shutdown semantics).
    let _ = sink.dropped_count();
}
