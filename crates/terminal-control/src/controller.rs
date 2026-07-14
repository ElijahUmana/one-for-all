use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::state::{SharedState, DEFAULT_RAW_OUTPUT_BYTES, DEFAULT_SCROLLBACK_LINES};
use crate::types::{
    MouseButton, MouseEventKind, MouseEventRequest, SessionSandbox, SpawnTerminalRequest,
    SpawnTerminalResult, TermAltScreenState, TermError, TermExitEvent, TermExitState,
    TermMouseEncoding, TermMouseMode, TermOutputEvent, TermReadChunk, TermScrollbackLine,
    TermSnapshot, TerminalEvent,
};
use crate::NotificationSink;

const READ_BUFFER_BYTES: usize = 8192;
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLOSE_WAIT_TRIES: usize = 20;
const CLOSE_WAIT_STEP: Duration = Duration::from_millis(50);
const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin";

pub struct TerminalController {
    sessions: DashMap<String, Arc<TermSession>>,
    next_id: AtomicU64,
    notification_sink: parking_lot::RwLock<Option<Arc<dyn NotificationSink>>>,
}

impl std::fmt::Debug for TerminalController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalController")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl Default for TerminalController {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TerminalController {
    pub fn new(notification_sink: Option<Arc<dyn NotificationSink>>) -> Self {
        Self {
            sessions: DashMap::new(),
            next_id: AtomicU64::new(1),
            notification_sink: parking_lot::RwLock::new(notification_sink),
        }
    }

    pub fn set_notification_sink(&self, sink: Option<Arc<dyn NotificationSink>>) {
        *self.notification_sink.write() = sink;
    }

    pub async fn spawn_terminal(
        &self,
        request: SpawnTerminalRequest,
    ) -> Result<SpawnTerminalResult, TermError> {
        validate_size(request.rows, request.cols)?;
        let shell = resolve_shell_path(&request.shell)?;
        let cwd = resolve_cwd(request.sandbox.as_ref(), request.cwd.as_deref())?;
        let cmd = build_command(&shell, &cwd, request.sandbox.as_ref(), &request.env)?;
        let system = portable_pty::native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TermError::Spawn(e.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TermError::Spawn(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TermError::Spawn(e.to_string()))?;
        let child = pair
            .slave
            .spawn_command(cmd.clone())
            .map_err(|e| TermError::Spawn(e.to_string()))?;

        let session_id = format!("term_{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let shared = SharedState::new(
            request.rows,
            request.cols,
            DEFAULT_RAW_OUTPUT_BYTES,
            DEFAULT_SCROLLBACK_LINES,
        );
        let session = Arc::new(TermSession {
            session_id: session_id.clone(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(child),
            shared: shared.clone(),
            read_handle: Mutex::new(None),
            exit_handle: Mutex::new(None),
        });

        let sink = self.notification_sink.read().clone();
        let shared_for_read = shared.clone();
        let session_id_for_read = session_id.clone();
        let read_handle = tokio::task::spawn_blocking(move || {
            let mut buf = [0_u8; READ_BUFFER_BYTES];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        let dropped = shared_for_read.process_output(chunk);
                        if let Some(sink) = sink.as_ref() {
                            sink.notify(TerminalEvent::Output(TermOutputEvent {
                                session_id: session_id_for_read.clone(),
                                seq: shared_for_read.next_output_seq(),
                                data: chunk.to_vec(),
                                dropped_bytes: dropped,
                                eof: false,
                            }));
                        }
                    }
                    Err(error) => {
                        warn!(session_id = %session_id_for_read, error = %error, "terminal reader loop failed");
                        break;
                    }
                }
            }
        });
        *session.read_handle.lock() = Some(read_handle);

        let session_for_exit = Arc::clone(&session);
        let sink = self.notification_sink.read().clone();
        let exit_handle = tokio::spawn(async move {
            loop {
                match session_for_exit.try_wait_once() {
                    Ok(Some(exit)) => {
                        session_for_exit
                            .shared
                            .mark_exit(exit.exit_code, exit.signal.clone());
                        if let Some(sink) = sink.as_ref() {
                            sink.notify(TerminalEvent::Exit(TermExitEvent {
                                session_id: session_for_exit.session_id.clone(),
                                exit,
                            }));
                        }
                        break;
                    }
                    Ok(None) => tokio::time::sleep(EXIT_POLL_INTERVAL).await,
                    Err(error) => {
                        warn!(session_id = %session_for_exit.session_id, error = %error, "terminal exit polling failed");
                        break;
                    }
                }
            }
        });
        *session.exit_handle.lock() = Some(exit_handle);

        self.sessions
            .insert(session_id.clone(), Arc::clone(&session));
        Ok(SpawnTerminalResult {
            session_id,
            rows: request.rows,
            cols: request.cols,
        })
    }

    pub fn write_bytes(&self, session_id: &str, data: &[u8]) -> Result<usize, TermError> {
        let session = self.session(session_id)?;
        session.write_bytes(data)
    }

    pub fn read_output(
        &self,
        session_id: &str,
        max_bytes: usize,
    ) -> Result<TermReadChunk, TermError> {
        let session = self.session(session_id)?;
        Ok(session.shared.read_output(max_bytes))
    }

    pub fn snapshot(&self, session_id: &str) -> Result<TermSnapshot, TermError> {
        let session = self.session(session_id)?;
        Ok(session.shared.build_snapshot(session_id))
    }

    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<(), TermError> {
        validate_size(rows, cols)?;
        let session = self.session(session_id)?;
        {
            let master = session.master.lock();
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| TermError::Io(e.to_string()))?;
        }
        session.shared.resize(rows, cols);
        Ok(())
    }

    pub fn scrollback(
        &self,
        session_id: &str,
        n_lines: usize,
    ) -> Result<Vec<TermScrollbackLine>, TermError> {
        let session = self.session(session_id)?;
        Ok(session.shared.scrollback_lines(n_lines))
    }

    pub fn alt_screen_state(&self, session_id: &str) -> Result<TermAltScreenState, TermError> {
        let session = self.session(session_id)?;
        Ok(session.shared.alt_screen_state())
    }

    pub fn send_signal(&self, session_id: &str, signal_name: &str) -> Result<(), TermError> {
        let session = self.session(session_id)?;
        let signal = parse_signal(signal_name)?;
        session.send_signal(signal)
    }

    pub fn mouse_event(
        &self,
        session_id: &str,
        request: MouseEventRequest,
    ) -> Result<(), TermError> {
        let session = self.session(session_id)?;
        let snapshot = session.shared.build_snapshot(session_id);
        if request.row >= snapshot.rows || request.col >= snapshot.cols {
            return Err(TermError::MouseOutOfBounds {
                rows: snapshot.rows,
                cols: snapshot.cols,
                row: request.row,
                col: request.col,
            });
        }
        let sequence = encode_mouse_event(snapshot.mouse_mode, snapshot.mouse_encoding, request)?;
        session.write_bytes(sequence.as_bytes()).map(|_| ())
    }

    pub async fn close(&self, session_id: &str) -> Result<TermExitState, TermError> {
        let Some((_, session)) = self.sessions.remove(session_id) else {
            return Err(TermError::SessionNotFound(session_id.to_owned()));
        };
        session.close().await
    }

    pub async fn shutdown_all(&self) {
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for session_id in sessions {
            if let Err(error) = self.close(&session_id).await {
                warn!(session_id = %session_id, error = %error, "terminal shutdown failed");
            }
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    fn session(&self, session_id: &str) -> Result<Arc<TermSession>, TermError> {
        self.sessions
            .get(session_id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| TermError::SessionNotFound(session_id.to_owned()))
    }
}

struct TermSession {
    session_id: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    shared: SharedState,
    read_handle: Mutex<Option<JoinHandle<()>>>,
    exit_handle: Mutex<Option<JoinHandle<()>>>,
}

impl TermSession {
    fn write_bytes(&self, data: &[u8]) -> Result<usize, TermError> {
        let mut guard = self.writer.lock();
        let writer = guard
            .as_mut()
            .ok_or_else(|| TermError::Internal("terminal writer already closed".to_owned()))?;
        writer
            .write_all(data)
            .map_err(|e| TermError::Io(e.to_string()))?;
        writer.flush().map_err(|e| TermError::Io(e.to_string()))?;
        Ok(data.len())
    }

    fn try_wait_once(&self) -> Result<Option<TermExitState>, TermError> {
        let mut child = self.child.lock();
        let status = child.try_wait().map_err(|e| TermError::Io(e.to_string()))?;
        Ok(status.map(|status| TermExitState {
            exited: true,
            exit_code: Some(status.exit_code()),
            signal: status.signal().map(ToOwned::to_owned),
        }))
    }

    fn send_signal(&self, signal: Signal) -> Result<(), TermError> {
        #[cfg(unix)]
        {
            let pgid = {
                let master = self.master.lock();
                master.process_group_leader()
            };
            if let Some(pgid) = pgid {
                signal::kill(Pid::from_raw(-pgid), signal)
                    .map_err(|e| TermError::Io(e.to_string()))?;
                return Ok(());
            }
            let pid = {
                let child = self.child.lock();
                child.process_id()
            };
            if let Some(pid) = pid {
                signal::kill(Pid::from_raw(pid as i32), signal)
                    .map_err(|e| TermError::Io(e.to_string()))?;
                return Ok(());
            }
            Err(TermError::Internal("terminal child has no pid".to_owned()))
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            Err(TermError::UnsupportedSignal(
                "signals require unix".to_owned(),
            ))
        }
    }

    async fn close(&self) -> Result<TermExitState, TermError> {
        self.writer.lock().take();
        let _ = self.send_signal(Signal::SIGHUP);
        if let Some(exit) = wait_for_exit(self).await? {
            self.abort_tasks();
            return Ok(exit);
        }
        let _ = self.send_signal(Signal::SIGTERM);
        if let Some(exit) = wait_for_exit(self).await? {
            self.abort_tasks();
            return Ok(exit);
        }
        {
            let mut child = self.child.lock();
            child.kill().map_err(|e| TermError::Io(e.to_string()))?;
        }
        let exit = wait_for_exit(self).await?.unwrap_or(TermExitState {
            exited: true,
            exit_code: None,
            signal: Some("SIGKILL".to_owned()),
        });
        self.shared.mark_exit(exit.exit_code, exit.signal.clone());
        self.abort_tasks();
        Ok(exit)
    }

    fn abort_tasks(&self) {
        if let Some(handle) = self.read_handle.lock().take() {
            handle.abort();
        }
        if let Some(handle) = self.exit_handle.lock().take() {
            handle.abort();
        }
    }
}

async fn wait_for_exit(session: &TermSession) -> Result<Option<TermExitState>, TermError> {
    for _ in 0..CLOSE_WAIT_TRIES {
        if let Some(exit) = session.try_wait_once()? {
            session
                .shared
                .mark_exit(exit.exit_code, exit.signal.clone());
            return Ok(Some(exit));
        }
        tokio::time::sleep(CLOSE_WAIT_STEP).await;
    }
    Ok(None)
}

fn validate_size(rows: u16, cols: u16) -> Result<(), TermError> {
    if rows == 0 || cols == 0 {
        return Err(TermError::InvalidSize { rows, cols });
    }
    Ok(())
}

fn resolve_shell_path(shell: &str) -> Result<PathBuf, TermError> {
    if shell.trim().is_empty() {
        return Err(TermError::EmptyShell);
    }
    let candidate = Path::new(shell);
    if candidate.is_absolute() {
        return Ok(candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from(DEFAULT_PATH));
    for dir in std::env::split_paths(&path) {
        let full = dir.join(shell);
        if full.is_file() {
            return Ok(full);
        }
    }
    Err(TermError::Spawn(format!(
        "shell not found on PATH: {shell}"
    )))
}

fn resolve_cwd(sandbox: Option<&SessionSandbox>, cwd: Option<&Path>) -> Result<PathBuf, TermError> {
    match sandbox {
        Some(sandbox) => resolve_sandbox_cwd(sandbox, cwd),
        None => {
            let candidate = match cwd {
                Some(path) if path.is_absolute() => path.to_path_buf(),
                Some(path) => std::env::current_dir()
                    .map_err(|e| TermError::Io(e.to_string()))?
                    .join(path),
                None => std::env::current_dir().map_err(|e| TermError::Io(e.to_string()))?,
            };
            if candidate.is_dir() {
                Ok(candidate)
            } else {
                Err(TermError::CwdNotFound(candidate.display().to_string()))
            }
        }
    }
}

fn resolve_sandbox_cwd(sandbox: &SessionSandbox, cwd: Option<&Path>) -> Result<PathBuf, TermError> {
    let rootfs = &sandbox.rootfs;
    let candidate = match cwd {
        None => rootfs.clone(),
        Some(path) if path == Path::new("/") => rootfs.clone(),
        Some(path) if path.starts_with(rootfs) => path.to_path_buf(),
        Some(path) if path.is_relative() => rootfs.join(path),
        Some(path) => map_host_path_into_rootfs(rootfs, &sandbox.inherit, path)?,
    };
    if !candidate.exists() || !candidate.is_dir() {
        return Err(TermError::CwdNotFound(candidate.display().to_string()));
    }
    let canonical = std::fs::canonicalize(&candidate).map_err(|e| TermError::Io(e.to_string()))?;
    if !canonical.starts_with(rootfs) {
        return Err(TermError::CwdOutsideRootfs(canonical.display().to_string()));
    }
    Ok(canonical)
}

fn map_host_path_into_rootfs(
    rootfs: &Path,
    inherit: &[sandbox::InheritSpec],
    host_path: &Path,
) -> Result<PathBuf, TermError> {
    if let Some(home) = dirs::home_dir() {
        if host_path == home {
            return Ok(rootfs.to_path_buf());
        }
    }
    for spec in inherit {
        if host_path == spec.host_path || host_path.starts_with(&spec.host_path) {
            let Some(leaf) = spec.host_path.file_name() else {
                continue;
            };
            let suffix = host_path.strip_prefix(&spec.host_path).map_err(|e| {
                TermError::Internal(format!(
                    "strip_prefix failed for {}: {e}",
                    host_path.display()
                ))
            })?;
            return Ok(rootfs.join(leaf).join(suffix));
        }
    }
    Err(TermError::CwdOutsideRootfs(host_path.display().to_string()))
}

fn build_command(
    shell: &Path,
    cwd: &Path,
    sandbox: Option<&SessionSandbox>,
    env_overrides: &[(String, String)],
) -> Result<CommandBuilder, TermError> {
    let mut builder = if let Some(sandbox) = sandbox {
        let (binary, argv) = sandbox::wrap_argv(&sandbox.profile_path, shell, &[])
            .map_err(|e| TermError::Spawn(e.to_string()))?;
        let mut full_argv = Vec::with_capacity(argv.len() + 1);
        full_argv.push(binary.into_os_string());
        full_argv.extend(argv);
        CommandBuilder::from_argv(full_argv)
    } else {
        CommandBuilder::new(shell.as_os_str())
    };

    builder.env_clear();
    builder.cwd(cwd.as_os_str());
    builder.env(
        "PATH",
        std::env::var_os("PATH").unwrap_or_else(|| OsString::from(DEFAULT_PATH)),
    );
    builder.env("TERM", "xterm-256color");
    builder.env("PWD", cwd.as_os_str());
    builder.env("SHELL", shell.as_os_str());
    if let Some(lang) = std::env::var_os("LANG") {
        builder.env("LANG", lang);
    }
    if let Some(sandbox) = sandbox {
        let tmpdir = sandbox.rootfs.join("tmp");
        std::fs::create_dir_all(&tmpdir).map_err(|e| TermError::Io(e.to_string()))?;
        builder.env("HOME", &sandbox.rootfs);
        builder.env("TMPDIR", &tmpdir);
    } else {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        builder.env("HOME", home);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            builder.env("TMPDIR", tmpdir);
        }
    }

    for (key, value) in env_overrides {
        builder.env(key, value);
    }
    Ok(builder)
}

fn parse_signal(name: &str) -> Result<Signal, TermError> {
    let upper = name.trim().to_ascii_uppercase();
    let normalized = upper.strip_prefix("SIG").unwrap_or(&upper);
    match normalized {
        "HUP" => Ok(Signal::SIGHUP),
        "INT" => Ok(Signal::SIGINT),
        "QUIT" => Ok(Signal::SIGQUIT),
        "TERM" => Ok(Signal::SIGTERM),
        "KILL" => Ok(Signal::SIGKILL),
        "USR1" => Ok(Signal::SIGUSR1),
        "USR2" => Ok(Signal::SIGUSR2),
        "TSTP" => Ok(Signal::SIGTSTP),
        "CONT" => Ok(Signal::SIGCONT),
        other => Err(TermError::UnsupportedSignal(other.to_owned())),
    }
}

fn encode_mouse_event(
    mode: TermMouseMode,
    encoding: TermMouseEncoding,
    request: MouseEventRequest,
) -> Result<String, TermError> {
    if matches!(mode, TermMouseMode::None) {
        return Err(TermError::MouseTrackingDisabled);
    }
    let mut code = modifier_bits(request.shift, request.alt, request.ctrl);
    let release_uses_lowercase = matches!(encoding, TermMouseEncoding::Sgr)
        && matches!(request.kind, MouseEventKind::Release);
    match request.kind {
        MouseEventKind::Press => {
            code = code.saturating_add(button_code(request.button.unwrap_or(MouseButton::Left))?);
        }
        MouseEventKind::Release => {
            code = code.saturating_add(3);
        }
        MouseEventKind::Move => {
            code = code.saturating_add(32).saturating_add(motion_button_code(
                request.button.unwrap_or(MouseButton::None),
            )?);
        }
        MouseEventKind::ScrollUp => {
            code = code.saturating_add(64);
        }
        MouseEventKind::ScrollDown => {
            code = code.saturating_add(65);
        }
    }
    let x = request.col.saturating_add(1);
    let y = request.row.saturating_add(1);
    match encoding {
        TermMouseEncoding::Sgr => Ok(format!(
            "\u{1b}[<{};{};{}{}",
            code,
            x,
            y,
            if release_uses_lowercase { 'm' } else { 'M' }
        )),
        TermMouseEncoding::Default | TermMouseEncoding::Utf8 => {
            let c1 = x.saturating_add(32);
            let c2 = y.saturating_add(32);
            if c1 > 255 || c2 > 255 {
                return Err(TermError::MouseOutOfBounds {
                    rows: y,
                    cols: x,
                    row: request.row,
                    col: request.col,
                });
            }
            let cb = u32::from(code.saturating_add(32));
            let cx = u32::from(c1);
            let cy = u32::from(c2);
            let Some(b0) = char::from_u32(cb) else {
                return Err(TermError::Internal(
                    "invalid mouse event button encoding".to_owned(),
                ));
            };
            let Some(b1) = char::from_u32(cx) else {
                return Err(TermError::Internal(
                    "invalid mouse event x encoding".to_owned(),
                ));
            };
            let Some(b2) = char::from_u32(cy) else {
                return Err(TermError::Internal(
                    "invalid mouse event y encoding".to_owned(),
                ));
            };
            Ok(format!("\u{1b}[M{b0}{b1}{b2}"))
        }
    }
}

fn modifier_bits(shift: bool, alt: bool, ctrl: bool) -> u8 {
    let mut bits = 0_u8;
    if shift {
        bits = bits.saturating_add(4);
    }
    if alt {
        bits = bits.saturating_add(8);
    }
    if ctrl {
        bits = bits.saturating_add(16);
    }
    bits
}

fn button_code(button: MouseButton) -> Result<u8, TermError> {
    match button {
        MouseButton::Left => Ok(0),
        MouseButton::Middle => Ok(1),
        MouseButton::Right => Ok(2),
        MouseButton::None => Err(TermError::Internal(
            "buttonless press is invalid".to_owned(),
        )),
    }
}

fn motion_button_code(button: MouseButton) -> Result<u8, TermError> {
    match button {
        MouseButton::Left => Ok(0),
        MouseButton::Middle => Ok(1),
        MouseButton::Right => Ok(2),
        MouseButton::None => Ok(3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn host_downloads_path_maps_into_rootfs() {
        let sandbox = SessionSandbox {
            rootfs: PathBuf::from("/tmp/rootfs"),
            user_data_dir: PathBuf::from("/tmp/rootfs"),
            profile_path: PathBuf::from("/tmp/rootfs/sandbox.sb"),
            seed_plan_path: PathBuf::from("/tmp/rootfs/v_r1_seed.json"),
            inherit: vec![sandbox::InheritSpec::rw("/Users/test/Downloads")],
            network_outbound: true,
            native_ax_allowed: false,
            enforced: true,
        };
        let mapped = map_host_path_into_rootfs(
            &sandbox.rootfs,
            &sandbox.inherit,
            Path::new("/Users/test/Downloads/demo/file.txt"),
        )
        .expect("map host path");
        assert_eq!(mapped, PathBuf::from("/tmp/rootfs/Downloads/demo/file.txt"));
    }

    #[test]
    fn sgr_mouse_release_uses_lowercase_m() {
        let encoded = encode_mouse_event(
            TermMouseMode::PressRelease,
            TermMouseEncoding::Sgr,
            MouseEventRequest {
                row: 2,
                col: 4,
                kind: MouseEventKind::Release,
                button: Some(MouseButton::Left),
                shift: false,
                alt: false,
                ctrl: false,
            },
        )
        .expect("encode sgr release");
        assert_eq!(encoded, "\u{1b}[<3;5;3m");
    }

    #[test]
    fn default_mouse_encoding_produces_x10_sequence() {
        let encoded = encode_mouse_event(
            TermMouseMode::Press,
            TermMouseEncoding::Default,
            MouseEventRequest {
                row: 0,
                col: 0,
                kind: MouseEventKind::Press,
                button: Some(MouseButton::Left),
                shift: false,
                alt: false,
                ctrl: false,
            },
        )
        .expect("encode default mouse");
        assert!(encoded.starts_with("\u{1b}[M"));
    }
}
