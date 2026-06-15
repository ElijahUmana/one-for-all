//! Retry policy for transient transport errors on idempotent CDP commands.
//!
//! [`Command::IDEMPOTENT`] is set by codegen for read-only commands whose
//! name starts with `get`, `query`, `describe`, `is`, `has`, or `read`. When
//! a session sends one of those via [`CdpSession::send_with_retry`], a
//! transient transport failure ([`CdpError::ConnectionClosed`],
//! [`CdpError::Timeout`]) triggers an exponential-backoff retry capped at
//! [`RetryPolicy::max_attempts`]. Anything that round-trips a wire reply
//! (including [`CdpError::ProtocolError`]) is forwarded immediately —
//! retrying a `-32601 Method not found` would loop forever and a
//! `-32600 Invalid params` won't get fixed by trying again.
//!
//! ## Why not retry every command?
//!
//! Many CDP commands have observable side effects: `Page.navigate`,
//! `Input.dispatchMouseEvent`, `Target.createTarget`. Retrying after a pipe
//! flap could double-fire them. The codegen-driven idempotence flag is
//! conservative; consumers can opt-in per-call when they know better via
//! [`CdpSession::send_with_retry_policy`].
//!
//! [`CdpSession::send_with_retry`]: crate::CdpSession::send_with_retry
//! [`CdpSession::send_with_retry_policy`]: crate::CdpSession::send_with_retry_policy
//! [`Command::IDEMPOTENT`]: crate::Command::IDEMPOTENT
//! [`CdpError::ConnectionClosed`]: crate::CdpError::ConnectionClosed
//! [`CdpError::Timeout`]: crate::CdpError::Timeout
//! [`CdpError::ProtocolError`]: crate::CdpError::ProtocolError

use std::time::Duration;

use crate::error::CdpError;

/// Bounded exponential-backoff retry policy. Cheap to clone (`Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of attempts inclusive of the first send. `1` means
    /// "do not retry"; `3` means "send once, retry up to two more times".
    pub max_attempts: u32,
    /// Backoff before the *second* attempt. Each subsequent attempt doubles
    /// the previous wait, capped at `max_backoff`.
    pub initial_backoff: Duration,
    /// Hard ceiling on backoff between retries.
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// Disabled: do not retry. `max_attempts = 1`. Default for `send`/
    /// `send_with_timeout` so existing call sites don't gain new latency.
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: Duration::from_millis(0),
            max_backoff: Duration::from_millis(0),
        }
    }

    /// Default for `send_with_retry`: 3 attempts, 50ms → 100ms → cap 200ms.
    /// Picked to be invisible on transient pipe flaps but to fail fast when
    /// Chromium has actually died.
    pub const fn default_idempotent() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
        }
    }

    /// Compute the backoff before attempt `n` (1-indexed). Attempt 1 has
    /// zero backoff; attempt 2 uses `initial_backoff`; subsequent attempts
    /// double up to `max_backoff`.
    pub fn backoff_before(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::from_millis(0);
        }
        let exp = attempt.saturating_sub(2);
        let mult = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        let scaled = self
            .initial_backoff
            .checked_mul(mult.min(u32::MAX as u64) as u32)
            .unwrap_or(self.max_backoff);
        scaled.min(self.max_backoff)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Classify whether `err` is worth retrying on. Wire replies — protocol
/// errors, JSON shape errors — are NOT retried because the same input will
/// produce the same output. Transport errors (pipe closed, timeout) MAY be
/// retried because the next attempt rides a fresh frame.
pub(crate) fn is_transient(err: &CdpError) -> bool {
    matches!(
        err,
        CdpError::ConnectionClosed | CdpError::Timeout | CdpError::SessionDetached
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_retries() {
        let p = RetryPolicy::disabled();
        assert_eq!(p.max_attempts, 1);
        assert_eq!(p.backoff_before(1), Duration::ZERO);
        assert_eq!(p.backoff_before(2), Duration::ZERO);
    }

    #[test]
    fn idempotent_default_has_three_attempts_and_caps_backoff() {
        let p = RetryPolicy::default_idempotent();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.backoff_before(1), Duration::ZERO);
        assert_eq!(p.backoff_before(2), Duration::from_millis(50));
        assert_eq!(p.backoff_before(3), Duration::from_millis(100));
        assert_eq!(p.backoff_before(4), Duration::from_millis(200)); // capped
        assert_eq!(p.backoff_before(20), Duration::from_millis(200)); // still capped
    }

    #[test]
    fn transient_classifies_correctly() {
        assert!(is_transient(&CdpError::ConnectionClosed));
        assert!(is_transient(&CdpError::Timeout));
        assert!(is_transient(&CdpError::SessionDetached));
        assert!(!is_transient(&CdpError::ProtocolError {
            code: -32601,
            message: "Method not found".to_owned(),
            data: None,
        }));
        assert!(!is_transient(&CdpError::UnknownId("abc".to_owned())));
    }
}
