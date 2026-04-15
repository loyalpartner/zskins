//! Per-toplevel image capture using the ext protocols.
//!
//! Uses three Wayland staging protocols to capture individual window contents
//! without requiring sway IPC or per-output screencopy + coordinate cropping:
//!
//! 1. `ext_foreign_toplevel_list_v1` — enumerate all toplevel handles
//!    (including windows on other workspaces and minimized windows).
//! 2. `ext_foreign_toplevel_image_capture_source_manager_v1` — turn a
//!    toplevel handle into an opaque `ext_image_capture_source_v1`.
//! 3. `ext_image_copy_capture_manager_v1` — create a capture session from
//!    the source, negotiate buffer constraints, and copy pixels into shm.
//!
//! The public entry point is [`capture_toplevels`], which returns
//! `HashMap<(app_id, title), RgbaBuffer>`.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_shm::{self, Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use crate::screencopy::{convert_to_rgba, RgbaBuffer};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland global init: {0}")]
    GlobalInit(#[from] wayland_client::globals::GlobalError),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("compositor lacks ext_foreign_toplevel_list_v1")]
    NoToplevelList,
    #[error("compositor lacks ext_foreign_toplevel_image_capture_source_manager_v1")]
    NoToplevelCaptureSource,
    #[error("compositor lacks ext_image_copy_capture_manager_v1")]
    NoCopyCapture,
    #[error("compositor lacks wl_shm")]
    NoShm,
    #[error("wayland backend: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("capture timeout")]
    Timeout,
    #[error("capture failed: {0}")]
    Failed(String),
}

/// Capture every toplevel window into an RGBA buffer keyed by `(app_id, title)`.
///
/// Returns an empty map when the compositor lacks the required protocols or
/// when no toplevels are found. This is the replacement for the
/// `screencopy + sway_tree` pipeline — it works on any compositor that
/// implements the ext capture protocols and can capture minimized / off-screen
/// windows.
pub fn capture_toplevels(timeout: Duration) -> Result<HashMap<(String, String), RgbaBuffer>, CaptureError> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue): (_, EventQueue<State>) = registry_queue_init(&conn)?;
    let qh = queue.handle();

    // Bind all required globals.
    let _toplevel_list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| CaptureError::NoToplevelList)?;
    let capture_source_mgr: ExtForeignToplevelImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| CaptureError::NoToplevelCaptureSource)?;
    let copy_capture_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| CaptureError::NoCopyCapture)?;
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| CaptureError::NoShm)?;

    let mut state = State::default();

    // Phase 1: collect toplevels. The compositor sends toplevel events
    // immediately after binding the list; a roundtrip ensures we've received
    // the initial batch plus done events for each handle.
    let deadline = Instant::now() + timeout;
    // Two roundtrips: first triggers the toplevel events, second ensures all
    // per-handle property events (title, app_id, done) have arrived.
    conn.roundtrip()?;
    queue.dispatch_pending(&mut state)?;
    conn.roundtrip()?;
    queue.dispatch_pending(&mut state)?;

    // Stop the toplevel list — we don't need further updates.
    _toplevel_list.stop();
    conn.flush()?;

    let toplevels: Vec<ToplevelInfo> = state
        .toplevels
        .iter()
        .filter(|t| t.done && t.app_id.is_some())
        .cloned()
        .collect();

    tracing::info!(
        "toplevel_capture: discovered {} toplevels",
        toplevels.len()
    );
    if toplevels.is_empty() {
        return Ok(HashMap::new());
    }

    // Phase 2: for each toplevel, create a capture source and capture one frame.
    let mut results = HashMap::new();
    for tl in &toplevels {
        if Instant::now() >= deadline {
            tracing::warn!("toplevel_capture: global timeout reached");
            break;
        }
        let app_id = tl.app_id.clone().unwrap_or_default();
        let title = tl.title.clone().unwrap_or_default();
        let remaining = deadline.saturating_duration_since(Instant::now());

        match capture_one_toplevel(
            &conn,
            &mut queue,
            &qh,
            &capture_source_mgr,
            &copy_capture_mgr,
            &shm,
            &tl.handle,
            remaining,
        ) {
            Ok(buf) => {
                tracing::info!(
                    "toplevel_capture: captured ({app_id:?}, {title:?}) {}x{}",
                    buf.width,
                    buf.height
                );
                results.insert((app_id, title), buf);
            }
            Err(e) => {
                tracing::info!(
                    "toplevel_capture: skip ({app_id:?}, {title:?}): {e}"
                );
            }
        }
    }

    tracing::info!(
        "toplevel_capture: captured {}/{} toplevels",
        results.len(),
        toplevels.len()
    );

    // Explicitly destroy all protocol objects so the compositor releases
    // capture-related rendering state (e.g. off-screen compositing) before
    // we drop the connection. Without this, sway keeps windows in a
    // degraded rendering mode until something else forces a recomposite.
    for tl in &state.toplevels {
        tl.handle.destroy();
    }
    _toplevel_list.destroy();
    capture_source_mgr.destroy();
    copy_capture_mgr.destroy();
    // Roundtrip so the compositor processes all destroy requests and
    // restores normal rendering before we close the connection.
    let _ = conn.flush();
    let _ = conn.roundtrip();
    let _ = queue.dispatch_pending(&mut state);

    Ok(results)
}

/// Capture a single toplevel into an RgbaBuffer.
#[allow(clippy::too_many_arguments)]
fn capture_one_toplevel(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    qh: &QueueHandle<State>,
    capture_source_mgr: &ExtForeignToplevelImageCaptureSourceManagerV1,
    copy_capture_mgr: &ExtImageCopyCaptureManagerV1,
    shm: &WlShm,
    handle: &ExtForeignToplevelHandleV1,
    timeout: Duration,
) -> Result<RgbaBuffer, CaptureError> {
    // Step 1: create an image capture source from the toplevel handle.
    let source: ExtImageCaptureSourceV1 =
        capture_source_mgr.create_source(handle, qh, ());

    // Step 2: create a capture session (no cursor overlay).
    let session_state = Arc::new(Mutex::new(SessionState::default()));
    let session: ExtImageCopyCaptureSessionV1 =
        copy_capture_mgr.create_session(&source, Options::empty(), qh, session_state.clone());
    conn.flush()?;

    // Step 3: wait for buffer constraints (buffer_size, shm_format, done).
    let deadline = Instant::now() + timeout;
    let (width, height, format) = loop {
        if Instant::now() >= deadline {
            session.destroy();
            source.destroy();
            return Err(CaptureError::Timeout);
        }
        queue.blocking_dispatch(&mut State::default())?;
        let st = session_state.lock().unwrap();
        if st.stopped {
            drop(st);
            session.destroy();
            source.destroy();
            return Err(CaptureError::Failed("session stopped".into()));
        }
        if st.done {
            if let (Some((w, h)), Some(fmt)) = (st.buffer_size, st.shm_format) {
                break (w, h, fmt);
            }
        }
    };

    // Step 4: allocate shm buffer matching constraints.
    let stride = width * 4;
    let size = (stride * height) as usize;
    let shm_buf = ShmBuffer::new(size)?;
    let pool: WlShmPool = shm.create_pool(shm_buf.fd.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    pool.destroy();

    // Step 5: create a frame, attach buffer, damage full region, capture.
    let frame_state = Arc::new(Mutex::new(FrameState::default()));
    let frame: ExtImageCopyCaptureFrameV1 =
        session.create_frame(qh, frame_state.clone());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    conn.flush()?;

    // Step 6: wait for ready or failed.
    loop {
        if Instant::now() >= deadline {
            frame.destroy();
            session.destroy();
            source.destroy();
            buffer.destroy();
            return Err(CaptureError::Timeout);
        }
        queue.blocking_dispatch(&mut State::default())?;
        let st = frame_state.lock().unwrap();
        if st.failed {
            let reason = st.failure_reason.clone();
            drop(st);
            frame.destroy();
            session.destroy();
            source.destroy();
            buffer.destroy();
            return Err(CaptureError::Failed(
                reason.unwrap_or_else(|| "unknown".into()),
            ));
        }
        if st.ready {
            break;
        }
    }

    // Step 7: read pixels from mmap, convert to RGBA.
    let raw = unsafe { std::slice::from_raw_parts(shm_buf.ptr.as_ptr() as *const u8, size) };
    let rgba = convert_to_rgba(raw, width, height, stride, format);

    frame.destroy();
    session.destroy();
    source.destroy();
    buffer.destroy();

    Ok(RgbaBuffer {
        width,
        height,
        data: rgba,
    })
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    toplevels: Vec<ToplevelInfo>,
}

#[derive(Clone)]
struct ToplevelInfo {
    handle: ExtForeignToplevelHandleV1,
    app_id: Option<String>,
    title: Option<String>,
    done: bool,
}

#[derive(Default)]
struct SessionState {
    buffer_size: Option<(u32, u32)>,
    shm_format: Option<Format>,
    done: bool,
    stopped: bool,
}

#[derive(Default)]
struct FrameState {
    ready: bool,
    failed: bool,
    failure_reason: Option<String>,
}

struct ShmBuffer {
    fd: std::os::fd::OwnedFd,
    ptr: NonNull<std::ffi::c_void>,
    len: usize,
}

impl ShmBuffer {
    fn new(size: usize) -> std::io::Result<Self> {
        let name = std::ffi::CString::new("zwindows-toplevel-capture").unwrap();
        let fd = memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC)
            .map_err(|e| std::io::Error::other(format!("memfd_create: {e}")))?;
        nix::unistd::ftruncate(&fd, size as i64)
            .map_err(|e| std::io::Error::other(format!("ftruncate: {e}")))?;
        let len = NonZeroUsize::new(size).ok_or_else(|| std::io::Error::other("zero-size shm"))?;
        let ptr = unsafe {
            mmap(
                None,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
        }
        .map_err(|e| std::io::Error::other(format!("mmap: {e}")))?;
        Ok(Self { fd, ptr, len: size })
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

// --- Registry ---

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

// --- wl_shm ---

impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self, _: &WlShmPool, _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

// --- ext_foreign_toplevel_list_v1 ---

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state.toplevels.push(ToplevelInfo {
                    handle: toplevel,
                    app_id: None,
                    title: None,
                    done: false,
                });
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                tracing::info!("toplevel_capture: toplevel list finished");
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

// --- ext_foreign_toplevel_handle_v1 ---

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(tl) = state
            .toplevels
            .iter_mut()
            .find(|t| t.handle.id() == handle.id())
        else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                tl.app_id = Some(app_id);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                tl.title = Some(title);
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                tl.done = true;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                tl.done = true;
            }
            _ => {}
        }
    }
}

// --- ext_image_capture_source_v1 ---

impl Dispatch<ExtImageCaptureSourceV1, ()> for State {
    fn event(
        _: &mut Self, _: &ExtImageCaptureSourceV1,
        _: wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

// --- ext_foreign_toplevel_image_capture_source_manager_v1 ---

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _: &mut Self, _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _: wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

// --- ext_image_copy_capture_manager_v1 ---

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(
        _: &mut Self, _: &ExtImageCopyCaptureManagerV1,
        _: wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

// --- ext_image_copy_capture_session_v1 ---

impl Dispatch<ExtImageCopyCaptureSessionV1, Arc<Mutex<SessionState>>> for State {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        data: &Arc<Mutex<SessionState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut st = data.lock().unwrap();
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                st.buffer_size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => {
                // Prefer ARGB/XRGB which we already know how to convert.
                if st.shm_format.is_none() {
                    st.shm_format = Some(fmt);
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                st.done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                st.stopped = true;
            }
            _ => {}
        }
    }
}

// --- ext_image_copy_capture_frame_v1 ---

impl Dispatch<ExtImageCopyCaptureFrameV1, Arc<Mutex<FrameState>>> for State {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        data: &Arc<Mutex<FrameState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut st = data.lock().unwrap();
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                st.ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed {
                reason: WEnum::Value(r),
            } => {
                st.failed = true;
                st.failure_reason = Some(format!("{r:?}"));
            }
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                st.failed = true;
                st.failure_reason = Some("unknown".into());
            }
            _ => {}
        }
    }
}
