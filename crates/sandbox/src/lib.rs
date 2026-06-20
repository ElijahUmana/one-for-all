//! # sandbox — SPEC §11 V3
//!
//! Per-session APFS clone of the user's Chrome profile + arbitrary user dirs
//! into `~/.one-for-all/sessions/<id>/`, plus a generator for the
//! `sandbox-exec` profile (.sb) that confines the spawned Chromium to that
//! rootfs, plus the `ofa merge` driver that promotes selected agent-side
//! changes back to the host on demand.
//!
//! ## What this crate is and is not
//!
//! It **is** the lossless, fast, host-touching primitive that lets N
//! Claude-Code agents start from forks of the user's real Chrome state
//! (cookies, logins, extensions) and a chosen slice of `$HOME` without
//! collisions.
//!
//! It is **not** a full chroot or container — sandbox-exec is process-level
//! confinement, not namespace isolation. Filesystem inheritance via
//! `clonefile(2)` is per-tree, not union-mounted; the merge tool is opt-in
//! and rsync-driven, not a transparent overlay.
//!
//! ## Rejection of shortcuts
//!
//! Per the project quality bar (§12 / §15): this crate does NOT silently
//! degrade. If `clonefile(2)` fails (cross-volume, FileVault-blocked,
//! `ENOTSUP`, etc.) the error type explicitly surfaces the cause; the broker
//! decides whether to fall back to V-R1 (CDP-cookie seeding) or to fail
//! `session.register`. We never copy-with-cp and lie about being O(1).

#![deny(clippy::unwrap_used, clippy::expect_used)]
#![deny(unsafe_op_in_unsafe_fn)]
// Tests deliberately use unwrap()/expect() — they make failures fail
// loudly and are the idiomatic shape for assertions. The deny lint above
// is the production-code rule (§12 quality bar).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod clone;
pub mod errors;
pub mod inherit;
pub mod limits;
pub mod merge;
pub mod portable;
pub mod probe;
pub mod sbpl;
pub mod snapshot;
pub mod spawn;
pub mod v_r1;

pub use clone::{
    clone_chrome_profile, clone_tree, clone_user_dirs, default_chrome_profile_path,
    detect_filevault, CloneStats, FileVaultState,
};
pub use errors::Error;
pub use inherit::{default_allowlist, parse_inherit_keys, InheritMode, InheritSpec};
pub use limits::{CHROMIUM_CPU_SECONDS_SOFT, CHROMIUM_MEMORY_BYTES};
pub use merge::{MergePlan, MergeReport, MergeStrategy};
pub use portable::{default_isolator, Isolator, LinuxStubIsolator, PreparedSandbox};
pub use probe::{probe_sandbox_enforces, ProbeOutcome};
pub use sbpl::{generate_sbpl, write_sbpl, SbplParams};
pub use snapshot::{
    read_snapshot_meta, restore_snapshot, snapshot_root, take_snapshot, SnapshotMeta,
};
pub use spawn::{wrap_argv, SANDBOX_EXEC_PATH};
pub use v_r1::{
    read_host_cookies, read_seed_plan, seed_plan_path, write_seed_plan, CacheHeader,
    CacheStorageEntry, CookieRecord, IndexedDbRecord, SeedPlan, ServiceWorkerReg, StorageEntry,
};

/// Convenience alias used across the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
