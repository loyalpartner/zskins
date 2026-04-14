//! Client wrapper for `wlr-foreign-toplevel-management-unstable-v1`.
//!
//! Phase 1: pure state types + `Registry` aggregation logic. The Wayland
//! transport layer is added in a follow-up commit.

// Registry is consumed by the upcoming Wayland client module; for the
// registry-only commit we silence the dead-code lint rather than exposing
// internals to the public API.
#[allow(dead_code)]
mod registry;

/// Snapshot of a single toplevel's state. Clones are cheap enough for our
/// channel-based fan-out; the struct is small and strings are typically short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toplevel {
    pub id: u64,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub activated: bool,
    pub minimized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToplevelEvent {
    Added(Toplevel),
    Updated(Toplevel),
    Removed(u64),
}
