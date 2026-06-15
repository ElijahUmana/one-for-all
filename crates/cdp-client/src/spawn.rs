//! Chromium spawn with `--remote-debugging-pipe`.
//!
//! Owned by `cdp-client`. Spawns a Chromium process so that:
//!
//! - Chromium's fd 3 reads commands from us (we own the write end).
//! - Chromium's fd 4 writes events/replies to us (we own the read end).
//!
//! The pipes are inherited via `pre_exec` (`dup2` of our pre-created pipe
//! ends onto fds 3 and 4). The parent side is converted to async tokio
//! pipes after the child has been spawned.
//!
//! # Threading & ownership
//!
//! [`Chromium`] owns the spawned `tokio::process::Child`, the parent ends
//! of both pipes, and a [`crate::CdpSession`] for the root browser session.
//! Concurrent access is safe via the underlying mpsc/broadcast channels.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::{Child, Command};

use crate::connection::{spawn_actors, ConnectionState};
use crate::error::{CdpError, Result};
use crate::session::{CdpSession, SessionId};

/// Builder/options for [`Chromium::launch`].
#[derive(Debug, Clone)]
pub struct ChromiumOptions {
    /// Path to the Chromium executable.
    pub binary: PathBuf,
    /// `--user-data-dir`. Per SPEC §6 each session has its own UDD under
    /// `~/.one-for-all/sessions/<session_id>/`.
    pub user_data_dir: PathBuf,
    /// Whether to launch headless (`--headless=new`) or headed.
    pub headless: bool,
    /// Extra command-line args appended after the defaults.
    pub extra_args: Vec<OsString>,
}

impl ChromiumOptions {
    pub fn new(binary: impl Into<PathBuf>, user_data_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            user_data_dir: user_data_dir.into(),
            headless: true,
            extra_args: Vec::new(),
        }
    }

    /// The default flag set per SPEC §5 (focus-no-steal layered defense).
    /// Composed in [`Chromium::launch`].
    fn baseline_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        args.push("--remote-debugging-pipe".into());
        args.push(format!("--user-data-dir={}", self.user_data_dir.display()).into());
        args.push("--no-first-run".into());
        args.push("--no-default-browser-check".into());
        args.push("--disable-default-apps".into());
        args.push("--disable-popup-blocking".into());
        args.push("--disable-prompt-on-repost".into());
        args.push("--disable-background-networking".into());
        args.push("--disable-breakpad".into());
        args.push("--disable-component-update".into());
        args.push("--disable-domain-reliability".into());
        args.push("--disable-sync".into());
        args.push("--disable-translate".into());
        args.push("--metrics-recording-only".into());
        args.push("--no-pings".into());
        args.push("--no-service-autorun".into());
        args.push("--password-store=basic".into());
        args.push("--use-mock-keychain".into());
        args.push("--enable-automation".into());
        if self.headless {
            args.push("--headless=new".into());
            args.push("--hide-scrollbars".into());
            args.push("--mute-audio".into());
        } else {
            // Headed focus-no-steal armor (SPEC §5 layer B).
            args.push("--no-startup-window".into());
            args.push("--silent-launch".into());
            args.push("--window-position=-32000,-32000".into());
            args.push("--window-size=1280,800".into());
        }
        args
    }
}

/// Handle for a spawned Chromium process.
pub struct Chromium {
    child: Child,
    /// Connection-wide state (writer queue, pending replies, sessions).
    state: Arc<ConnectionState>,
    /// Path passed in (for diagnostic logs).
    binary: PathBuf,
    /// User-data-dir passed in (for diagnostic logs).
    user_data_dir: PathBuf,
}

impl Chromium {
    /// Spawn a Chromium process and wire up the pipe transport.
    pub async fn launch(opts: ChromiumOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.user_data_dir).map_err(|e| {
            CdpError::Internal(format!(
                "create user_data_dir {}: {}",
                opts.user_data_dir.display(),
                e
            ))
        })?;

        let mut cmd = build_command(&opts).map_err(|e| CdpError::Internal(e.to_string()))?;
        // Stdin/stdout/stderr are NOT used for CDP — only fd 3/4 are.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        // Set up pipes and configure pre_exec so the child sees them on fd 3/4.
        // On unix this is implemented in `unix_setup`; non-unix is unsupported.
        #[cfg(unix)]
        let (parent_w, parent_r) = unix_setup::wire_pipes(&mut cmd)?;
        #[cfg(not(unix))]
        let (parent_w, parent_r): (tokio::io::DuplexStream, tokio::io::DuplexStream) = {
            let _ = &mut cmd;
            return Err(CdpError::Internal(
                "cdp-client supports only unix targets".into(),
            ));
        };

        let child = cmd.spawn().map_err(CdpError::Spawn)?;
        let (state, _closed) = spawn_actors(parent_r, parent_w);

        Ok(Self {
            child,
            state,
            binary: opts.binary,
            user_data_dir: opts.user_data_dir,
        })
    }

    /// The root browser CDP session (sessionId = `""`).
    pub fn root_session(&self) -> CdpSession {
        self.state.root.clone()
    }

    /// Obtain the session for an attached target id, creating it on demand.
    pub fn session_for(&self, id: &SessionId) -> CdpSession {
        self.state.session_for(id)
    }

    /// Returns the child's PID, if known.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Path to the launched binary.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// User-data-dir of the launched browser.
    pub fn user_data_dir(&self) -> &Path {
        &self.user_data_dir
    }

    /// Send `Browser.close` and wait up to `grace` for the child to exit;
    /// otherwise SIGKILL.
    pub async fn shutdown(mut self, grace: std::time::Duration) -> Result<()> {
        // Best-effort `Browser.close`.
        let _ = self
            .state
            .root
            .send_raw("Browser.close", serde_json::Value::Null)
            .await;
        let killed = match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(Ok(_)) => false,
            _ => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                true
            }
        };
        if killed {
            tracing::warn!("Chromium child SIGKILLed after grace period");
        }
        Ok(())
    }
}

fn build_command(opts: &ChromiumOptions) -> anyhow::Result<Command> {
    let mut cmd = Command::new(&opts.binary);
    for a in opts.baseline_args() {
        cmd.arg(a);
    }
    for a in &opts.extra_args {
        cmd.arg(a);
    }
    cmd.kill_on_drop(true);
    Ok(cmd)
}

#[cfg(unix)]
mod unix_setup {
    use super::*;
    use nix::unistd::{close as nix_close, dup2, pipe};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
    use tokio::process::Command;

    /// Create the two pipes and arrange for the child to receive them as fd 3/4.
    /// Returns the parent ends as async tokio pipes (write-end, read-end).
    pub(crate) fn wire_pipes(
        cmd: &mut Command,
    ) -> Result<(
        tokio::net::unix::pipe::Sender,
        tokio::net::unix::pipe::Receiver,
    )> {
        // pipe(): (read_end, write_end). We need:
        //   - parent_w → child fd 3 (chromium reads)
        //   - child fd 4 → parent_r (chromium writes)
        let (child_r_fd, parent_w_fd) = pipe().map_err(io_err)?;
        let (parent_r_fd, child_w_fd) = pipe().map_err(io_err)?;

        // Convert to OwnedFd so they're closed if we error out below.
        let child_r: OwnedFd = child_r_fd;
        let parent_w: OwnedFd = parent_w_fd;
        let parent_r: OwnedFd = parent_r_fd;
        let child_w: OwnedFd = child_w_fd;

        // Convert parent ends to async pipes immediately so OwnedFds are
        // tied to those handles for their lifetime.
        let parent_w_async = tokio::net::unix::pipe::Sender::from_owned_fd(parent_w)
            .map_err(super::CdpError::Spawn)?;
        let parent_r_async = tokio::net::unix::pipe::Receiver::from_owned_fd(parent_r)
            .map_err(super::CdpError::Spawn)?;

        // The child ends move into the pre_exec closure below. After fork
        // (before exec) we dup2 them onto fd 3 / 4, then close the originals.
        let child_r_raw: RawFd = child_r.into_raw_fd();
        let child_w_raw: RawFd = child_w.into_raw_fd();

        unsafe {
            cmd.pre_exec(move || -> io::Result<()> {
                // dup2 child_r → 3, child_w → 4. dup2 is async-signal-safe.
                if dup2(child_r_raw, 3).is_err() {
                    return Err(io::Error::last_os_error());
                }
                if dup2(child_w_raw, 4).is_err() {
                    return Err(io::Error::last_os_error());
                }
                // Close the originals if they were not 3/4 already.
                if child_r_raw != 3 {
                    let _ = nix_close(child_r_raw);
                }
                if child_w_raw != 4 {
                    let _ = nix_close(child_w_raw);
                }
                Ok(())
            });
        }

        Ok((parent_w_async, parent_r_async))
    }

    fn io_err(e: nix::errno::Errno) -> io::Error {
        io::Error::from_raw_os_error(e as i32)
    }

    // Helpers for AsRawFd debug.
    #[allow(dead_code)]
    fn fd_of<F: AsRawFd>(f: &F) -> RawFd {
        f.as_raw_fd()
    }
    #[allow(dead_code)]
    fn from_raw<F: FromRawFd>(fd: RawFd) -> F {
        unsafe { F::from_raw_fd(fd) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn baseline_args_include_pipe_flag_and_udd() {
        let opts = ChromiumOptions::new("/tmp/no-such", "/tmp/udd");
        let args: Vec<String> = opts
            .baseline_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--remote-debugging-pipe"));
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/udd"));
        assert!(args.iter().any(|a| a == "--headless=new"));
    }

    #[test]
    fn headed_args_use_offscreen_window() {
        let mut opts = ChromiumOptions::new("/tmp/no-such", "/tmp/udd");
        opts.headless = false;
        let args: Vec<String> = opts
            .baseline_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "--no-startup-window"));
        assert!(args.iter().any(|a| a == "--window-position=-32000,-32000"));
        assert!(!args.iter().any(|a| a.starts_with("--headless")));
    }
}
