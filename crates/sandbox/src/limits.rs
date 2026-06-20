//! Single source of truth for resource limits applied to spawned Chromium.
//!
//! These constants are referenced from BOTH:
//! - `browser-engine`'s `pre_exec` hook, where they're applied via
//!   `setrlimit(2)` between fork and exec (SPEC §10 M9 — process-local
//!   memory cap so a runaway tab cannot OOM the host).
//! - The SBPL profile generator in this crate, which annotates the emitted
//!   `.sb` file with the same numbers so a reviewer auditing the sandbox
//!   profile sees the in-process cap that's also being enforced. SBPL has
//!   no native `mem-limit` clause — sandbox-exec memory enforcement is
//!   layered via `setrlimit` in `pre_exec`, which already runs alongside
//!   the sandbox-exec wrap. The pairing here is "one constant, two
//!   enforcement layers in the same fork": rlimit (kernel-imposed) plus
//!   the audit annotation in the profile.
//!
//! Co-locating these here also lets a future contributor change the cap
//! without missing a call site — there is exactly ONE call site per crate
//! and both refer back to this module.

/// Process-wide virtual-memory cap for spawned Chromium, in bytes.
///
/// 4 GiB is the SPEC §10 M9 ceiling: enough for any realistic Chromium
/// workload (multiple GPU + utility + renderer subprocesses, each well
/// under 1 GiB resident on this codebase's headless preset) and small
/// enough that a runaway page (cryptojacker, infinite-loop-allocator,
/// memory-pressure attack from a hostile site) is killed by the kernel
/// long before the host machine pages.
///
/// This is a HARD cap (`rlim_cur == rlim_max`) so the child process
/// cannot lift it via `setrlimit(2)`. The kernel rejects allocations
/// past this with `ENOMEM`, which Chromium handles by killing the
/// offending renderer.
pub const CHROMIUM_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Soft CPU-time cap, in seconds. Hits `SIGXCPU` when reached; the child
/// can install a handler if it cares (Chromium does not, by design).
///
/// One hour is comfortably above the longest realistic interactive
/// session and well below "this process has been pegging a core all day,
/// kill it." Hard limit is `RLIM_INFINITY` so `SIGXCPU` is delivered
/// once and the child can finish a graceful shutdown if it chooses.
pub const CHROMIUM_CPU_SECONDS_SOFT: u64 = 3600;

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the SPEC §10 M9 numbers. If a future change touches these,
    /// the change MUST also update the SPEC and the corresponding pre_exec
    /// audit comment in `browser-engine/src/browser.rs`.
    #[test]
    fn chromium_memory_bytes_is_4gib() {
        assert_eq!(CHROMIUM_MEMORY_BYTES, 4u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn chromium_cpu_seconds_is_one_hour() {
        assert_eq!(CHROMIUM_CPU_SECONDS_SOFT, 3600);
    }

    /// Sanity check: 4 GiB fits in the kernel's `rlim_t`. On every
    /// POSIX target Rust currently supports, `rlim_t` is at least
    /// `u64`-shaped (32-bit targets are not supported by this crate).
    /// This test is a tripwire for an unlikely future port.
    #[test]
    fn chromium_memory_bytes_fits_rlim_t() {
        let _: libc::rlim_t = CHROMIUM_MEMORY_BYTES as libc::rlim_t;
    }
}
