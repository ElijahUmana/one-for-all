//! Centralized capacity constants for bounded mpsc/broadcast channels.
//!
//! Per N2 work item: every `mpsc::channel(N)` and `broadcast::channel(N)`
//! site in the workspace must reference one of these constants — never a
//! literal — so capacities are tuned in one place and grep-auditable.
//!
//! Sizes are derived from SPEC §10 + §11 N21 backpressure analysis:
//!
//! - [`PER_TARGET_CAPACITY`] — per-CDP-target event fan-out. 1024 entries
//!   absorbs a typical SPA load burst (~600 Network.* events) without
//!   forcing the producer side to await on a slow consumer.
//! - [`NETWORK_OBSERVE_CAP`] — `net.observe` subscription buffer. Bigger
//!   than per-target because one observer can fan in from many tabs.
//! - [`PAGE_LIFECYCLE_CAPACITY`] — Page lifecycle events (load, dom-content,
//!   network-idle). Small; lifecycle events are O(navigations).
//! - [`CONSOLE_CAP`] — per-Page console message broadcast.
//! - [`EXCEPTION_CAP`] — per-Page exception broadcast (bumped from 64 per
//!   N21 because tight error loops can exceed the legacy cap).

/// Per-CDP-target event fan-out capacity (broadcast).
pub const PER_TARGET_CAPACITY: usize = 1024;

/// `net.observe` subscription buffer capacity (broadcast).
pub const NETWORK_OBSERVE_CAP: usize = 4096;

/// Page lifecycle event capacity (mpsc/broadcast).
pub const PAGE_LIFECYCLE_CAPACITY: usize = 64;

/// Per-Page console message broadcast capacity.
pub const CONSOLE_CAP: usize = 512;

/// Per-Page page-exception broadcast capacity (bumped from 64 per N21).
pub const EXCEPTION_CAP: usize = 512;

/// Per-session lifecycle command channel (Connected / Disconnected / Shutdown).
/// Small; one message per state transition, not per CDP event.
pub const SESSION_LIFECYCLE_CAPACITY: usize = 64;

/// SPEC §12 U4 — heap-snapshot drain channel capacity. Used by
/// `browser-engine::perf::heap_snapshot` to buffer
/// `HeapProfiler.addHeapSnapshotChunk` events between the broadcast
/// receiver and the disk writer. A 1024-slot bounded mpsc gives true
/// backpressure (the writer is the only consumer; the producer is the
/// CDP event pump). At ~16 KB per chunk that's ~16 MiB of headroom,
/// well past steady-state writer latency on every disk we ship to.
pub const HEAP_CHUNK_DRAIN_CAPACITY: usize = 1024;

/// SPEC §12 U4 — tracing IO.read chunk size in bytes. Tracing payloads
/// often run hundreds of MiB; pulling them in 64 KiB chunks keeps each
/// CDP round-trip small enough that the broker's 30 s default tool
/// timeout doesn't fire.
pub const TRACING_IO_READ_CHUNK_BYTES: i64 = 65_536;
