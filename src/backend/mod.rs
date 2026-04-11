#![allow(dead_code)]

use gpui::{AsyncApp, Task};

pub mod detect;
pub mod sway;
pub mod ext_workspace;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub active: bool,
    pub urgent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceState {
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
}

#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    /// Full state replacement (initial snapshot or after any update).
    Snapshot(WorkspaceState),
    /// Backend disconnected; module should clear UI and wait for reconnect.
    Disconnected,
}

/// Async sender to the workspaces module.
/// Implementations call this from background tasks; the module receives via channel.
pub type EventSink = std::sync::mpsc::Sender<WorkspaceEvent>;

pub trait WorkspaceBackend: Send + 'static {
    /// Spawn the backend's main loop on `cx`. The backend pushes `WorkspaceEvent`s
    /// through `sink`. Returns a Task that owns the loop's lifetime.
    fn run(self: Box<Self>, sink: EventSink, cx: &mut AsyncApp) -> Task<()>;

    /// Switch to the given workspace. Best-effort; failures logged but not propagated.
    fn activate(&self, id: &WorkspaceId);
}
