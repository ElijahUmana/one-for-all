//! # browser-engine
//!
//! High-level Chromium driver. **Per spec D2/D3** there is one [`Browser`] per
//! session — each owns its own Chromium child process and `--user-data-dir`.
//! There is no shared Chromium across sessions in v1.
//!
//! Stack position:
//!
//! ```text
//! broker
//!   └── browser_engine::Browser  (this crate, one per SessionId)
//!         ├── focus_manager      (spawn-without-focus-steal)
//!         ├── cdp_client         (typed CDP transport)
//!         ├── ax                 (snapshot — internal pending T4)
//!         └── Chromium           (child process)
//! ```
//!
//! ## CDP transport
//!
//! All CDP traffic goes through the [`cdp_client`] crate. browser-engine
//! retains ownership of `tokio::process::Child` (so `pre_exec` can layer
//! RLIMIT_AS / RLIMIT_CPU and focus-restore) and hands the parent ends of
//! the fd 3 / fd 4 pipes to [`cdp_client::Connection::from_pipe_halves`].
//! Every CDP call site uses typed `cdp_client::Command` implementations —
//! method-name typos are compile errors, not runtime errors.

#![deny(rust_2018_idioms)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::result_large_err)]

pub mod actions;
pub mod browser;
pub mod context;
pub mod cookies;
pub mod emulate;
pub mod input_translation;
pub mod network;
pub mod page;
pub mod pdf;
pub mod perf;
pub mod snapshot;
pub mod stealth;
pub mod wait;

pub use browser::{validate_navigable_url, Browser, BrowserConfig, LaunchError};
pub use context::{BrowserContext, ContextId};
pub use network::{
    ErrorReason, EsMessage, HarContent, HarCreator, HarEntry, HarExport, HarLog, HarNameValue,
    HarRequest, HarResponse, InterceptAction, MockResponse, NetworkRegistry, ProxyAuth,
    ProxyConfig, RequestOverrides, WsFrame, WsFrameKind,
};
pub use page::{Page, TabId};
pub use snapshot::{Element, Snapshot, SnapshotDelta, SnapshotResponse};

use serde::{Deserialize, Serialize};

/// Locked spec wait predicate, see SPEC §7 `tab.open` / `tab.navigate`.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WaitUntil {
    Load,
    DomContentLoaded,
    NetworkIdle,
    #[default]
    None,
}

/// Per-page lifecycle event broadcast capacity. Re-exported from
/// [`observability::caps::PAGE_LIFECYCLE_CAPACITY`] (N2 centralization).
pub(crate) const PAGE_LIFECYCLE_CAPACITY: usize = observability::caps::PAGE_LIFECYCLE_CAPACITY;
