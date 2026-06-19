//! Server→client event bus.
//!
//! `EventBus` is a topic-keyed broadcast: any subscriber on a given topic
//! receives the event. Used so that components like `browser-engine` can
//! emit page-lifecycle / network events without knowing which session they
//! belong to — the broker layer maps `(session_id, topic)` → connected MCP.

use std::sync::Arc;

use crate::protocol::{ServerEvent, VisionFrameEvent};

/// Outbound items the per-conn writer task drains.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// Reply to a request the client made. (Synchronous half — handled in
    /// `router.rs`; this variant is here for future event-pull tools.)
    Reply(crate::protocol::JsonRpcResponse),
    /// Server-pushed notification (`event/notify`, SPEC §2.6). JSON-encoded.
    Notify(ServerEvent),
    /// SPEC §11 V5 — binary `vision.frame` notification. The writer task
    /// serializes this with bincode and prefixes the magic byte `0x01`,
    /// instead of JSON. mcp-server decodes the bincode envelope and
    /// re-emits as JSON over the MCP stdio (LSP) transport.
    VisionFrame(VisionFrameEvent),
}

impl ClientEvent {
    /// Helper for the writer task hot-path: returns true iff this event
    /// should take the bincode binary-topic path.
    pub fn is_vision_frame(&self) -> bool {
        matches!(self, ClientEvent::VisionFrame(_))
    }
}

/// Trivial type-tag so EventBus can stay generic. Currently a marker.
pub struct EventBus {
    /// Reserved for future use (broadcast tx by topic). For v1 every
    /// event flows directly through `SessionEntry::try_push`, so the bus is
    /// just an Arc handle that other modules can hold.
    _marker: (),
}

impl EventBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { _marker: () })
    }
}
