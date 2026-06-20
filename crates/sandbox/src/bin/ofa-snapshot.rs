//! `ofa-snapshot` — git-stash for browser sessions.
//!
//! Usage:
//!   ofa-snapshot take    --session <id> --name <snapshot>
//!   ofa-snapshot restore --name <snapshot> --into <new-session-id>
//!   ofa-snapshot list
//!   ofa-snapshot show    --name <snapshot>

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use sandbox::merge::session_rootfs;
use sandbox::snapshot::{
    read_snapshot_meta, restore_snapshot, snapshot_root, take_snapshot, SnapshotMeta,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        return print_usage(0);
    };
    match cmd.as_str() {
        "-h" | "--help" => print_usage(0),
        "take" => cmd_take(&args[1..]),
        "restore" => cmd_restore(&args[1..]),
        "list" => cmd_list(),
        "show" => cmd_show(&args[1..]),
        other => {
            eprintln!("error: unknown subcommand: {other}");
            print_usage(2)
        }
    }
}

fn cmd_take(args: &[String]) -> ExitCode {
    let mut session: Option<String> = None;
    let mut name: Option<String> = None;
    let mut it = args.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session = it.next(),
            "--name" => name = it.next(),
            other => return die(&format!("unknown arg: {other}")),
        }
    }
    let Some(session) = session else {
        return die("--session required");
    };
    let Some(name) = name else {
        return die("--name required");
    };

    let session_dir = match session_rootfs(&session) {
        Ok(p) => p,
        Err(e) => return die(&e.to_string()),
    };
    if !session_dir.exists() {
        return die(&format!(
            "session rootfs missing at {}",
            session_dir.display()
        ));
    }
    let snap_dir = match snapshot_root(&name) {
        Ok(p) => p,
        Err(e) => return die(&e.to_string()),
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut meta = SnapshotMeta {
        name: name.clone(),
        source_session_id: session.clone(),
        created_at_unix_ms: now_ms,
        bytes_apparent: 0,
        file_count: 0,
    };

    match take_snapshot(&session_dir, &snap_dir, &mut meta) {
        Ok(()) => {
            println!(
                "snapshot taken: {} ({} files, {} bytes)",
                name, meta.file_count, meta.bytes_apparent
            );
            ExitCode::from(0)
        }
        Err(e) => die(&format!("take_snapshot: {e}")),
    }
}

fn cmd_restore(args: &[String]) -> ExitCode {
    let mut name: Option<String> = None;
    let mut into: Option<String> = None;
    let mut it = args.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = it.next(),
            "--into" => into = it.next(),
            other => return die(&format!("unknown arg: {other}")),
        }
    }
    let Some(name) = name else {
        return die("--name required");
    };
    let Some(into) = into else {
        return die("--into required (new session id)");
    };

    let snap = match snapshot_root(&name) {
        Ok(p) => p,
        Err(e) => return die(&e.to_string()),
    };
    if !snap.exists() {
        return die(&format!("snapshot {name} not found at {}", snap.display()));
    }
    let target = match session_rootfs(&into) {
        Ok(p) => p,
        Err(e) => return die(&e.to_string()),
    };
    if target.exists() {
        return die(&format!(
            "target session {into} already exists at {} — pick a fresh id",
            target.display()
        ));
    }
    match restore_snapshot(&snap, &target) {
        Ok(()) => {
            println!(
                "restored {} -> session {} at {}",
                name,
                into,
                target.display()
            );
            ExitCode::from(0)
        }
        Err(e) => die(&format!("restore_snapshot: {e}")),
    }
}

fn cmd_list() -> ExitCode {
    let Some(home) = dirs::home_dir() else {
        return die("home dir unresolvable");
    };
    let snaps = home.join(".one-for-all/snapshots");
    if !snaps.exists() {
        println!("(no snapshots yet)");
        return ExitCode::from(0);
    }
    let mut names: Vec<PathBuf> = match std::fs::read_dir(&snaps) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(e) => return die(&format!("read_dir: {e}")),
    };
    names.sort();
    if names.is_empty() {
        println!("(no snapshots yet)");
        return ExitCode::from(0);
    }
    for p in names {
        if let Ok(Some(meta)) = read_snapshot_meta(&p) {
            println!(
                "{:<30} session={:<10} files={:<6} bytes={}",
                meta.name, meta.source_session_id, meta.file_count, meta.bytes_apparent
            );
        }
    }
    ExitCode::from(0)
}

fn cmd_show(args: &[String]) -> ExitCode {
    let mut name: Option<String> = None;
    let mut it = args.iter().cloned();
    while let Some(a) = it.next() {
        if a == "--name" {
            name = it.next();
        }
    }
    let Some(name) = name else {
        return die("--name required");
    };
    let snap = match snapshot_root(&name) {
        Ok(p) => p,
        Err(e) => return die(&e.to_string()),
    };
    match read_snapshot_meta(&snap) {
        Ok(Some(m)) => {
            println!("{}", serde_json::to_string_pretty(&m).unwrap_or_default());
            ExitCode::from(0)
        }
        Ok(None) => die(&format!("snapshot {name} has no meta.json")),
        Err(e) => die(&e.to_string()),
    }
}

fn die(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(2)
}

fn print_usage(code: u8) -> ExitCode {
    eprintln!(
        "ofa-snapshot — git-stash for browser sessions\n\n\
         USAGE:\n  \
           ofa-snapshot take    --session <id> --name <snap>\n  \
           ofa-snapshot restore --name <snap> --into <new-session-id>\n  \
           ofa-snapshot list\n  \
           ofa-snapshot show    --name <snap>\n"
    );
    ExitCode::from(code)
}
