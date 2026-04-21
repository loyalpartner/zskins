use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use gpui::{AsyncApp, Task};
use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalList, GlobalListContents},
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WsState},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::backend::{
    EventSink, Workspace, WorkspaceBackend, WorkspaceEvent, WorkspaceId, WorkspaceState,
};

#[derive(Debug, thiserror::Error)]
pub enum ExtWorkspaceError {
    #[error("wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland global error: {0}")]
    Global(#[from] wayland_client::globals::GlobalError),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("failed to bind ext_workspace_manager_v1: {0}")]
    BindManager(wayland_client::globals::BindError),
    #[error("wayland protocol: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, ExtWorkspaceError>;

#[derive(Default, Clone)]
struct WsBuilder {
    name: Option<String>,
    active: bool,
    urgent: bool,
    /// protocol_id of the ExtWorkspaceGroupHandleV1 this workspace currently
    /// belongs to (set via group.workspace_enter / cleared via workspace_leave).
    group_pid: Option<u32>,
}

struct GroupState {
    outputs: HashSet<u32>,
    workspaces: HashSet<u32>,
}

impl GroupState {
    fn new() -> Self {
        GroupState {
            outputs: HashSet::new(),
            workspaces: HashSet::new(),
        }
    }
}

struct Shared {
    /// (workspace_name, output_name) → handle. `output_name` is `None` only
    /// when we couldn't determine one (best-effort fallback).
    by_key: Mutex<HashMap<(String, Option<String>), ExtWorkspaceHandleV1>>,
    session: Mutex<Option<SessionHandles>>,
    /// Per-bar event sinks. The single session broadcasts to all of them so
    /// each bar sees the same workspace state without each spawning its own
    /// wayland connection (which would desynchronize handle ownership).
    sinks: Mutex<Vec<EventSink>>,
    /// Last emitted state — replayed to newly-subscribed sinks so a late bar
    /// doesn't sit empty until the next compositor event.
    last_snapshot: Mutex<Option<WorkspaceState>>,
    started: std::sync::Once,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            by_key: Mutex::new(HashMap::new()),
            session: Mutex::new(None),
            sinks: Mutex::new(Vec::new()),
            last_snapshot: Mutex::new(None),
            started: std::sync::Once::new(),
        }
    }
}

struct SessionHandles {
    manager: ExtWorkspaceManagerV1,
    conn: Connection,
}

struct AppState {
    /// workspace protocol_id → (handle, state)
    workspaces: HashMap<u32, (ExtWorkspaceHandleV1, WsBuilder)>,
    /// group protocol_id → group membership state
    groups: HashMap<u32, GroupState>,
    /// wl_output protocol_id → name (from wl_output.name event).
    outputs: HashMap<u32, String>,
    shared: Arc<Shared>,
}

impl AppState {
    fn output_name_for_group(&self, group_pid: u32) -> Option<String> {
        let group = self.groups.get(&group_pid)?;
        // Pick the first output we can resolve a name for.
        group
            .outputs
            .iter()
            .find_map(|out_pid| self.outputs.get(out_pid).cloned())
    }

    fn flush_state(&mut self) {
        // Sort by protocol_id so output is a deterministic function of state.
        let mut entries: Vec<(u32, &ExtWorkspaceHandleV1, &WsBuilder)> = self
            .workspaces
            .iter()
            .map(|(pid, (handle, b))| (*pid, handle, b))
            .collect();
        entries.sort_by_key(|(pid, _, _)| *pid);

        let mut by_key: HashMap<(String, Option<String>), ExtWorkspaceHandleV1> = HashMap::new();
        let mut workspaces: Vec<Workspace> = Vec::new();
        for (_pid, handle, b) in &entries {
            let Some(name) = b.name.as_ref().cloned() else {
                continue;
            };
            let output = b.group_pid.and_then(|gp| self.output_name_for_group(gp));
            let key = (name.clone(), output.clone());
            by_key.insert(key, (*handle).clone());
            workspaces.push(Workspace {
                id: WorkspaceId(name.clone()),
                name,
                active: b.active,
                urgent: b.urgent,
                output,
            });
        }

        // Sort primarily by output then by name for stable display order.
        workspaces.sort_by(|a, b| a.output.cmp(&b.output).then_with(|| a.name.cmp(&b.name)));

        let active = workspaces.iter().find(|w| w.active).map(|w| w.id.clone());
        let state = WorkspaceState { workspaces, active };

        // Handle map is always refreshed — handles are only compared by
        // identity, so `WorkspaceState` equality doesn't cover them.
        *self.shared.by_key.lock().unwrap() = by_key;

        // niri emits Done on every focus tick; suppress identical back-to-back
        // snapshots so downstream bars don't re-render on no-op updates.
        {
            let mut last = self.shared.last_snapshot.lock().unwrap();
            if last.as_ref() == Some(&state) {
                return;
            }
            *last = Some(state.clone());
        }
        tracing::debug!(
            "snapshot: {} workspaces; active={:?}",
            state.workspaces.len(),
            state.active
        );
        self.shared.broadcast(WorkspaceEvent::Snapshot(state));
    }
}

impl Shared {
    fn broadcast(&self, ev: WorkspaceEvent) {
        // Copy out first; `send_blocking` may block on a slow consumer and we
        // don't want to stall sink registration or other broadcasts behind it.
        let sinks: Vec<EventSink> = self.sinks.lock().unwrap().clone();
        for sink in &sinks {
            let _ = sink.send_blocking(ev.clone());
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            let pid = output.id().protocol_id();
            tracing::debug!("wl_output {pid} name = {name}");
            state.outputs.insert(pid, name);
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for AppState {
    event_created_child!(AppState, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                let id = workspace.id().protocol_id();
                state
                    .workspaces
                    .insert(id, (workspace, WsBuilder::default()));
            }
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                let id = workspace_group.id().protocol_id();
                state.groups.insert(id, GroupState::new());
            }
            ext_workspace_manager_v1::Event::Done => {
                tracing::debug!(
                    "ext-ws Done; workspaces={} groups={}",
                    state.workspaces.len(),
                    state.groups.len()
                );
                state.flush_state();
            }
            ext_workspace_manager_v1::Event::Finished => {
                tracing::info!("ext-workspace manager finished");
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let group_pid = group.id().protocol_id();
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                let out_pid = output.id().protocol_id();
                state
                    .groups
                    .entry(group_pid)
                    .or_insert_with(GroupState::new)
                    .outputs
                    .insert(out_pid);
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                let out_pid = output.id().protocol_id();
                if let Some(g) = state.groups.get_mut(&group_pid) {
                    g.outputs.remove(&out_pid);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                let ws_pid = workspace.id().protocol_id();
                state
                    .groups
                    .entry(group_pid)
                    .or_insert_with(GroupState::new)
                    .workspaces
                    .insert(ws_pid);
                if let Some((_, builder)) = state.workspaces.get_mut(&ws_pid) {
                    builder.group_pid = Some(group_pid);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                let ws_pid = workspace.id().protocol_id();
                if let Some(g) = state.groups.get_mut(&group_pid) {
                    g.workspaces.remove(&ws_pid);
                }
                if let Some((_, builder)) = state.workspaces.get_mut(&ws_pid) {
                    if builder.group_pid == Some(group_pid) {
                        builder.group_pid = None;
                    }
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.groups.remove(&group_pid);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id().protocol_id();
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some((_, builder)) = state.workspaces.get_mut(&id) {
                    builder.name = Some(name);
                }
            }
            ext_workspace_handle_v1::Event::Coordinates { .. } => {}
            ext_workspace_handle_v1::Event::State { state: s } => {
                if let Some((_, builder)) = state.workspaces.get_mut(&id) {
                    let bits: WsState = match s {
                        WEnum::Value(v) => v,
                        WEnum::Unknown(_) => WsState::empty(),
                    };
                    builder.active = bits.contains(WsState::Active);
                    builder.urgent = bits.contains(WsState::Urgent);
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&id);
            }
            _ => {}
        }
    }
}

pub struct ExtWorkspaceBackend {
    shared: Arc<Shared>,
}

impl Default for ExtWorkspaceBackend {
    fn default() -> Self {
        ExtWorkspaceBackend {
            shared: Arc::new(Shared::default()),
        }
    }
}

impl ExtWorkspaceBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether the running compositor advertises ext_workspace_manager_v1.
    pub fn probe() -> bool {
        let Ok(conn) = Connection::connect_to_env() else {
            return false;
        };
        let Ok((globals, _queue)) = registry_queue_init::<ProbeState>(&conn) else {
            return false;
        };
        globals.contents().with_list(|list| {
            list.iter()
                .any(|g| g.interface == "ext_workspace_manager_v1")
        })
    }
}

struct ProbeState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ProbeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Enumerate every wl_output advertised by the compositor and return
/// `(name, logical_width)` pairs in registry order. Names come from the
/// `wl_output.name` event (v4+); logical width is taken from the Mode event.
///
/// Bars use this to discover their output name before any workspace data
/// arrives, which lets the ext-workspace filter work on the first render.
pub fn query_wayland_outputs() -> Vec<(String, f32)> {
    let Ok(conn) = Connection::connect_to_env() else {
        return Vec::new();
    };
    let Ok((globals, mut event_queue)) = registry_queue_init::<OutputProbeState>(&conn) else {
        return Vec::new();
    };
    let qh = event_queue.handle();

    let mut state = OutputProbeState::default();

    globals.contents().with_list(|list| {
        for g in list {
            if g.interface == "wl_output" {
                let version = g.version.min(4);
                let _output = globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, OutputProbeState>(g.name, version, &qh, g.name);
                state.entries.push(OutputProbeEntry::new(g.name));
            }
        }
    });

    // Two roundtrips: one for binds, one for name/mode events to arrive.
    for _ in 0..2 {
        if conn.roundtrip().is_err() {
            break;
        }
        let _ = event_queue.dispatch_pending(&mut state);
    }

    state
        .entries
        .into_iter()
        .filter_map(|e| e.name.map(|n| (n, e.width)))
        .collect()
}

#[derive(Default)]
struct OutputProbeState {
    entries: Vec<OutputProbeEntry>,
}

struct OutputProbeEntry {
    registry_name: u32,
    name: Option<String>,
    width: f32,
}

impl OutputProbeEntry {
    fn new(registry_name: u32) -> Self {
        OutputProbeEntry {
            registry_name,
            name: None,
            width: 0.0,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for OutputProbeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for OutputProbeState {
    fn event(
        state: &mut Self,
        _output: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let registry_name = *data;
        let Some(entry) = state
            .entries
            .iter_mut()
            .find(|e| e.registry_name == registry_name)
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = Some(name),
            wl_output::Event::Mode {
                width,
                flags: WEnum::Value(v),
                ..
            } if v.contains(wl_output::Mode::Current) => {
                entry.width = width as f32;
            }
            _ => {}
        }
    }
}

impl WorkspaceBackend for ExtWorkspaceBackend {
    /// Register a per-bar sink and, on the first call, spawn the single
    /// shared wayland session. The returned Task is a no-op placeholder — the
    /// real session runs detached for the lifetime of the process, because
    /// multiple bars each hold a sink and we can't tie the session to any one
    /// of them. (The trait doc says the Task owns the loop's lifetime; here
    /// the lifetime is effectively global.)
    fn run(&self, sink: EventSink, cx: &mut AsyncApp) -> Task<()> {
        let shared = self.shared.clone();
        shared.sinks.lock().unwrap().push(sink.clone());
        if let Some(last) = shared.last_snapshot.lock().unwrap().clone() {
            let _ = sink.send_blocking(WorkspaceEvent::Snapshot(last));
        }

        let spawn_shared = shared.clone();
        shared.started.call_once(|| {
            cx.background_executor()
                .spawn(async move {
                    let mut delay_ms: u64 = 1000;
                    loop {
                        match run_session(spawn_shared.clone()) {
                            Ok(()) => tracing::info!("ext-workspace session ended cleanly"),
                            Err(e) => tracing::warn!(
                                "ext-workspace session error: {e:#}; reconnecting in {delay_ms}ms"
                            ),
                        }
                        *spawn_shared.session.lock().unwrap() = None;
                        spawn_shared.by_key.lock().unwrap().clear();
                        *spawn_shared.last_snapshot.lock().unwrap() = None;
                        spawn_shared.broadcast(WorkspaceEvent::Disconnected);
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        delay_ms = (delay_ms * 2).min(30_000);
                    }
                })
                .detach();
        });

        cx.background_executor().spawn(async move {})
    }

    fn activate(&self, id: &WorkspaceId, output: Option<&str>) {
        let by_key = self.shared.by_key.lock().unwrap();
        let handle = by_key
            .get(&(id.0.clone(), output.map(|s| s.to_string())))
            .cloned()
            // Fallback: if the caller didn't supply an output, or the exact
            // (name, output) key is missing, try any workspace with that name.
            .or_else(|| {
                by_key
                    .iter()
                    .find(|((name, _), _)| name == &id.0)
                    .map(|(_, h)| h.clone())
            });
        let Some(handle) = handle else {
            let known: Vec<&(String, Option<String>)> = by_key.keys().collect();
            tracing::warn!(
                "activate: workspace name='{}' output={:?} not found; known={known:?}",
                id.0,
                output
            );
            return;
        };
        drop(by_key);
        let session = self.shared.session.lock().unwrap();
        let Some(s) = session.as_ref() else {
            tracing::warn!("activate: no active ext-workspace session");
            return;
        };
        let proxy_id = handle.id().protocol_id();
        if !handle.is_alive() {
            tracing::warn!(
                "activate: workspace '{}' handle is dead (proxy_id={})",
                id.0,
                proxy_id
            );
            return;
        }
        tracing::debug!(
            "activate: name='{}' output={:?} proxy_id={}",
            id.0,
            output,
            proxy_id
        );
        handle.activate();
        s.manager.commit();
        if let Err(e) = s.conn.flush() {
            tracing::warn!("activate: flush failed: {e}");
        }
    }
}

fn bind_all_outputs(globals: &GlobalList, qh: &QueueHandle<AppState>) -> Vec<wl_output::WlOutput> {
    globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| {
                // wl_output v4 introduces the `name` event. Bind whatever
                // version the compositor advertises, up to v4.
                let version = g.version.min(4);
                globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, AppState>(g.name, version, qh, g.name)
            })
            .collect::<Vec<_>>()
    })
}

fn run_session(shared: Arc<Shared>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue): (_, EventQueue<AppState>) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let manager: ExtWorkspaceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(ExtWorkspaceError::BindManager)?;

    // Keep wl_output proxies alive so we continue to receive their name events.
    let _outputs = bind_all_outputs(&globals, &qh);

    *shared.session.lock().unwrap() = Some(SessionHandles {
        manager,
        conn: conn.clone(),
    });

    let mut state = AppState {
        workspaces: HashMap::new(),
        groups: HashMap::new(),
        outputs: HashMap::new(),
        shared,
    };

    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}
