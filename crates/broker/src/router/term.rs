use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::protocol::ErrorCode;
use crate::registry::SessionEntry;
use crate::State;

use super::{required_str, RouterError, ToolResult};

const DEFAULT_TERM_ROWS: u16 = 24;
const DEFAULT_TERM_COLS: u16 = 80;
const DEFAULT_READ_BYTES: usize = 64 * 1024;

pub(super) async fn term_spawn(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let shell = required_str(&params, "shell")?;
    let cwd = optional_path(&params, "cwd");
    let env = parse_env_map(&params)?;
    let rows = optional_u16(&params, "rows")?.unwrap_or(DEFAULT_TERM_ROWS);
    let cols = optional_u16(&params, "cols")?.unwrap_or(DEFAULT_TERM_COLS);
    let sandbox = session.session_sandbox.read().clone();
    let result = session
        .terminal_controller
        .spawn_terminal(terminal_control::SpawnTerminalRequest {
            shell: shell.to_owned(),
            cwd,
            env,
            rows,
            cols,
            sandbox,
        })
        .await
        .map_err(map_term_err)?;
    serde_json::to_value(result)
        .map_err(|e| RouterError::internal(format!("term.spawn serialize: {e}")))
}

pub(super) async fn term_write(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let data = if let Some(text) = params.get("text").and_then(Value::as_str) {
        text.as_bytes().to_vec()
    } else if let Some(data_b64) = params.get("data_base64").and_then(Value::as_str) {
        base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| RouterError::invalid_params(format!("invalid data_base64: {e}")))?
    } else {
        return Err(RouterError::invalid_params("missing text or data_base64"));
    };
    let bytes_written = session
        .terminal_controller
        .write_bytes(session_id, &data)
        .map_err(map_term_err)?;
    Ok(json!({"bytes_written": bytes_written}))
}

pub(super) async fn term_read(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let max_bytes = params
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_READ_BYTES);
    let chunk = session
        .terminal_controller
        .read_output(session_id, max_bytes)
        .map_err(map_term_err)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&chunk.data);
    let text = String::from_utf8(chunk.data.clone())
        .ok()
        .map(|text| redact_text(session, &text));
    Ok(json!({
        "bytes": chunk.data.len(),
        "data_base64": encoded,
        "text": text,
        "eof": chunk.eof,
        "dropped_bytes": chunk.dropped_bytes,
    }))
}

pub(super) async fn term_snapshot(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let mut snapshot = session
        .terminal_controller
        .snapshot(session_id)
        .map_err(map_term_err)?;
    redact_snapshot(session, &mut snapshot);
    serde_json::to_value(snapshot)
        .map_err(|e| RouterError::internal(format!("term.snapshot serialize: {e}")))
}

pub(super) async fn term_resize(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let rows = optional_u16(&params, "rows")?
        .ok_or_else(|| RouterError::invalid_params("missing rows"))?;
    let cols = optional_u16(&params, "cols")?
        .ok_or_else(|| RouterError::invalid_params("missing cols"))?;
    session
        .terminal_controller
        .resize(session_id, rows, cols)
        .map_err(map_term_err)?;
    Ok(json!({"ok": true, "rows": rows, "cols": cols}))
}

pub(super) async fn term_close(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let exit = session
        .terminal_controller
        .close(session_id)
        .await
        .map_err(map_term_err)?;
    serde_json::to_value(exit)
        .map_err(|e| RouterError::internal(format!("term.close serialize: {e}")))
}

pub(super) async fn term_send_signal(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let signal = required_str(&params, "signal")?;
    session
        .terminal_controller
        .send_signal(session_id, signal)
        .map_err(map_term_err)?;
    Ok(json!({"ok": true, "signal": signal}))
}

pub(super) async fn term_scrollback(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let n_lines = params
        .get("n_lines")
        .and_then(Value::as_u64)
        .ok_or_else(|| RouterError::invalid_params("missing n_lines"))? as usize;
    let mut lines = session
        .terminal_controller
        .scrollback(session_id, n_lines)
        .map_err(map_term_err)?;
    for line in &mut lines {
        line.text = redact_text(session, &line.text);
    }
    Ok(json!({"lines": lines}))
}

pub(super) async fn term_alt_screen_active(
    session: &Arc<SessionEntry>,
    params: Value,
) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let mut state = session
        .terminal_controller
        .alt_screen_state(session_id)
        .map_err(map_term_err)?;
    if let Some(title) = state.window_title.take() {
        state.window_title = Some(redact_text(session, &title));
    }
    serde_json::to_value(state)
        .map_err(|e| RouterError::internal(format!("term.alt_screen_active serialize: {e}")))
}

pub(super) async fn term_mouse_event(session: &Arc<SessionEntry>, params: Value) -> ToolResult {
    let session_id = required_str(&params, "session_id")?;
    let row =
        optional_u16(&params, "row")?.ok_or_else(|| RouterError::invalid_params("missing row"))?;
    let col =
        optional_u16(&params, "col")?.ok_or_else(|| RouterError::invalid_params("missing col"))?;
    let kind = parse_mouse_kind(required_str(&params, "kind")?)?;
    let button = params
        .get("button")
        .and_then(Value::as_str)
        .map(parse_mouse_button)
        .transpose()?;
    let request = terminal_control::MouseEventRequest {
        row,
        col,
        kind,
        button,
        shift: params
            .get("shift")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        alt: params.get("alt").and_then(Value::as_bool).unwrap_or(false),
        ctrl: params.get("ctrl").and_then(Value::as_bool).unwrap_or(false),
    };
    session
        .terminal_controller
        .mouse_event(session_id, request)
        .map_err(map_term_err)?;
    Ok(json!({"ok": true}))
}

pub(super) fn install_terminal_notification_sink(state: &Arc<State>, entry: &Arc<SessionEntry>) {
    entry
        .terminal_controller
        .set_notification_sink(Some(Arc::new(BrokerTerminalSink {
            state: Arc::clone(state),
            entry: Arc::clone(entry),
        })));
}

struct BrokerTerminalSink {
    state: Arc<State>,
    entry: Arc<SessionEntry>,
}

impl terminal_control::NotificationSink for BrokerTerminalSink {
    fn notify(&self, event: terminal_control::TerminalEvent) {
        match event {
            terminal_control::TerminalEvent::Output(output) => {
                let text = String::from_utf8(output.data.clone())
                    .ok()
                    .map(|value| redact_text(&self.entry, &value));
                let payload = build_term_output_params(&self.entry.session_id, &output, text);
                self.trace_terminal_event(
                    "term.output",
                    &payload,
                    &json!({
                        "term_session_id": payload["payload"]["term_session_id"],
                        "seq": payload["payload"]["seq"],
                        "bytes": payload["payload"]["bytes"],
                    }),
                );
                let _ = self.entry.try_push(crate::ServerEvent {
                    jsonrpc: "2.0".into(),
                    method: "event/notify".into(),
                    params: payload,
                });
            }
            terminal_control::TerminalEvent::Exit(exit) => {
                let payload = build_term_exit_params(&self.entry.session_id, &exit);
                self.trace_terminal_event(
                    "term.exit",
                    &payload,
                    &json!({
                        "term_session_id": payload["payload"]["term_session_id"],
                    }),
                );
                let _ = self.entry.try_push(crate::ServerEvent {
                    jsonrpc: "2.0".into(),
                    method: "event/notify".into(),
                    params: payload,
                });
            }
        }
    }
}

fn build_term_output_params(
    session_id: &str,
    output: &terminal_control::TermOutputEvent,
    text: Option<String>,
) -> Value {
    json!({
        "topic": "term.output",
        "session_id": session_id,
        "payload": {
            "term_session_id": output.session_id,
            "seq": output.seq,
            "bytes": output.data.len(),
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&output.data),
            "text": text,
            "dropped_bytes": output.dropped_bytes,
            "eof": output.eof,
        }
    })
}

fn build_term_exit_params(session_id: &str, exit: &terminal_control::TermExitEvent) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "term_session_id".into(),
        Value::String(exit.session_id.clone()),
    );
    payload.insert("exited".into(), Value::Bool(exit.exit.exited));
    if let Some(code) = exit.exit.exit_code {
        payload.insert("exit_code".into(), json!(code));
    }
    if let Some(signal) = exit.exit.signal.clone() {
        payload.insert("signal".into(), Value::String(signal));
    }
    json!({
        "topic": "term.exit",
        "session_id": session_id,
        "payload": payload,
    })
}

impl BrokerTerminalSink {
    fn trace_terminal_event(&self, tool: &str, payload: &Value, args: &Value) {
        if !self
            .entry
            .trace_enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let Some(writer) = self.state.traces.get(&self.entry.session_id) else {
            return;
        };
        use observability::trace::{TraceEvent, TraceSink};
        writer.record(TraceEvent::Action {
            ts_ms: writer.now_ms(),
            session_id: self.entry.session_id.clone(),
            tab_id: String::new(),
            tool: tool.to_owned(),
            args: args.clone(),
            result: payload.clone(),
        });
    }
}

fn redact_text(session: &SessionEntry, text: &str) -> String {
    session
        .app_controller
        .privacy()
        .scrub_text(text)
        .or_else(|| session.app_controller.privacy().redact_text(text))
        .unwrap_or_else(|| text.to_owned())
}

fn redact_snapshot(session: &SessionEntry, snapshot: &mut terminal_control::TermSnapshot) {
    if let Some(title) = snapshot.window_title.take() {
        snapshot.window_title = Some(redact_text(session, &title));
    }
    for row in &mut snapshot.visible_rows {
        row.text = redact_text(session, &row.text);
    }
}

fn parse_env_map(params: &Value) -> Result<Vec<(String, String)>, RouterError> {
    let Some(map) = params.get("env") else {
        return Ok(Vec::new());
    };
    let Some(obj) = map.as_object() else {
        return Err(RouterError::invalid_params("env must be an object"));
    };
    let mut env = Vec::with_capacity(obj.len());
    for (key, value) in obj {
        let Some(value) = value.as_str() else {
            return Err(RouterError::invalid_params(format!(
                "env.{key} must be a string"
            )));
        };
        env.push((key.clone(), value.to_owned()));
    }
    Ok(env)
}

fn optional_path(params: &Value, field: &str) -> Option<PathBuf> {
    params.get(field).and_then(Value::as_str).map(PathBuf::from)
}

fn optional_u16(params: &Value, field: &str) -> Result<Option<u16>, RouterError> {
    match params.get(field) {
        None => Ok(None),
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return Err(RouterError::invalid_params(format!(
                    "{field} must be an integer"
                )));
            };
            let converted = u16::try_from(raw)
                .map_err(|_| RouterError::invalid_params(format!("{field} exceeds u16")))?;
            Ok(Some(converted))
        }
    }
}

fn parse_mouse_kind(value: &str) -> Result<terminal_control::MouseEventKind, RouterError> {
    match value {
        "press" => Ok(terminal_control::MouseEventKind::Press),
        "release" => Ok(terminal_control::MouseEventKind::Release),
        "move" => Ok(terminal_control::MouseEventKind::Move),
        "scroll_up" => Ok(terminal_control::MouseEventKind::ScrollUp),
        "scroll_down" => Ok(terminal_control::MouseEventKind::ScrollDown),
        other => Err(RouterError::invalid_params(format!(
            "unsupported mouse kind: {other}"
        ))),
    }
}

fn parse_mouse_button(value: &str) -> Result<terminal_control::MouseButton, RouterError> {
    match value {
        "left" => Ok(terminal_control::MouseButton::Left),
        "middle" => Ok(terminal_control::MouseButton::Middle),
        "right" => Ok(terminal_control::MouseButton::Right),
        "none" => Ok(terminal_control::MouseButton::None),
        other => Err(RouterError::invalid_params(format!(
            "unsupported mouse button: {other}"
        ))),
    }
}

fn map_term_err(error: terminal_control::TermError) -> RouterError {
    match error {
        terminal_control::TermError::SessionNotFound(_) => RouterError {
            code: ErrorCode::SessionNotFound,
            message: error.to_string(),
            data: None,
        },
        terminal_control::TermError::InvalidSize { .. }
        | terminal_control::TermError::EmptyShell
        | terminal_control::TermError::MouseTrackingDisabled
        | terminal_control::TermError::MouseOutOfBounds { .. }
        | terminal_control::TermError::InvalidUtf8
        | terminal_control::TermError::UnsupportedSignal(_) => {
            RouterError::invalid_params(error.to_string())
        }
        terminal_control::TermError::CwdOutsideRootfs(_) => RouterError {
            code: ErrorCode::PermissionDenied,
            message: error.to_string(),
            data: None,
        },
        terminal_control::TermError::MissingSandboxProfile => RouterError {
            code: ErrorCode::PermissionDenied,
            message: error.to_string(),
            data: None,
        },
        terminal_control::TermError::CwdNotFound(_) => RouterError::tab_not_found(),
        terminal_control::TermError::Io(_)
        | terminal_control::TermError::Spawn(_)
        | terminal_control::TermError::Internal(_) => RouterError::internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_output_notification_uses_payload_envelope() {
        let output = terminal_control::TermOutputEvent {
            session_id: "term_7".into(),
            seq: 9,
            data: b"hello\r\n".to_vec(),
            dropped_bytes: 3,
            eof: false,
        };
        let params = build_term_output_params("s_term", &output, Some("hello\r\n".into()));
        assert_eq!(params["topic"], "term.output");
        assert_eq!(params["session_id"], "s_term");
        assert_eq!(params["payload"]["term_session_id"], "term_7");
        assert_eq!(params["payload"]["seq"], 9);
        assert_eq!(params["payload"]["bytes"], 7);
        assert_eq!(params["payload"]["dropped_bytes"], 3);
        assert_eq!(params["payload"]["eof"], false);
        assert_eq!(params["payload"]["text"], "hello\r\n");
        assert!(params.get("term_session_id").is_none());
        assert!(params.get("seq").is_none());
        assert!(params.get("bytes").is_none());
        assert!(params.get("eof").is_none());
    }

    #[test]
    fn term_exit_notification_flattens_exit_fields_into_payload() {
        let exit = terminal_control::TermExitEvent {
            session_id: "term_8".into(),
            exit: terminal_control::TermExitState {
                exited: true,
                exit_code: Some(130),
                signal: Some("SIGINT".into()),
            },
        };
        let params = build_term_exit_params("s_term", &exit);
        assert_eq!(params["topic"], "term.exit");
        assert_eq!(params["session_id"], "s_term");
        assert_eq!(params["payload"]["term_session_id"], "term_8");
        assert_eq!(params["payload"]["exit_code"], 130);
        assert_eq!(params["payload"]["signal"], "SIGINT");
        assert_eq!(params["payload"]["exited"], true);
        assert!(params.get("term_session_id").is_none());
    }
}
