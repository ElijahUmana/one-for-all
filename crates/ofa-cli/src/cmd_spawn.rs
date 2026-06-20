//! `ofa spawn [--label X] [--inherit cookies,downloads,~/proj/foo] [-- cmd args...]`
//!
//! Per SPEC §11 V7. Sends `agent.spawn_subagent` to the broker, which is
//! responsible for allocating a session_id and per-session sandbox; the
//! CLI never touches Chromium or the sandbox primitives directly. When a
//! `--` separator is present the CLI then exec's the trailing command with
//! `OFA_SESSION_ID=<id>` injected into the environment, so the inner
//! command's MCP client (or any other broker-aware tool) auto-binds.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::broker_rpc::CliBrokerClient;
use crate::{resolve_socket_path, strip_socket_flag};

#[derive(Debug, Default)]
struct SpawnArgs {
    label: Option<String>,
    inherit: Vec<String>,
    profile: Option<String>,
    instructions: Option<String>,
    rest: Vec<String>,
}

fn parse(argv: &[String]) -> Result<SpawnArgs> {
    let mut out = SpawnArgs::default();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--" => {
                out.rest.extend_from_slice(&argv[i + 1..]);
                break;
            }
            "--label" => {
                out.label = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow!("--label requires a value"))?,
                );
                i += 2;
            }
            "--inherit" => {
                let v = argv
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--inherit requires a value"))?;
                out.inherit.extend(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
                i += 2;
            }
            "--profile" => {
                out.profile = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow!("--profile requires a value"))?,
                );
                i += 2;
            }
            "--instructions" => {
                out.instructions = Some(
                    argv.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow!("--instructions requires a value"))?,
                );
                i += 2;
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unknown flag for `ofa spawn`: {other}")
            }
            _ => {
                anyhow::bail!(
                    "`ofa spawn` takes flags then `-- <command>`; got positional `{}`",
                    argv[i]
                );
            }
        }
    }
    Ok(out)
}

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let socket = resolve_socket_path(argv)?;
    let argv = strip_socket_flag(argv);
    let args = parse(&argv)?;

    let mut params = serde_json::Map::new();
    if let Some(l) = &args.label {
        params.insert("label".into(), Value::String(l.clone()));
    }
    if !args.inherit.is_empty() {
        params.insert(
            "inherit".into(),
            Value::Array(
                args.inherit
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(p) = &args.profile {
        params.insert("profile".into(), Value::String(p.clone()));
    }
    if let Some(p) = &args.instructions {
        params.insert("instructions".into(), Value::String(p.clone()));
    }

    let mut client = CliBrokerClient::connect(&socket).await?;
    let result = client
        .call("agent.spawn_subagent", Value::Object(params))
        .await
        .context("agent.spawn_subagent failed")?;
    let session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("broker did not return session_id: {result}"))?
        .to_string();

    println!("{}", json!({"session_id": session_id, "label": args.label}));

    if !args.rest.is_empty() {
        // Exec the trailing command with the session id pinned in the env.
        // We replace the current process so the user sees the inner command's
        // exit code 1:1 (no extra wrapper layer).
        use std::os::unix::process::CommandExt;
        let (head, tail) = args
            .rest
            .split_first()
            .ok_or_else(|| anyhow!("--: empty trailing command"))?;
        let mut cmd = std::process::Command::new(head);
        cmd.args(tail);
        cmd.env("OFA_SESSION_ID", &session_id);
        let err = cmd.exec();
        // exec() only returns on failure
        anyhow::bail!("exec({head}) failed: {err}");
    }
    Ok(())
}
