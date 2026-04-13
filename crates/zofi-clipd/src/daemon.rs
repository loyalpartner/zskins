//! Unified clipboard daemon: watches selection events, serves the current
//! selection on demand (when zofi activates an entry), and ignores the events
//! triggered by its own `set_selection` calls.
//!
//! Single thread, manual poll() multiplex over the wayland fd and the IPC
//! listener fd. No tokio, no calloop, no extra threads.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use wayland_client::backend::ObjectId;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::db::{Db, DbError};
use crate::ipc::{self, Request, Response};
use crate::model::{Kind, MimeContent};
use crate::paths;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("connect to wayland: {0}")]
    WaylandConnect(#[source] wayland_client::ConnectError),
    #[error("wayland registry init: {0}")]
    RegistryInit(#[source] wayland_client::globals::GlobalError),
    #[error("compositor missing global `{name}`: {source}")]
    MissingGlobal {
        name: &'static str,
        #[source]
        source: wayland_client::globals::BindError,
    },
    #[error("bind {sock}: {source}")]
    BindSocket {
        sock: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("wayland: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("wayland queue still has pending events")]
    PendingEvents,
}

#[derive(Debug, thiserror::Error)]
enum OfferError {
    #[error("no acceptable mime in {0:?}")]
    NoMime(Vec<String>),
    #[error("empty content for {0}")]
    EmptyContent(String),
    #[error("content exceeds {0} byte cap")]
    TooLarge(usize),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("pipe: {0}")]
    Pipe(#[from] nix::Error),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("db: {0}")]
    Db(#[from] DbError),
}

const TEXT_CAP_BYTES: usize = 1_000_000;
const IMAGE_CAP_BYTES: usize = 10_000_000;
const DEFAULT_RING_SIZE: usize = 500;

struct State {
    /// Mimes advertised by an offer.
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    /// Latest selection offer waiting to be drained.
    pending: Option<ZwlrDataControlOfferV1>,

    /// Source we're currently holding (set by activate). When the compositor
    /// asks us to serve content, we write `held_content` to the fd.
    held_source: Option<ZwlrDataControlSourceV1>,
    held_mime: String,
    held_content: Vec<u8>,
    /// Content hash of what we just set as selection. Skip the next watcher
    /// event if its content hashes to the same value.
    expect_self_hash: Option<[u8; 32]>,
}

impl State {
    fn new() -> Self {
        Self {
            offer_mimes: HashMap::new(),
            pending: None,
            held_source: None,
            held_mime: String::new(),
            held_content: Vec::new(),
            expect_self_hash: None,
        }
    }
}

pub fn run(db: Db) -> Result<(), DaemonError> {
    let conn = Connection::connect_to_env().map_err(DaemonError::WaylandConnect)?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(DaemonError::RegistryInit)?;
    let qh = queue.handle();

    let seat: wl_seat::WlSeat =
        globals
            .bind(&qh, 1..=9, ())
            .map_err(|source| DaemonError::MissingGlobal {
                name: "wl_seat",
                source,
            })?;
    let manager: ZwlrDataControlManagerV1 =
        globals
            .bind(&qh, 1..=2, ())
            .map_err(|source| DaemonError::MissingGlobal {
                name: "zwlr_data_control_manager_v1",
                source,
            })?;
    let device = manager.get_data_device(&seat, &qh, ());

    let sock_path = paths::sock_path();
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&sock_path).map_err(|source| DaemonError::BindSocket {
        sock: sock_path.clone(),
        source,
    })?;
    listener.set_nonblocking(true)?;

    let mut state = State::new();
    tracing::info!("clipboard daemon ready (sock={sock_path:?})");

    loop {
        queue.dispatch_pending(&mut state)?;

        if let Some(offer) = state.pending.take() {
            if let Err(e) = process_offer(&offer, &mut state, &db, &mut queue) {
                tracing::warn!("drop selection: {e}");
            }
            state.offer_mimes.remove(&offer.id());
            offer.destroy();
            if let Err(e) = db.prune(DEFAULT_RING_SIZE) {
                tracing::warn!("prune failed: {e}");
            }
        }

        queue.flush()?;

        let read_guard = queue.prepare_read().ok_or(DaemonError::PendingEvents)?;
        let wl_fd = read_guard.connection_fd();
        let listener_fd = listener.as_fd();

        let (wl_ready, ipc_ready) = poll_two(wl_fd, listener_fd)?;

        if wl_ready {
            // Pull bytes off the wayland socket.
            if let Err(e) = read_guard.read() {
                tracing::warn!("wayland read: {e}");
            }
        } else {
            drop(read_guard);
        }

        if ipc_ready {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => handle_ipc(stream, &mut state, &db, &device, &manager, &qh),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("ipc accept: {e}");
                        break;
                    }
                }
            }
        }
    }
}

fn poll_two(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> Result<(bool, bool), std::io::Error> {
    let mut fds = [
        PollFd::new(a, PollFlags::POLLIN),
        PollFd::new(b, PollFlags::POLLIN),
    ];
    poll(&mut fds, PollTimeout::NONE)?;
    let ready = |i: usize| {
        fds[i]
            .revents()
            .map(|f: PollFlags| f.intersects(PollFlags::POLLIN))
            .unwrap_or(false)
    };
    Ok((ready(0), ready(1)))
}

fn handle_ipc(
    mut stream: UnixStream,
    state: &mut State,
    db: &Db,
    device: &ZwlrDataControlDeviceV1,
    manager: &ZwlrDataControlManagerV1,
    qh: &QueueHandle<State>,
) {
    stream.set_nonblocking(false).ok();
    let mut line = String::new();
    let read_result = (|| -> std::io::Result<()> {
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut line)?;
        Ok(())
    })();
    if let Err(e) = read_result {
        tracing::warn!("ipc read: {e}");
        return;
    }
    let resp = match ipc::parse_request(&line) {
        Ok(req) => apply_request(req, state, db, device, manager, qh),
        Err(e) => Response::Error {
            message: format!("{e:#}"),
        },
    };
    if let Err(e) = ipc::write_response(&mut stream, &resp) {
        tracing::warn!("ipc write: {e}");
    }
}

fn apply_request(
    req: Request,
    state: &mut State,
    db: &Db,
    device: &ZwlrDataControlDeviceV1,
    manager: &ZwlrDataControlManagerV1,
    qh: &QueueHandle<State>,
) -> Response {
    match req {
        Request::Activate { uuid, mime } => match db.get(&uuid) {
            Ok(Some(entry)) => {
                let target_mime = mime.unwrap_or_else(|| entry.primary_mime.clone());
                let Some(content) = entry.content_for(&target_mime) else {
                    return Response::Error {
                        message: format!("mime {target_mime} not stored for {uuid}"),
                    };
                };
                let content = content.to_vec();
                if let Err(e) = db.touch(&uuid) {
                    return Response::Error {
                        message: format!("touch: {e:#}"),
                    };
                }
                hold_selection(state, device, manager, qh, target_mime, content);
                Response::Ok
            }
            Ok(None) => Response::Error {
                message: format!("uuid {uuid} not found"),
            },
            Err(e) => Response::Error {
                message: format!("db get: {e:#}"),
            },
        },
    }
}

fn hold_selection(
    state: &mut State,
    device: &ZwlrDataControlDeviceV1,
    manager: &ZwlrDataControlManagerV1,
    qh: &QueueHandle<State>,
    mime: String,
    content: Vec<u8>,
) {
    if let Some(prev) = state.held_source.take() {
        prev.destroy();
    }
    let source = manager.create_data_source(qh, ());
    source.offer(mime.clone());
    device.set_selection(Some(&source));

    state.expect_self_hash = Some(*blake3::hash(&content).as_bytes());
    state.held_mime = mime;
    state.held_content = content;
    state.held_source = Some(source);
    tracing::debug!(
        "now serving {} bytes of {}",
        state.held_content.len(),
        state.held_mime
    );
}

fn process_offer(
    offer: &ZwlrDataControlOfferV1,
    state: &mut State,
    db: &Db,
    queue: &mut EventQueue<State>,
) -> Result<(), OfferError> {
    let mimes = state
        .offer_mimes
        .get(&offer.id())
        .cloned()
        .unwrap_or_default();
    let (kind, primary_mime) =
        pick_mime(&mimes).ok_or_else(|| OfferError::NoMime(mimes.clone()))?;

    let cap = match kind {
        Kind::Text => TEXT_CAP_BYTES,
        Kind::Image => IMAGE_CAP_BYTES,
    };

    let primary_content = receive(offer, &primary_mime, cap, queue, state)?;
    if primary_content.is_empty() {
        return Err(OfferError::EmptyContent(primary_mime));
    }

    // Self-loop suppression: while we hold a source, the compositor
    // re-broadcasts our selection multiple times per copy. Suppress every
    // event whose primary content matches what we put on the wire — only
    // cleared when our source is Cancelled (see Source dispatch impl).
    let hash = *blake3::hash(&primary_content).as_bytes();
    if state.expect_self_hash.as_ref() == Some(&hash) {
        return Ok(());
    }

    // Drain every other recognized mime the source advertised.
    let extras = drain_extra_mimes(offer, &mimes, &primary_mime, kind, queue, state);

    let preview = match kind {
        Kind::Text => Some(crate::preview::build_from_bytes(&primary_content)),
        Kind::Image => None,
    };

    let result = db.record(
        kind,
        &primary_mime,
        &primary_content,
        preview.as_deref(),
        &extras,
    )?;
    tracing::debug!(
        "synced uuid={uuid} kind={kind} primary_mime={primary_mime} extras={extras_count} bytes={bytes}",
        uuid = match &result {
            crate::db::RecordResult::Inserted(u) | crate::db::RecordResult::Existed(u) => u,
        },
        extras_count = extras.len(),
        bytes = primary_content.len(),
    );
    Ok(())
}

/// Drain every "useful" mime from the offer beyond the primary one. Best
/// effort: errors on an extra are logged and skipped, not fatal.
fn drain_extra_mimes(
    offer: &ZwlrDataControlOfferV1,
    advertised: &[String],
    primary_mime: &str,
    kind: Kind,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Vec<MimeContent> {
    let cap = match kind {
        Kind::Text => TEXT_CAP_BYTES,
        Kind::Image => IMAGE_CAP_BYTES,
    };
    let mut out = Vec::new();
    for m in advertised {
        if m == primary_mime || !worth_keeping(m, kind) {
            continue;
        }
        match receive(offer, m, cap, queue, state) {
            Ok(content) if !content.is_empty() => out.push(MimeContent {
                mime: m.clone(),
                content,
            }),
            Ok(_) => {}
            Err(e) => tracing::debug!("skip extra mime {m}: {e:#}"),
        }
    }
    out
}

fn worth_keeping(mime: &str, kind: Kind) -> bool {
    match kind {
        Kind::Text => {
            let m = mime.to_ascii_lowercase();
            m.starts_with("text/") || m == "utf8_string" || m == "string"
        }
        Kind::Image => mime.to_ascii_lowercase().starts_with("image/"),
    }
}

fn receive(
    offer: &ZwlrDataControlOfferV1,
    mime: &str,
    cap: usize,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Result<Vec<u8>, OfferError> {
    let (read_fd, write_fd) = nix::unistd::pipe()?;

    offer.receive(mime.to_string(), write_fd.as_fd());
    queue.roundtrip(state)?;
    drop(write_fd);

    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut file = std::fs::File::from(read_fd);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() + n > cap {
            return Err(OfferError::TooLarge(cap));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn pick_mime(mimes: &[String]) -> Option<(Kind, String)> {
    let has = |needle: &str| {
        mimes
            .iter()
            .find(|m| m.eq_ignore_ascii_case(needle))
            .cloned()
    };

    if let Some(m) = has("image/png") {
        return Some((Kind::Image, m));
    }
    if let Some(m) = has("image/jpeg") {
        return Some((Kind::Image, m));
    }
    if let Some(m) = mimes.iter().find(|m| m.starts_with("image/")).cloned() {
        return Some((Kind::Image, m));
    }
    for candidate in [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "text/html",
        "text/uri-list",
    ] {
        if let Some(m) = has(candidate) {
            return Some((Kind::Text, m));
        }
    }
    None
}

// ─── Dispatch impls ──────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id: Some(offer) } => {
                state.pending = Some(offer);
            }
            zwlr_data_control_device_v1::Event::Finished => {
                tracing::warn!("data control device finished");
            }
            _ => {}
        }
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => {
                qh.make_data::<ZwlrDataControlOfferV1, ()>(())
            }
            _ => panic!("unknown child opcode {opcode}"),
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state
                .offer_mimes
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.held_source.as_ref().map(|s| s.id()) != Some(source.id()) {
            return;
        }
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type == state.held_mime {
                    use std::io::Write;
                    let mut f = std::fs::File::from(fd);
                    if let Err(e) = f.write_all(&state.held_content) {
                        tracing::warn!("serve content: {e}");
                    }
                    let _ = f.flush();
                } else {
                    tracing::debug!("paste asked for {mime_type}, only have {}", state.held_mime);
                    let _ = nix::unistd::close(fd.as_raw_fd());
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if let Some(prev) = state.held_source.take() {
                    prev.destroy();
                }
                state.held_content.clear();
                state.held_mime.clear();
                state.expect_self_hash = None;
            }
            _ => {}
        }
    }
}
