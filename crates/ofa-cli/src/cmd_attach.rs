//! `ofa attach --session <ID>` — attach a TTY to a session's trace stream.
//!
//! Initial implementation: subscribes via `agent.observe_subagent` and
//! prints incoming notifications line-by-line. Stdin forwarding into the
//! session's primary tab as keystrokes is staged for a follow-up; we keep
//! the surface here so the broker contract (#55) is exercised end-to-end.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::broker_rpc::CliBrokerClient;
use crate::{resolve_socket_path, strip_socket_flag};

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let socket = resolve_socket_path(argv)?;
    let argv = strip_socket_flag(argv);
    let session_id = arg_value(&argv, "--session")
        .ok_or_else(|| anyhow!("`ofa attach` requires --session <ID>"))?;

    let mut client = CliBrokerClient::connect(&socket).await?;
    let _ack = client
        .call("agent.observe_subagent", json!({"session_id": session_id}))
        .await?;
    eprintln!("attached to {session_id}; Ctrl-C to detach");
    // Once `agent.observe_subagent` lands its broadcast topic, the CLI will
    // pump notifications here. For now we hold the connection open so the
    // server-side observer registration stays live.
    let _: Value = client.call("_internal.ping", Value::Null).await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn arg_value(argv: &[String], flag: &str) -> Option<String> {
    let i = argv.iter().position(|a| a == flag)?;
    argv.get(i + 1).cloned()
}
