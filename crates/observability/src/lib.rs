//! Centralized observability for one-for-all components.
//!
//! Each binary (`broker`, `mcp`, etc.) calls [`init`] exactly once at startup
//! with its component name. Logs are written as JSON to
//! `~/.one-for-all/logs/<component>/<component>.log` with daily rotation,
//! keeping the last 7 days. Set `OFA_LOG_STDERR=1` to also emit pretty output to
//! stderr (useful when running a binary in the foreground).
//!
//! Filtering is controlled by `OFA_LOG` (a `tracing_subscriber::EnvFilter`
//! directive). Default: `info,one_for_all=debug`.

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub mod caps;
pub mod cdp;
pub mod latency;
pub mod log_dirs;
pub mod metrics;
pub mod trace;

pub use cdp::{CdpMethodSnapshot, CdpMethodsSnapshot, CdpMetricsSink};
pub use latency::{LatencyHistogram, LatencySnapshot, LatencyTimer};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Holds the non-blocking appender's worker thread alive. The caller must keep
/// it for the lifetime of the program — dropping it flushes pending logs and
/// stops the writer.
pub struct LogGuard {
    _file: WorkerGuard,
}

/// Initialize tracing for `component`. Idempotent; subsequent calls return an
/// inert guard and leave the global subscriber untouched.
pub fn init(component: &str) -> Result<LogGuard> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(LogGuard {
            // Re-initialization is a no-op; produce a guard that is safe to
            // drop. Constructing a real WorkerGuard requires an appender, so
            // we build a throwaway one against the same dir — its drop is
            // harmless.
            _file: spawn_inert_guard(component)?,
        });
    }

    let dir = log_dirs::for_component(component)
        .with_context(|| format!("resolving log dir for component {component:?}"))?;
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .max_log_files(7)
        .filename_prefix(component)
        .filename_suffix("log")
        .build(&dir)
        .with_context(|| format!("building rolling appender at {}", dir.display()))?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_env("OFA_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,one_for_all=debug,broker=debug,mcp=debug"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .json()
        .with_current_span(true)
        .with_span_list(false);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    if std::env::var_os("OFA_LOG_STDERR").is_some() {
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(true)
            .compact();
        registry.with(stderr_layer).try_init()?;
    } else {
        registry.try_init()?;
    }

    tracing::debug!(component = component, dir = %dir.display(), "observability initialized");
    Ok(LogGuard { _file: file_guard })
}

fn spawn_inert_guard(component: &str) -> Result<WorkerGuard> {
    // Used only on a re-init call; we still produce a valid WorkerGuard so the
    // caller's RAII contract holds without poisoning the live subscriber.
    let dir = log_dirs::for_component(component)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .max_log_files(1)
        .filename_prefix(format!("{component}.inert"))
        .filename_suffix("log")
        .build(&dir)?;
    let (_writer, guard) = tracing_appender::non_blocking(appender);
    Ok(guard)
}
