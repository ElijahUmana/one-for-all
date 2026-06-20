//! `ofa` — one-for-all operator CLI per SPEC §11 V7.
//!
//! Subcommands:
//! - `ofa spawn`  — spawn a sub-agent session, optionally exec a command bound to it.
//! - `ofa list`   — list running sessions (label, pid, RSS, last activity, tab count).
//! - `ofa attach` — attach a TTY to a session's trace stream + forward stdin.
//! - `ofa merge`  — promote selected sub-agent state back to host.
//! - `ofa kill`   — graceful shutdown of one session (or `--all`).
//! - `ofa logs`   — tail session trace JSONL under `~/.one-for-all/sessions/<session_id>/trace/`.
//!
//! All subcommands speak NDJSON JSON-RPC against the broker socket
//! (`~/.one-for-all/broker.sock`) per SPEC §2. The CLI itself never touches
//! Chromium directly — it asks the broker.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

mod broker_rpc;
mod cmd_attach;
mod cmd_kill;
mod cmd_list;
mod cmd_logs;
mod cmd_merge;
mod cmd_spawn;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio current-thread runtime")
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ofa: {e:#}");
            return ExitCode::from(1);
        }
    };

    let result = runtime.block_on(dispatch(args));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ofa: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn dispatch(args: Vec<String>) -> Result<()> {
    let (head, tail) = args.split_first().ok_or_else(|| anyhow!("no subcommand"))?;
    match head.as_str() {
        "spawn" => cmd_spawn::run(tail).await,
        "list" | "ls" => cmd_list::run(tail).await,
        "attach" => cmd_attach::run(tail).await,
        "merge" => cmd_merge::run(tail).await,
        "kill" => cmd_kill::run(tail).await,
        "logs" => cmd_logs::run(tail).await,
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "--version" | "-V" => {
            println!("ofa {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown subcommand `{other}` — try `ofa help`"),
    }
}

fn print_usage() {
    let _ = writeln!(
        std::io::stderr(),
        "ofa {} — one-for-all operator CLI (SPEC §11 V7)\n\
         \n\
         USAGE:\n\
            ofa <SUBCOMMAND> [OPTIONS]\n\
         \n\
         SUBCOMMANDS:\n\
            spawn    Spawn a sub-agent session and optionally exec a command bound to it\n\
            list     List running sessions (alias: ls)\n\
            attach   Attach a TTY to a running session's trace stream\n\
            merge    Promote selected sub-agent state back to host\n\
            kill     Graceful shutdown (one session or --all)\n\
            logs     Tail per-session broker log files\n\
            help     Print this message\n\
         \n\
         GLOBAL OPTIONS:\n\
            --socket <path>   Override broker socket (default: ~/.one-for-all/broker.sock)\n\
            --version, -V     Print version and exit",
        env!("CARGO_PKG_VERSION")
    );
}

/// Resolve the broker socket path, honoring `OFA_SOCKET` env override and the
/// `--socket <path>` flag inside subcommand argv.
pub(crate) fn resolve_socket_path(argv: &[String]) -> Result<PathBuf> {
    if let Some(idx) = argv.iter().position(|a| a == "--socket") {
        let path = argv
            .get(idx + 1)
            .ok_or_else(|| anyhow!("--socket requires an argument"))?;
        return Ok(PathBuf::from(path));
    }
    if let Ok(p) = std::env::var("OFA_SOCKET") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
    Ok(home.join(".one-for-all").join("broker.sock"))
}

/// Strip `--socket <path>` (if present) from argv before further parsing,
/// so subcommand-specific arg parsers don't have to re-implement the lookup.
pub(crate) fn strip_socket_flag(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut skip_next = false;
    for (i, a) in argv.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--socket" {
            // skip both this and the next arg
            if i + 1 < argv.len() {
                skip_next = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}
