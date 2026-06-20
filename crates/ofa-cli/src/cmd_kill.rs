//! `ofa kill --session <ID> | --all` — graceful shutdown.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::broker_rpc::CliBrokerClient;
use crate::{resolve_socket_path, strip_socket_flag};

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let socket = resolve_socket_path(argv)?;
    let argv = strip_socket_flag(argv);
    let kill_all = argv.iter().any(|a| a == "--all");
    let session_id = arg_value(&argv, "--session");

    if kill_all == session_id.is_some() {
        anyhow::bail!("`ofa kill` requires exactly one of --session <ID> or --all");
    }

    let mut client = CliBrokerClient::connect(&socket).await?;

    if kill_all {
        // Pull the live session list, then unregister each in turn. We rely on
        // `_internal.status` because the broker doesn't (yet) expose a
        // server-driven kill-all.
        let status = client.call("_internal.status", Value::Null).await?;
        let sessions = status
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if sessions.is_empty() {
            println!("(no sessions to kill)");
            return Ok(());
        }
        for s in sessions {
            let sid = s
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("status row missing session_id"))?
                .to_string();
            // session.unregister implicitly targets the connection-bound
            // session, so we open one connection per kill. For 100+ sessions
            // this is fine; the bigger concern is the single-shot teardown
            // case for which `--all` exists.
            let mut c = CliBrokerClient::connect(&socket).await?;
            // The CLI's auto-register binds a fresh session — we don't actually
            // want to kill OUR session, so we ask the broker to kill `sid` via
            // `_internal.kill_session` if available; fall back to a noop.
            let res = c
                .call("_internal.kill_session", json!({"session_id": sid}))
                .await;
            match res {
                Ok(_) => println!("killed {sid}"),
                Err(e) => eprintln!("kill {sid}: {e}"),
            }
        }
        Ok(())
    } else {
        // Single-session kill. The connection-bound session is OUR fresh CLI
        // session, so we route via `_internal.kill_session`.
        let sid = session_id.ok_or_else(|| anyhow!("--session required"))?;
        let res = client
            .call("_internal.kill_session", json!({"session_id": sid}))
            .await?;
        println!("{}", serde_json::to_string(&res)?);
        Ok(())
    }
}

fn arg_value(argv: &[String], flag: &str) -> Option<String> {
    let i = argv.iter().position(|a| a == flag)?;
    argv.get(i + 1).cloned()
}
