//! `cdp-client` — Chromium DevTools Protocol client over
//! `--remote-debugging-pipe` (NUL-delimited JSON, fd 3 read / fd 4 write).
//!
//! # Public surface
//!
//! - [`Chromium`] — spawn handle (one per child process).
//! - [`CdpSession`] — one per `Target.attachedToTarget`. `send::<C>(params)`
//!   returns `Result<C::Returns>`; `events()` returns a `broadcast::Receiver`
//!   of every event for that session.
//! - [`Command`] — trait implemented by every generated `<Cmd>Params` struct.
//!   Provides the wire method name and the matching `Returns` type.
//! - [`framing`] — pure NUL-delimited JSON codec (also useful for tests).
//! - [`generated`] — every CDP domain's commands/events as Rust types.
//!
//! # Threading model
//!
//! Per [`Chromium`] there is one reader actor and one writer actor on the
//! pipe. The reader demuxes by `sessionId` to per-session `mpsc::Sender`s;
//! commands round-trip through a `DashMap<id, oneshot::Sender>` so callers
//! get back the right reply without ordering assumptions.
//!
//! # Wire framing
//!
//! Per SPEC §2: NUL-delimited JSON, 100MB per-frame cap. See [`framing`].

#![deny(unsafe_op_in_unsafe_fn)]
// SPEC §10: zero `.unwrap()` / `.expect()` in production code.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod connection;
pub mod error;
pub mod framing;
pub mod metrics;
pub mod retry;
pub mod session;
pub mod spawn;

pub use connection::Connection;
pub use error::{CdpError, FramingError, Result};
pub use metrics::{MetricsSink, Outcome};
pub use retry::RetryPolicy;
pub use session::{CdpSession, SessionId};
pub use spawn::{Chromium, ChromiumOptions};

/// Trait implemented by every generated `<Cmd>Params` struct.
///
/// `METHOD` is the CDP wire method name (e.g. `"Page.captureScreenshot"`),
/// `Returns` is the matching `<Cmd>Returns` shape.
///
/// `IDEMPOTENT` declares whether the command is safe to retry on transient
/// transport errors (pipe close, timeout). Defaults to `false`; the codegen
/// in `build.rs` overrides this to `true` for read-only commands whose name
/// starts with `get`, `query`, `describe`, `is`, `has`, or `read` — see
/// [`is_idempotent_method`] in the build script. Override per-call with
/// [`CdpSession::send_with_retry`] when you know better than the heuristic.
pub trait Command: serde::Serialize {
    const METHOD: &'static str;
    /// `true` when retrying on a transient transport error is safe — the
    /// command has no observable side effect on the browser. Defaults to
    /// `false`; codegen sets it for read-only `get*`/`query*`/`describe*`
    /// methods.
    const IDEMPOTENT: bool = false;
    type Returns: serde::de::DeserializeOwned + Send + 'static;
}

/// Generated CDP bindings. The build script writes domain modules into
/// `$OUT_DIR/generated/{domains.rs,events.rs}`.
pub mod generated {
    #[allow(
        clippy::all,
        non_camel_case_types,
        non_snake_case,
        dead_code,
        deprecated,
        unused_imports
    )]
    pub mod domains {
        include!(concat!(env!("OUT_DIR"), "/generated/domains.rs"));
    }
    #[allow(clippy::all, non_camel_case_types, dead_code, deprecated)]
    pub mod events {
        include!(concat!(env!("OUT_DIR"), "/generated/events.rs"));
    }

    pub use events::CdpEvent;
}

/// Re-export the event enum at crate root for convenience.
pub use generated::events::CdpEvent;
