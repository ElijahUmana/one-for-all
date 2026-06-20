//! `ofa logs --session <ID> [--follow]` — tail session trace JSONL.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

use crate::strip_socket_flag;

pub(super) async fn run(argv: &[String]) -> Result<()> {
    let argv = strip_socket_flag(argv);
    let session_id = arg_value(&argv, "--session")
        .ok_or_else(|| anyhow!("`ofa logs` requires --session <ID>"))?;
    let follow = argv.iter().any(|a| a == "--follow" || a == "-f");

    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home dir"))?;
    let trace_dir: PathBuf = home
        .join(".one-for-all")
        .join("sessions")
        .join(&session_id)
        .join("trace");
    if !trace_dir.exists() {
        anyhow::bail!(
            "no trace dir for session {session_id}: {}",
            trace_dir.display()
        );
    }

    // Pick the most recently-modified live .jsonl trace segment in the dir.
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&trace_dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            latest = Some((mtime, entry.path()));
        }
    }
    let path = latest
        .ok_or_else(|| anyhow!("no .jsonl trace files in {}", trace_dir.display()))?
        .1;

    let mut file = tokio::fs::File::open(&path).await?;
    if follow {
        // Seek to end, then poll every 250ms for new bytes.
        file.seek(std::io::SeekFrom::End(0)).await?;
    }
    let mut reader = BufReader::new(file);
    let stdout = tokio::io::stdout();
    use tokio::io::AsyncWriteExt as _;
    let mut out = stdout;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            if !follow {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        out.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn arg_value(argv: &[String], flag: &str) -> Option<String> {
    let i = argv.iter().position(|a| a == flag)?;
    argv.get(i + 1).cloned()
}
