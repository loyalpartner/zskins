//! One-shot wlr-screencopy client.
//!
//! Captures every visible `wl_output` into an in-memory RGBA buffer keyed by
//! output name. Designed for "snapshot the desktop right now" workflows
//! (e.g. window-thumbnail previews) — not for streaming. Each call:
//!
//! 1. Connects to Wayland, binds `zwlr_screencopy_manager_v1`, `wl_shm`, and
//!    every `wl_output` (v4+ for the `name` event).
//! 2. For each output, asks the compositor to capture into a freshly allocated
//!    shm-backed `wl_buffer`, blocks until `ready` or `failed`, then copies
//!    bytes out of the mmap.
//! 3. Returns `HashMap<output_name, RgbaBuffer>` (BGR/X-prefixed shm formats
//!    are normalised to canonical RGBA byte order so cropping callers don't
//!    need to know about wl_shm format quirks).
//!
//! Failure paths (non-wlroots compositor, no outputs, capture failed/timeout)
//! return an empty map rather than an error — preview is a "nice to have" and
//! falling back to text is preferable to crashing the launcher.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry,
        wl_shm::{self, Format, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// A captured frame, pre-converted to packed RGBA8 (4 bytes per pixel, no row
/// padding). Stride is implied as `width * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaBuffer {
    pub width: u32,
    pub height: u32,
    /// Packed `width * height * 4` bytes, RGBA order.
    pub data: Vec<u8>,
}

impl RgbaBuffer {
    /// Crop a sub-rectangle out of this buffer. Out-of-bounds rects are
    /// clipped to the source dimensions (sway sometimes reports a window rect
    /// extending into output gaps after a workspace switch). An empty
    /// intersection returns `None` so callers fall back to no-preview.
    pub fn crop(&self, x: i32, y: i32, w: u32, h: u32) -> Option<RgbaBuffer> {
        let sx = x.max(0) as u32;
        let sy = y.max(0) as u32;
        let ex = (x.saturating_add(w as i32)).max(0) as u32;
        let ey = (y.saturating_add(h as i32)).max(0) as u32;
        let cx = sx.min(self.width);
        let cy = sy.min(self.height);
        let cex = ex.min(self.width);
        let cey = ey.min(self.height);
        if cx >= cex || cy >= cey {
            return None;
        }
        let cw = cex - cx;
        let ch = cey - cy;
        let mut out = Vec::with_capacity((cw * ch * 4) as usize);
        for row in 0..ch {
            let src_start = (((cy + row) * self.width) + cx) as usize * 4;
            let src_end = src_start + cw as usize * 4;
            out.extend_from_slice(&self.data[src_start..src_end]);
        }
        Some(RgbaBuffer {
            width: cw,
            height: ch,
            data: out,
        })
    }

    /// Downscale to fit within a `(max_w, max_h)` box, preserving aspect
    /// ratio, using fractional area-averaging (each destination pixel is the
    /// weighted average of the source pixels it overlaps).
    ///
    /// Aimed at preview thumbnails: GPUI's wgpu sampler is plain bilinear
    /// with no mipmaps, so downscaling by >~1.5x in the shader aliases text.
    /// Pre-rendering to roughly the final display size lets the GPU do a
    /// near-identity resample — the cheap path its linear filter handles
    /// well. Returns `self` unchanged if it already fits.
    pub fn downscale_to_box(&self, max_w: u32, max_h: u32) -> RgbaBuffer {
        if self.width == 0
            || self.height == 0
            || max_w == 0
            || max_h == 0
            || (self.width <= max_w && self.height <= max_h)
        {
            return self.clone();
        }
        // Preserve aspect: pick the tighter scale so both dims fit.
        let scale = (max_w as f32 / self.width as f32).min(max_h as f32 / self.height as f32);
        let dw = ((self.width as f32 * scale).round() as u32).max(1);
        let dh = ((self.height as f32 * scale).round() as u32).max(1);
        // Inverse sampling window for each destination pixel.
        let fx = self.width as f32 / dw as f32;
        let fy = self.height as f32 / dh as f32;
        let mut out = vec![0u8; (dw * dh * 4) as usize];
        let src_stride = self.width as usize * 4;
        for dy in 0..dh {
            let sy0 = (dy as f32 * fy).floor() as u32;
            let sy1 = (((dy + 1) as f32 * fy).ceil() as u32).min(self.height);
            for dx in 0..dw {
                let sx0 = (dx as f32 * fx).floor() as u32;
                let sx1 = (((dx + 1) as f32 * fx).ceil() as u32).min(self.width);
                let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
                let mut n: u32 = 0;
                for sy in sy0..sy1 {
                    let row = sy as usize * src_stride;
                    for sx in sx0..sx1 {
                        let p = row + sx as usize * 4;
                        r += self.data[p] as u32;
                        g += self.data[p + 1] as u32;
                        b += self.data[p + 2] as u32;
                        a += self.data[p + 3] as u32;
                        n += 1;
                    }
                }
                let d = (dy * dw + dx) as usize * 4;
                out[d] = (r / n) as u8;
                out[d + 1] = (g / n) as u8;
                out[d + 2] = (b / n) as u8;
                out[d + 3] = (a / n) as u8;
            }
        }
        RgbaBuffer {
            width: dw,
            height: dh,
            data: out,
        }
    }

    /// Convenience wrapper: square bounding box of `max_side` on each side.
    pub fn downscale_to(&self, max_side: u32) -> RgbaBuffer {
        self.downscale_to_box(max_side, max_side)
    }

    /// Encode as PNG bytes for handoff to gpui::Image (which expects an
    /// encoded payload). Returns None on encode failure — callers degrade to
    /// no-preview rather than panic.
    pub fn to_png(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&self.data).ok()?;
        }
        Some(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("wayland global init: {0}")]
    GlobalInit(#[from] wayland_client::globals::GlobalError),
    #[error("wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("compositor lacks zwlr_screencopy_manager_v1")]
    NoScreencopy,
    #[error("compositor lacks wl_shm")]
    NoShm,
    #[error("wayland backend: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("screencopy timeout")]
    Timeout,
    #[error("compositor returned screencopy `failed` event")]
    Failed,
}

/// Capture every visible output. Returns a map keyed by output name (e.g.
/// `"DP-1"`); outputs that don't deliver a name event in time are skipped.
///
/// `timeout` bounds the wait per-output: hitting it logs a warning and the
/// affected output is omitted from the result map. The total wall time is
/// roughly `timeout * num_outputs` in the worst case but in practice each
/// capture takes <100ms on modern compositors.
pub fn capture_all_outputs(timeout: Duration) -> Result<HashMap<String, RgbaBuffer>, CaptureError> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue): (_, EventQueue<State>) = registry_queue_init(&conn)?;
    let qh = queue.handle();

    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|_| CaptureError::NoScreencopy)?;
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| CaptureError::NoShm)?;

    let mut state = State {
        outputs: Vec::new(),
    };

    // Bind every advertised wl_output. v4 added the `name` event we rely on
    // for keying the result map; older compositors won't surface a name and
    // the output will be silently skipped.
    let contents = globals.contents();
    let listing = contents.clone_list();
    for global in listing {
        if global.interface == "wl_output" {
            let version = global.version.min(4);
            let output: WlOutput =
                globals
                    .registry()
                    .bind(global.name, version, &qh, OutputData::default());
            state.outputs.push(OutputEntry { output, name: None });
        }
    }

    // Drain the initial output events (Name, Geometry, Done) so we know which
    // output corresponds to which name before issuing capture requests.
    let deadline = Instant::now() + timeout;
    while state.outputs.iter().any(|o| o.name.is_none()) {
        if Instant::now() >= deadline {
            tracing::warn!("screencopy: timed out waiting for output names");
            break;
        }
        // Use roundtrip rather than blocking_dispatch so we always make
        // progress on names even if no spontaneous events are pending.
        conn.roundtrip()?;
        queue.dispatch_pending(&mut state)?;
    }

    let mut results = HashMap::new();
    for entry in state.outputs.drain(..) {
        let Some(name) = entry.name.clone() else {
            continue;
        };
        match capture_one(
            &conn,
            &mut queue,
            &qh,
            &manager,
            &shm,
            &entry.output,
            timeout,
        ) {
            Ok(buf) => {
                results.insert(name, buf);
            }
            Err(e) => {
                tracing::warn!("screencopy: capture {} failed: {}", name, e);
            }
        }
    }
    Ok(results)
}

/// Per-output capture: ask for one frame, wait for buffer params, allocate a
/// matching wl_buffer, request copy, await ready, return RGBA bytes.
fn capture_one(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    qh: &QueueHandle<State>,
    manager: &ZwlrScreencopyManagerV1,
    shm: &WlShm,
    output: &WlOutput,
    timeout: Duration,
) -> Result<RgbaBuffer, CaptureError> {
    let frame_state = Arc::new(Mutex::new(FrameState::default()));
    let frame: ZwlrScreencopyFrameV1 = manager.capture_output(0, output, qh, frame_state.clone());
    conn.flush()?;

    let deadline = Instant::now() + timeout;

    // Phase 1: wait for buffer_done so we know the format/size.
    let (format, width, height, stride) = loop {
        if Instant::now() >= deadline {
            frame.destroy();
            return Err(CaptureError::Timeout);
        }
        queue.blocking_dispatch(&mut State {
            outputs: Vec::new(),
        })?;
        let st = frame_state.lock().unwrap();
        if st.failed {
            drop(st);
            frame.destroy();
            return Err(CaptureError::Failed);
        }
        if st.buffer_done {
            if let Some(p) = st.params {
                break p;
            }
        }
    };

    let size = (stride as usize) * (height as usize);
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

    frame.copy(&buffer);
    conn.flush()?;

    // Phase 2: wait for ready (or failed).
    loop {
        if Instant::now() >= deadline {
            frame.destroy();
            buffer.destroy();
            return Err(CaptureError::Timeout);
        }
        queue.blocking_dispatch(&mut State {
            outputs: Vec::new(),
        })?;
        let st = frame_state.lock().unwrap();
        if st.failed {
            drop(st);
            frame.destroy();
            buffer.destroy();
            return Err(CaptureError::Failed);
        }
        if st.ready {
            break;
        }
    }

    // Read the mmap, normalise to RGBA, then drop the wayland-side resources.
    let raw = unsafe { std::slice::from_raw_parts(shm_buf.ptr.as_ptr() as *const u8, size) };
    let rgba = convert_to_rgba(raw, width, height, stride, format);
    let flipped = if frame_state.lock().unwrap().y_invert {
        flip_vertical(&rgba, width, height)
    } else {
        rgba
    };
    frame.destroy();
    buffer.destroy();
    Ok(RgbaBuffer {
        width,
        height,
        data: flipped,
    })
}

fn flip_vertical(buf: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 4;
    let mut out = vec![0u8; buf.len()];
    for y in 0..height as usize {
        let src = y * row;
        let dst = (height as usize - 1 - y) * row;
        out[dst..dst + row].copy_from_slice(&buf[src..src + row]);
    }
    out
}

/// Translate a raw shm buffer into packed RGBA8.
///
/// Common wlroots formats are `Argb8888`, `Xrgb8888`, `Abgr8888`, `Xbgr8888` —
/// the channel order on the wire is little-endian (so `Argb8888` is `[B,G,R,A]`
/// in memory). Other formats degrade to a magenta placeholder so the preview
/// still renders something, signalling "we got data but couldn't decode it".
pub fn convert_to_rgba(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
) -> Vec<u8> {
    let pixel_count = (width * height) as usize;
    let mut out = Vec::with_capacity(pixel_count * 4);
    for y in 0..height as usize {
        let row_start = y * stride as usize;
        for x in 0..width as usize {
            let i = row_start + x * 4;
            // Bounds-check: corrupt buffer would otherwise panic the worker
            // thread and tear down the launcher. Pad with opaque black on
            // short reads.
            if i + 4 > raw.len() {
                out.extend_from_slice(&[0, 0, 0, 255]);
                continue;
            }
            let p = &raw[i..i + 4];
            match format {
                Format::Argb8888 => out.extend_from_slice(&[p[2], p[1], p[0], p[3]]),
                Format::Xrgb8888 => out.extend_from_slice(&[p[2], p[1], p[0], 255]),
                Format::Abgr8888 => out.extend_from_slice(&[p[0], p[1], p[2], p[3]]),
                Format::Xbgr8888 => out.extend_from_slice(&[p[0], p[1], p[2], 255]),
                _ => out.extend_from_slice(&[255, 0, 255, 255]),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct State {
    outputs: Vec<OutputEntry>,
}

struct OutputEntry {
    output: WlOutput,
    name: Option<String>,
}

#[derive(Default)]
struct OutputData {
    name: Mutex<Option<String>>,
}

#[derive(Default)]
struct FrameState {
    buffer_done: bool,
    ready: bool,
    failed: bool,
    y_invert: bool,
    /// (format, width, height, stride) from the buffer event.
    params: Option<(Format, u32, u32, u32)>,
}

struct ShmBuffer {
    fd: std::os::fd::OwnedFd,
    ptr: NonNull<std::ffi::c_void>,
    len: usize,
}

impl ShmBuffer {
    fn new(size: usize) -> std::io::Result<Self> {
        let name = std::ffi::CString::new("zwindows-screencopy").unwrap();
        let fd = memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC)
            .map_err(|e| std::io::Error::other(format!("memfd_create: {e}")))?;
        // Resize to exactly `size` so wl_shm_pool sees the right length.
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
        // Best-effort munmap; nothing useful to do on failure during teardown.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

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

impl Dispatch<WlOutput, OutputData> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            *data.name.lock().unwrap() = Some(name.clone());
            if let Some(entry) = state
                .outputs
                .iter_mut()
                .find(|e| e.output.id() == output.id())
            {
                entry.name = Some(name);
            }
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, Arc<Mutex<FrameState>>> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        data: &Arc<Mutex<FrameState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut st = data.lock().unwrap();
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(fmt),
                width,
                height,
                stride,
            } => {
                // Pick the first wl_shm format the compositor offers; we only
                // act on it if buffer_done arrives. Compositors may emit
                // several Buffer events (one per supported format).
                st.params.get_or_insert((fmt, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                st.buffer_done = true;
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(f),
            } => {
                st.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                st.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                st.failed = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(w: u32, h: u32, fill: [u8; 4]) -> RgbaBuffer {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            data.extend_from_slice(&fill);
        }
        RgbaBuffer {
            width: w,
            height: h,
            data,
        }
    }

    #[test]
    fn crop_returns_subrect_in_bounds() {
        // 4x4 source, all pixels distinct via row index.
        let mut data = Vec::with_capacity(64);
        for y in 0..4u8 {
            for x in 0..4u8 {
                data.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let src = RgbaBuffer {
            width: 4,
            height: 4,
            data,
        };
        let crop = src.crop(1, 1, 2, 2).unwrap();
        assert_eq!(crop.width, 2);
        assert_eq!(crop.height, 2);
        // First pixel of crop = source (1,1) → [1,1,0,255]
        assert_eq!(&crop.data[0..4], &[1, 1, 0, 255]);
    }

    #[test]
    fn crop_clips_to_source_bounds() {
        let src = rgba(10, 10, [200, 100, 50, 255]);
        let crop = src.crop(8, 8, 10, 10).unwrap();
        // 10x10 source, rect (8,8,10,10) clips to 2x2.
        assert_eq!(crop.width, 2);
        assert_eq!(crop.height, 2);
    }

    #[test]
    fn crop_empty_intersection_returns_none() {
        let src = rgba(5, 5, [0; 4]);
        assert!(src.crop(100, 100, 50, 50).is_none());
        assert!(src.crop(-100, -100, 50, 50).is_none());
    }

    #[test]
    fn crop_negative_origin_clipped_to_zero() {
        let src = rgba(4, 4, [9, 8, 7, 255]);
        // x=-1, y=-1, w=3, h=3 → effective (0,0,2,2)
        let crop = src.crop(-1, -1, 3, 3).unwrap();
        assert_eq!(crop.width, 2);
        assert_eq!(crop.height, 2);
    }

    #[test]
    fn convert_argb8888_swaps_bgra_to_rgba() {
        // wire little-endian Argb8888 = bytes [B, G, R, A] in memory.
        let raw = vec![10, 20, 30, 255, 40, 50, 60, 128];
        let out = convert_to_rgba(&raw, 2, 1, 8, Format::Argb8888);
        assert_eq!(out, vec![30, 20, 10, 255, 60, 50, 40, 128]);
    }

    #[test]
    fn convert_xrgb8888_forces_alpha_255() {
        // Xrgb8888 has no alpha — we must inject opaque, never trust the X byte.
        let raw = vec![10, 20, 30, 0];
        let out = convert_to_rgba(&raw, 1, 1, 4, Format::Xrgb8888);
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn convert_abgr8888_passes_through_rgba_order() {
        let raw = vec![1, 2, 3, 200];
        let out = convert_to_rgba(&raw, 1, 1, 4, Format::Abgr8888);
        assert_eq!(out, vec![1, 2, 3, 200]);
    }

    #[test]
    fn convert_xbgr8888_forces_alpha_255() {
        let raw = vec![1, 2, 3, 0];
        let out = convert_to_rgba(&raw, 1, 1, 4, Format::Xbgr8888);
        assert_eq!(out, vec![1, 2, 3, 255]);
    }

    #[test]
    fn convert_unknown_format_emits_magenta_placeholder() {
        // Don't crash on exotic shm formats — flag the pixels visibly so a
        // user notices "this preview is wrong".
        let raw = vec![0u8; 4];
        let out = convert_to_rgba(&raw, 1, 1, 4, Format::C8);
        assert_eq!(out, vec![255, 0, 255, 255]);
    }

    #[test]
    fn convert_skips_stride_padding() {
        // 2px wide image with stride=12 (4 bytes pad per row). The pad bytes
        // must not appear in the output.
        let raw = vec![
            10, 20, 30, 255, 40, 50, 60, 128, 0xFF, 0xFF, 0xFF, 0xFF, 70, 80, 90, 200, 100, 110,
            120, 50, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let out = convert_to_rgba(&raw, 2, 2, 12, Format::Argb8888);
        assert_eq!(
            out,
            vec![30, 20, 10, 255, 60, 50, 40, 128, 90, 80, 70, 200, 120, 110, 100, 50]
        );
    }

    #[test]
    fn flip_vertical_reverses_rows() {
        // 1x2 image: row 0 = red, row 1 = blue. After flip, row 0 = blue.
        let buf = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let out = flip_vertical(&buf, 1, 2);
        assert_eq!(out, vec![0, 0, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn downscale_noop_when_under_max() {
        let src = rgba(100, 80, [1, 2, 3, 255]);
        let out = src.downscale_to(1024);
        assert_eq!(out, src);
    }

    #[test]
    fn downscale_shrinks_longer_side_under_max() {
        // 2000x1000: factor = ceil(2000/1000)=2, so output = 1000x500.
        let src = rgba(2000, 1000, [40, 80, 120, 255]);
        let out = src.downscale_to(1000);
        assert_eq!(out.width, 1000);
        assert_eq!(out.height, 500);
        // Uniform input → uniform output; box filter preserves color.
        assert_eq!(&out.data[..4], &[40, 80, 120, 255]);
    }

    #[test]
    fn downscale_averages_2x2_block() {
        // 2x2 with four different colors; factor 2 collapses to 1x1 average.
        let src = RgbaBuffer {
            width: 2,
            height: 2,
            data: vec![
                0, 0, 0, 0, //
                100, 100, 100, 100, //
                50, 50, 50, 50, //
                250, 250, 250, 250,
            ],
        };
        let out = src.downscale_to(1);
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(out.data, vec![100, 100, 100, 100]);
    }

    #[test]
    fn to_png_round_trips_dimensions() {
        let src = rgba(4, 3, [100, 50, 25, 255]);
        let png = src.to_png().expect("png encoding should succeed");
        // Re-decode header to confirm dimensions survive the roundtrip.
        let decoder = png::Decoder::new(png.as_slice());
        let reader = decoder.read_info().expect("decode header");
        assert_eq!(reader.info().width, 4);
        assert_eq!(reader.info().height, 3);
    }
}
