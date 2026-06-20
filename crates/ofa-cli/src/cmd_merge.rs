//! `ofa merge --session <ID> --paths X,Y [--strategy ours|theirs|prompt|three-way] [--apply] [--dry-run]`
//!
//! Front-door for `agent.merge_subagent_state`. The actual three-way TUI
//! and sandbox merge primitives are owned by sandbox-architect under
//! `crates/sandbox/src/merge.rs`; this CLI dispatches the broker call and
//! prints the result.

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::broker_rpc::CliBrokerClient;
use crate::{resolve_socket_path, strip_socket_flag};

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let socket = resolve_socket_path(argv)?;
    let argv = strip_socket_flag(argv);
    let session_id = arg_value(&argv, "--session")
        .ok_or_else(|| anyhow!("`ofa merge` requires --session <ID>"))?;
    let paths_csv =
        arg_value(&argv, "--paths").ok_or_else(|| anyhow!("`ofa merge` requires --paths <csv>"))?;
    let strategy = arg_value(&argv, "--strategy").unwrap_or_else(|| "prompt".into());
    let apply = argv.iter().any(|a| a == "--apply");
    let dry_run = argv.iter().any(|a| a == "--dry-run");

    let paths: Vec<&str> = paths_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if paths.is_empty() {
        anyhow::bail!("--paths must be a non-empty CSV");
    }
    if apply && dry_run {
        anyhow::bail!("--apply and --dry-run are mutually exclusive");
    }

    let mut client = CliBrokerClient::connect(&socket).await?;
    let result = client
        .call(
            "agent.merge_subagent_state",
            json!({
                "child_session_id": session_id,
                "paths": paths,
                "strategy": strategy,
                "apply": apply,
                "dry_run": dry_run,
            }),
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn arg_value(argv: &[String], flag: &str) -> Option<String> {
    let i = argv.iter().position(|a| a == flag)?;
    argv.get(i + 1).cloned()
}
