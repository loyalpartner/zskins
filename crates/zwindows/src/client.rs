//! Wayland connection + dispatcher for wlr-foreign-toplevel-management-v1.
//!
//! A single background thread owns the connection, event queue, and
//! `Mutex<Registry>`. All handle protocol objects are smuggled back to the
//! main thread via `Client.handles` so callers can `activate()` without
//! touching the event loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, State as HState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::registry::Registry;
use crate::{Client, ToplevelEvent};

/// Handle bundle that lets callers request actions on a specific toplevel.
///
/// Cloning `wayland-client` proxies is cheap (ref-counted) — we keep a
/// snapshot per-id rather than forcing callers to look up via the event loop.
pub struct ToplevelHandle {
    handle: ZwlrForeignToplevelHandleV1,
    seat: WlSeat,
    conn: Connection,
}

impl ToplevelHandle {
    /// Focus this toplevel. Unminimizes first — activating a minimized window
    /// on wlroots does not restore it by itself, which would confuse users
    /// who just clicked their launcher.
    pub fn activate(&self) {
        self.handle.unset_minimized();
        self.handle.activate(&self.seat);
        // Flush here so clicks feel immediate instead of being batched with
        // the next incoming event.
        let _ = self.conn.flush();
    }
}

/// Start the Wayland thread. Returns `None` if the environment has no
/// Wayland socket or the compositor does not advertise the manager global.
pub fn spawn() -> Option<Client> {
    let (tx, rx) = async_channel::bounded::<ToplevelEvent>(256);
    let handles: Arc<RwLock<HashMap<u64, ToplevelHandle>>> = Arc::new(RwLock::new(HashMap::new()));

    let handles_for_thread = handles.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<bool>(1);

    std::thread::Builder::new()
        .name("zwindows".into())
        .spawn(move || {
            match run(tx, handles_for_thread, ready_tx.clone()) {
                Ok(()) => tracing::info!("zwindows session ended"),
                Err(e) => tracing::warn!("zwindows session error: {e}"),
            }
            // If run() failed before announcing readiness, unblock spawn().
            let _ = ready_tx.try_send(false);
        })
        .ok()?;

    match ready_rx.recv() {
        Ok(true) => Some(Client {
            events: rx,
            handles,
        }),
        _ => None,
    }
}

struct AppState {
    registry: Arc<Mutex<Registry>>,
    tx: async_channel::Sender<ToplevelEvent>,
    handles: Arc<RwLock<HashMap<u64, ToplevelHandle>>>,
    seat: WlSeat,
    conn: Connection,
}

impl AppState {
    fn emit(&self, ev: Option<ToplevelEvent>) {
        if let Some(ev) = ev {
            // send_blocking inside a bounded channel applies back-pressure
            // on the event loop, which is what we want: dropping events
            // would desynchronize consumer state from the compositor.
            let _ = self.tx.send_blocking(ev);
        }
    }
}

fn run(
    tx: async_channel::Sender<ToplevelEvent>,
    handles: Arc<RwLock<HashMap<u64, ToplevelHandle>>>,
    ready: std::sync::mpsc::SyncSender<bool>,
) -> Result<(), crate::Error> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue): (_, EventQueue<AppState>) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    // Bind a seat first — activate() requires one; no seat means we can't do
    // our main job, so fail early rather than surface broken handles.
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|_| crate::Error::NoSeat)?;

    let _manager: ZwlrForeignToplevelManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(crate::Error::BindManager)?;

    let mut state = AppState {
        registry: Arc::new(Mutex::new(Registry::new())),
        tx,
        handles,
        seat,
        conn: conn.clone(),
    };

    // Signal successful bind to spawn() before entering the dispatch loop —
    // otherwise callers would race against the first event.
    let _ = ready.send(true);

    loop {
        event_queue.blocking_dispatch(&mut state)?;
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

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                let id = toplevel.id().protocol_id() as u64;
                state.registry.lock().unwrap().ensure(id);
                // Store the handle bundle so callers can activate later
                // without knowing about the event loop.
                state.handles.write().unwrap().insert(
                    id,
                    ToplevelHandle {
                        handle: toplevel,
                        seat: state.seat.clone(),
                        conn: state.conn.clone(),
                    },
                );
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                tracing::info!("foreign-toplevel manager finished");
            }
            _ => {}
        }
    }

    // The manager's `toplevel` event creates a new `ZwlrForeignToplevelHandleV1`
    // child. wayland-client's default `event_created_child` panics to force us
    // to specify what user data to attach to the child proxy.
    wayland_client::event_created_child!(AppState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id().protocol_id() as u64;
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                let ev = state.registry.lock().unwrap().on_app_id(id, app_id);
                state.emit(ev);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                let ev = state.registry.lock().unwrap().on_title(id, title);
                state.emit(ev);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => {
                // State comes as a raw byte array of u32 entries — decode
                // manually rather than depending on a higher-level helper,
                // keeping us independent of wayland-client version quirks.
                let (activated, minimized) = decode_state(&bytes);
                let ev = state
                    .registry
                    .lock()
                    .unwrap()
                    .on_state(id, activated, minimized);
                state.emit(ev);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                let ev = state.registry.lock().unwrap().on_closed(id);
                state.handles.write().unwrap().remove(&id);
                state.emit(ev);
            }
            // title/app_id/state already fire; `done` is a batching marker
            // we don't need since our Registry already deduplicates per-field.
            _ => {}
        }
    }
}

fn decode_state(raw: &[u8]) -> (bool, bool) {
    // wire format: little-endian u32 entries from the state enum.
    let mut activated = false;
    let mut minimized = false;
    for chunk in raw.chunks_exact(4) {
        let value = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        match value {
            v if v == u32::from(HState::Activated) => activated = true,
            v if v == u32::from(HState::Minimized) => minimized = true,
            _ => {}
        }
    }
    (activated, minimized)
}
