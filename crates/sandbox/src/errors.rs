//! Error types for the sandbox crate.
//!
//! Each variant maps to a specific OS-observable failure mode so callers
//! (the broker) can decide the right fallback path — V-R1 cookie-seeding
//! versus `-32008 ChromiumLaunchFailed` versus a hard `session.register`
//! refusal. Nothing is collapsed into `anyhow::Error`; that would be exactly
//! the silent-degradation pattern §12 forbids.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("clonefile(2) hit EDEADLK and exhausted {attempts} retries: {src} -> {dst}")]
    CloneRetriesExhausted {
        src: PathBuf,
        dst: PathBuf,
        attempts: usize,
    },

    #[error("clonefile(2) is not supported on this filesystem (cross-volume or non-APFS): {src} -> {dst}")]
    CloneUnsupported { src: PathBuf, dst: PathBuf },

    #[error("clonefile(2) is unavailable on this OS (sandbox crate requires macOS 10.12+)")]
    CloneUnavailableOnPlatform,

    #[error("destination already exists; refusing to overwrite: {0}")]
    DestinationExists(PathBuf),

    #[error("source path does not exist: {0}")]
    SourceMissing(PathBuf),

    #[error("FileVault encrypted volume blocks clonefile(2) for {0}; falling back to V-R1 cookie seeding")]
    FileVaultBlocked(PathBuf),

    #[error("`/usr/bin/sandbox-exec` not found or not executable on this host")]
    SandboxExecMissing,

    #[error("home directory could not be resolved")]
    HomeDirUnresolvable,

    #[error("rsync failed: {0}")]
    RsyncFailed(String),

    #[error("`fdesetup status` produced output the parser could not interpret: {0:?}")]
    FdesetupParseFailure(String),

    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    BareIo(#[from] std::io::Error),

    #[error("invalid inherit key: {0}")]
    InvalidInheritKey(String),
}

impl Error {
    /// Convenience constructor that attaches a path to a raw `io::Error`.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
