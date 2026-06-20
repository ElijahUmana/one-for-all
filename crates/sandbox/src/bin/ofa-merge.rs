//! `ofa-merge` — CLI for promoting agent-side changes back to the host.
//!
//! Usage:
//!   ofa-merge --session <id> --paths <p1> [--paths <p2> ...] \
//!            [--strategy ours|theirs|prompt|three-way] [--apply]
//!
//! Default is dry-run. Pass `--apply` to actually mutate the host. We hand-
//! roll the parser to keep the dep graph minimal — no `clap` for the v1
//! binary.

use std::path::PathBuf;
use std::process::ExitCode;

use sandbox::merge::{ensure_rsync_present, session_rootfs, MergePlan, MergeStrategy};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return ExitCode::from(0);
    }

    let mut session_id: Option<String> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut strategy = MergeStrategy::ThreeWay;
    let mut dry_run = true;

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => match it.next() {
                Some(v) => session_id = Some(v),
                None => return die("--session needs a value"),
            },
            "--paths" => match it.next() {
                Some(v) => paths.push(PathBuf::from(v)),
                None => return die("--paths needs a value"),
            },
            "--strategy" => match it.next() {
                Some(v) => match v.as_str() {
                    "ours" => strategy = MergeStrategy::Ours,
                    "theirs" => strategy = MergeStrategy::Theirs,
                    "prompt" => strategy = MergeStrategy::Prompt,
                    "three-way" => strategy = MergeStrategy::ThreeWay,
                    other => return die(&format!("unknown strategy: {other}")),
                },
                None => return die("--strategy needs a value"),
            },
            "--apply" => dry_run = false,
            "--dry-run" => dry_run = true,
            other => return die(&format!("unknown arg: {other}")),
        }
    }

    let sid = match session_id {
        Some(s) => s,
        None => return die("--session is required"),
    };
    if paths.is_empty() {
        return die("at least one --paths is required");
    }

    if let Err(e) = ensure_rsync_present() {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }

    let rootfs = match session_rootfs(&sid) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if !rootfs.exists() {
        eprintln!("error: session rootfs missing at {}", rootfs.display());
        return ExitCode::from(2);
    }

    let plan = MergePlan {
        session_rootfs: rootfs,
        paths,
        strategy,
        dry_run,
    };

    println!(
        "ofa-merge: session={} strategy={} mode={}",
        sid,
        strategy.as_str(),
        if dry_run { "dry-run" } else { "apply" }
    );
    match plan.execute() {
        Ok(report) => {
            for line in &report.itemize_lines {
                println!("  {line}");
            }
            println!(
                "summary: considered={} copied={} unchanged={} conflicting={}",
                report.considered,
                report.copied,
                report.skipped_unchanged,
                report.skipped_conflicting,
            );
            if dry_run {
                println!("(dry-run; no files were modified — re-run with --apply to commit)");
            }
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn die(msg: &str) -> ExitCode {
    eprintln!("error: {msg}");
    print_usage();
    ExitCode::from(2)
}

fn print_usage() {
    eprintln!(
        "ofa-merge — promote agent-side changes back to the host\n\n\
         USAGE:\n  \
           ofa-merge --session <id> --paths <p1> [--paths <p2>] \\\n           \
                    [--strategy ours|theirs|prompt|three-way] [--apply]\n\n\
         FLAGS:\n  \
           --session <id>     SPEC §11 V3 session id\n  \
           --paths <path>     host path to promote (repeat for multiple)\n  \
           --strategy <name>  conflict strategy (default: three-way)\n  \
           --apply            mutate the host (default is --dry-run)\n  \
           --dry-run          explicit dry-run (the default)\n"
    );
}
