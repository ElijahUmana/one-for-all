//! Per-session lifecycle: idle → drain → kill (SPEC D18).
//!
//! Trigger is socket disconnect, not `session.unregister`. After
//! disconnect, a 5-minute draining window starts; if the same session
//! reconnects (rare in practice), the draining is cancelled. Otherwise the
//! Browser is shut down and the registry entry removed.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::registry::{SessionEntry, SessionRegistry};

/// Configuration for the lifecycle FSM.
#[derive(Debug, Clone, Copy)]
pub struct IdleConfig {
    pub drain_after: Duration,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            drain_after: Duration::from_secs(300), // SPEC D18: 5 minutes
        }
    }
}

/// Messages handled by a session's lifecycle task.
#[derive(Debug)]
pub enum LifecycleMessage {
    Connected,
    Disconnected,
    Shutdown,
}

/// Per-session lifecycle task. Cheap to clone (`mpsc::Sender` internally).
#[derive(Clone)]
pub struct SessionLifecycle {
    tx: mpsc::Sender<LifecycleMessage>,
}

impl SessionLifecycle {
    /// Spawn a per-session lifecycle task. The returned handle's `Drop`
    /// does not stop the task; send `LifecycleMessage::Shutdown` to do so
    /// gracefully.
    pub fn spawn(
        registry: Arc<SessionRegistry>,
        entry: Arc<SessionEntry>,
        idle: IdleConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel(observability::caps::SESSION_LIFECYCLE_CAPACITY);
        tokio::spawn(run(registry, entry, rx, idle));
        Self { tx }
    }

    pub async fn notify_connected(&self) {
        let _ = self.tx.send(LifecycleMessage::Connected).await;
    }
    pub async fn notify_disconnected(&self) {
        let _ = self.tx.send(LifecycleMessage::Disconnected).await;
    }
    pub async fn shutdown(&self) {
        let _ = self.tx.send(LifecycleMessage::Shutdown).await;
    }
}

async fn run(
    registry: Arc<SessionRegistry>,
    entry: Arc<SessionEntry>,
    mut rx: mpsc::Receiver<LifecycleMessage>,
    idle: IdleConfig,
) {
    debug!(session_id = %entry.session_id, "lifecycle task started");
    let mut connected_count: usize = 0;
    let mut drain_deadline: Option<tokio::time::Instant> = None;

    loop {
        let next_msg = match drain_deadline {
            Some(d) => {
                let sleep = tokio::time::sleep_until(d);
                tokio::select! {
                    msg = rx.recv() => msg,
                    _ = sleep => {
                        info!(session_id = %entry.session_id, "drain elapsed; shutting down session");
                        break;
                    }
                }
            }
            None => rx.recv().await,
        };

        let Some(msg) = next_msg else {
            break;
        };
        match msg {
            LifecycleMessage::Connected => {
                connected_count = connected_count.saturating_add(1);
                drain_deadline = None;
                debug!(
                    session_id = %entry.session_id,
                    connected_count,
                    "lifecycle: connected"
                );
            }
            LifecycleMessage::Disconnected => {
                connected_count = connected_count.saturating_sub(1);
                if connected_count == 0 {
                    drain_deadline = Some(tokio::time::Instant::now() + idle.drain_after);
                    info!(
                        session_id = %entry.session_id,
                        drain_secs = idle.drain_after.as_secs(),
                        "lifecycle: disconnected, starting drain"
                    );
                } else {
                    debug!(
                        session_id = %entry.session_id,
                        connected_count,
                        "lifecycle: disconnected but session still has active connections"
                    );
                }
            }
            LifecycleMessage::Shutdown => {
                info!(session_id = %entry.session_id, "lifecycle: shutdown requested");
                break;
            }
        }
    }

    *entry.lifecycle.lock() = None;
    // SPEC §10 M4 — `load_full()` returns an Arc snapshot we can `.await`
    // against without holding the ArcSwap guard.
    let browser = entry.browser.load_full();
    entry.shutdown_system_watches();
    entry.shutdown_terminals().await;
    if let Err(e) = browser.shutdown().await {
        warn!(session_id = %entry.session_id, error = %e, "browser shutdown error");
    }
    registry.remove(&entry.session_id);
    info!(session_id = %entry.session_id, "lifecycle task exited");
}
