use gpui::{AsyncApp, Task};

pub mod detect;
pub mod ext_workspace;
pub mod sway;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub active: bool,
    pub urgent: bool,
    /// Output (monitor) this workspace belongs to, if known. `None` means
    /// unknown / global (legacy single-output view).
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceState {
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
}

#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    Snapshot(WorkspaceState),
    Focus(WorkspaceId),
    Disconnected,
}

/// Async sender to the workspaces module.
/// Implementations call this from background tasks; the module receives via channel.
pub type EventSink = async_channel::Sender<WorkspaceEvent>;

pub trait WorkspaceBackend: Send + Sync + 'static {
    /// Spawn the backend's main loop on `cx`. The backend pushes `WorkspaceEvent`s
    /// through `sink`. Returns a Task that owns the loop's lifetime.
    fn run(&self, sink: EventSink, cx: &mut AsyncApp) -> Task<()>;

    /// Switch to the given workspace on the specified output. `output=None`
    /// falls back to the backend's idea of "current" output (or is simply
    /// ambiguous on multi-output setups). Best-effort; failures logged but not
    /// propagated.
    fn activate(&self, id: &WorkspaceId, output: Option<&str>);
}
