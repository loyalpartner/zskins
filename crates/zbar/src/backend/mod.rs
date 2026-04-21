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
    /// Register a per-bar sink and spawn (or join) the backend's event loop
    /// on `cx`. Backends push `WorkspaceEvent`s through `sink`.
    ///
    /// The returned `Task` conceptually owns the loop — but backends that
    /// multiplex a single compositor session across many bars (see
    /// `ExtWorkspaceBackend`) start the real work on first call and return a
    /// placeholder Task, so the loop outlives any one caller.
    fn run(&self, sink: EventSink, cx: &mut AsyncApp) -> Task<()>;

    /// Switch to the given workspace on the specified output. `output=None`
    /// falls back to the backend's idea of "current" output (or is simply
    /// ambiguous on multi-output setups). Best-effort; failures logged but not
    /// propagated.
    fn activate(&self, id: &WorkspaceId, output: Option<&str>);
}

/// GPUI derives `PlatformDisplay::uuid()` from the wl_output name using
/// `Uuid::new_v5(NAMESPACE_DNS, name.as_bytes())` — see gpui_linux's
/// `WaylandDisplay::uuid`. We mirror that so backend-side `wl_output.name`
/// values can be matched to GPUI displays across separate connections.
pub fn output_name_uuid(name: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::output_name_uuid;

    #[test]
    fn matches_gpui_uuid_v5_convention() {
        // Regression guard: if GPUI ever changes its UUID derivation, this
        // test keeps working (it exercises our own function) but the live
        // pairing in main.rs will silently break — and we'll know to update
        // this function AND the docstring pointing at gpui_linux.
        let a = output_name_uuid("DP-1");
        let b = output_name_uuid("DP-1");
        let c = output_name_uuid("HDMI-A-1");
        assert_eq!(a, b, "same name must hash to the same UUID");
        assert_ne!(a, c, "different names must hash to different UUIDs");
        // Hand-computed sanity value for "DP-1" under NAMESPACE_DNS.
        let expected = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, b"DP-1");
        assert_eq!(a, expected);
    }
}
