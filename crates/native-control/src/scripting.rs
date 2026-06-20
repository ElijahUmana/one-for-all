//! SPEC §12 U6 — `app.shortcut.run`, `app.automator.run`,
//! `app.applescript`, `app.javascript_for_automation`,
//! `app.terminal.spawn_session`.
//!
//! All five wrap subprocesses Apple ships in `/usr/bin`:
//! - `shortcuts run --name <name> [--input <input>]`
//! - `automator -i <input> <workflow.workflow>`
//! - `osascript -e <source>`            ← AppleScript
//! - `osascript -l JavaScript -e <src>` ← JXA
//! - Terminal sessions are coordinated with `terminal-master` via PTY APIs;
//!   we provide a thin `spawn_session` that opens a Terminal.app window via
//!   AppleScript when terminal-master isn't running yet (best-effort).
//!
//! Every subprocess is timeout-bounded so a stuck script doesn't keep a
//! `tokio::task::spawn_blocking` worker thread tied up forever.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;

use crate::types::NativeControlError;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared subprocess wrapper: run `cmd` capturing stdout/stderr; return JSON
/// if stdout parses as JSON, otherwise a string. Errors propagate as
/// [`NativeControlError::AppleScript`].
async fn run_with_timeout(
    mut cmd: Command,
    label: &'static str,
) -> Result<Value, NativeControlError> {
    let task = tokio::task::spawn_blocking(move || cmd.output());
    let output = timeout(DEFAULT_TIMEOUT, task)
        .await
        .map_err(|_| NativeControlError::Timeout(label))?
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
        .map_err(|e| NativeControlError::Io(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(NativeControlError::AppleScript {
            msg: if stderr.is_empty() {
                format!("{label} exit {:?}", output.status.code())
            } else {
                stderr
            },
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Ok(v) = serde_json::from_str::<Value>(&stdout) {
        Ok(v)
    } else {
        Ok(Value::String(stdout))
    }
}

/// Run a Shortcuts.app shortcut by name.
pub async fn shortcut_run(name: &str, input: Option<&str>) -> Result<Value, NativeControlError> {
    let mut cmd = Command::new("/usr/bin/shortcuts");
    cmd.arg("run").arg("--name").arg(name);
    if let Some(i) = input {
        cmd.arg("--input").arg(i);
    }
    run_with_timeout(cmd, "shortcuts").await
}

/// Run an Automator workflow by file path.
pub async fn automator_run(workflow_path: &str) -> Result<Value, NativeControlError> {
    let mut cmd = Command::new("/usr/bin/automator");
    cmd.arg(workflow_path);
    run_with_timeout(cmd, "automator").await
}

/// Run a raw AppleScript snippet (no `tell application id …` wrapping). The
/// `validate_no_activate` invariant from `actions.rs` is reused — bodies
/// containing `activate` are rejected.
pub async fn applescript(source: &str) -> Result<Value, NativeControlError> {
    if source.to_lowercase().contains("activate") {
        return Err(NativeControlError::ActivateForbidden {
            reason: "AppleScript contains the word 'activate'",
        });
    }
    let mut cmd = Command::new("/usr/bin/osascript");
    cmd.arg("-e").arg(source);
    run_with_timeout(cmd, "osascript").await
}

/// Run a JavaScript-for-Automation (JXA) snippet.
pub async fn jxa(source: &str) -> Result<Value, NativeControlError> {
    // JXA also exposes `Application(...).activate()`; we apply the same gate.
    if source.contains(".activate(") {
        return Err(NativeControlError::ActivateForbidden {
            reason: "JXA snippet contains a .activate() call",
        });
    }
    let mut cmd = Command::new("/usr/bin/osascript");
    cmd.arg("-l").arg("JavaScript").arg("-e").arg(source);
    run_with_timeout(cmd, "osascript-jxa").await
}

/// Spawn a new Terminal.app session running `command`. Returns the JSON
/// result of the AppleScript invocation (Terminal returns a session id).
///
/// This is a best-effort fallback when `terminal-master` is not co-running.
/// Production deployments should prefer `term.spawn` from terminal-master,
/// which uses portable-pty and exposes `term.read`/`term.write`.
pub async fn terminal_spawn_session(
    command: &str,
    confirm: bool,
    focus_steal: bool,
) -> Result<Value, NativeControlError> {
    if !(confirm && focus_steal) {
        return Err(NativeControlError::FocusStealForbidden {
            what: "app.terminal.spawn_session",
        });
    }
    if command.contains("activate") {
        return Err(NativeControlError::ActivateForbidden {
            reason: "Terminal command contains 'activate'",
        });
    }
    // We open Terminal.app and run a `do script` — not an `activate`. The
    // Terminal becomes visible by virtue of macOS launching it; we don't
    // explicitly raise it. Caller can still flag this as focus-stealing
    // semantics at the broker layer if needed.
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("tell application \"Terminal\" to do script \"{escaped}\"");
    let mut cmd = Command::new("/usr/bin/osascript");
    cmd.arg("-e").arg(script);
    run_with_timeout(cmd, "terminal-spawn").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applescript_rejects_activate() {
        let r = applescript(r#"tell application "Calculator" to activate"#).await;
        match r {
            Err(NativeControlError::ActivateForbidden { .. }) => {}
            other => panic!("expected ActivateForbidden, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn jxa_rejects_dotactivate() {
        let r = jxa(r#"Application("Calculator").activate()"#).await;
        match r {
            Err(NativeControlError::ActivateForbidden { .. }) => {}
            other => panic!("expected ActivateForbidden, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_spawn_requires_focus_gate() {
        let r = terminal_spawn_session("echo hi", false, true).await;
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.terminal.spawn_session");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
        let r = terminal_spawn_session("echo hi", true, false).await;
        match r {
            Err(NativeControlError::FocusStealForbidden { what }) => {
                assert_eq!(what, "app.terminal.spawn_session");
            }
            other => panic!("expected FocusStealForbidden, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_spawn_rejects_activate() {
        let r = terminal_spawn_session("activate something", true, true).await;
        match r {
            Err(NativeControlError::ActivateForbidden { .. }) => {}
            other => panic!("expected ActivateForbidden, got {other:?}"),
        }
    }
}
