//! Error types for `cdp-client`.
//!
//! Owned by `cdp-client`. `thiserror`-based; surfaced through every public
//! API. Inspectable variants let callers map to the SPEC §2 wire codes
//! without parsing strings.

use thiserror::Error;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CdpError>;

/// All errors the CDP client can produce.
#[derive(Debug, Error)]
pub enum CdpError {
    /// Failed to spawn or set up the Chromium child process.
    #[error("spawn failure: {0}")]
    Spawn(#[from] std::io::Error),

    /// Wire framing problem (NUL-delimited JSON over the pipe).
    #[error("framing: {0}")]
    Framing(#[from] FramingError),

    /// JSON ser/de problem.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Chromium reported an error in response to a command.
    #[error("protocol error {code}: {message}")]
    ProtocolError {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// The session was detached before the in-flight command completed.
    #[error("session detached")]
    SessionDetached,

    /// The connection (browser or session) was closed before the in-flight
    /// command completed.
    #[error("connection closed")]
    ConnectionClosed,

    /// Operation exceeded its deadline.
    #[error("timeout")]
    Timeout,

    /// Caller-supplied identifier (sessionId, targetId) is unknown.
    #[error("unknown id: {0}")]
    UnknownId(String),

    /// Catch-all for invariant violations not worth a dedicated variant.
    #[error("internal: {0}")]
    Internal(String),
}

/// Errors produced by the framing codec.
#[derive(Debug, Error)]
pub enum FramingError {
    /// The decoder buffered more than the configured cap (default 100MB)
    /// without seeing a NUL terminator.
    #[error("frame exceeds {limit} bytes without NUL terminator")]
    FrameTooLarge { limit: usize },

    /// Underlying I/O failed while reading or writing the pipe.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Inner JSON parse error after a frame was successfully de-NUL'd.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Pipe closed cleanly with no trailing NUL — only an error if we were
    /// mid-frame.
    #[error("pipe EOF mid-frame")]
    UnexpectedEof,
}
