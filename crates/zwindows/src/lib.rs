//! Client wrapper for `wlr-foreign-toplevel-management-unstable-v1`.
//!
//! Surfaces an event stream of open windows (`ToplevelEvent`) and a map of
//! handles for activation. The protocol plumbing is isolated in `client`; the
//! state-aggregation logic lives in `registry` and is pure / unit-tested.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

mod client;
mod registry;

pub use client::{spawn, ToplevelHandle};

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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland global error: {0}")]
    Global(#[from] wayland_client::globals::GlobalError),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("failed to bind zwlr_foreign_toplevel_manager_v1: {0}")]
    BindManager(wayland_client::globals::BindError),
    #[error("no wl_seat available; activation requires a seat")]
    NoSeat,
    #[error("wayland backend: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
}

/// Live view of foreign toplevels plus a way to control them.
///
/// `events` is the only way to learn about changes; `handles` is the only way
/// to act on them. Keeping them separate lets callers park activation calls
/// behind a lock while draining events on another task.
pub struct Client {
    pub events: async_channel::Receiver<ToplevelEvent>,
    pub handles: Arc<RwLock<HashMap<u64, ToplevelHandle>>>,
}
