//! `ofa list` — render the session table from `_internal.status`.
//!
//! Perf goal (item #19): p99 < 100 ms at 100+ sessions. We pre-allocate a
//! single output buffer, write columns with `write!` (no per-row `format!`
//! allocations), and avoid `serde_json::to_string` on the whole result —
//! we walk the array once.

use std::fmt::Write as _;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::broker_rpc::CliBrokerClient;
use crate::{resolve_socket_path, strip_socket_flag};

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let socket = resolve_socket_path(argv)?;
    let argv = strip_socket_flag(argv);
    let json_out = argv.iter().any(|a| a == "--json");

    let mut client = CliBrokerClient::connect(&socket).await?;
    let result = client.call("_internal.status", Value::Null).await?;

    if json_out {
        // Stream-friendly: the broker already returns a JSON tree; just emit it.
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }

    let sessions = result
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("_internal.status missing `sessions`"))?;

    if sessions.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }

    // Single pre-sized output buffer — the perf hot path. 100 sessions × ~80
    // bytes/row ≈ 8 KiB; one allocation total.
    let mut buf = String::with_capacity(80 + sessions.len() * 96);
    writeln!(
        &mut buf,
        "{:<14} {:>5} {:>10} {:>14}  {}",
        "SESSION", "TABS", "ACT_AGO_MS", "CREATED_MS", "LABEL"
    )?;
    for s in sessions {
        let id = s.get("session_id").and_then(Value::as_str).unwrap_or("?");
        let tabs = s.get("tab_count").and_then(Value::as_u64).unwrap_or(0);
        let last = s
            .get("last_activity_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let created = s.get("created_at_ms").and_then(Value::as_u64).unwrap_or(0);
        let label = s.get("label").and_then(Value::as_str).unwrap_or("");
        writeln!(
            &mut buf,
            "{:<14} {:>5} {:>10} {:>14}  {}",
            id, tabs, last, created, label
        )?;
    }
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    use std::io::Write as _;
    h.write_all(buf.as_bytes())?;
    Ok(())
}
