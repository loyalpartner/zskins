//! Compositor IPC abstraction.
//!
//! Each supported Wayland compositor exposes its "currently focused window"
//! via a different private IPC socket. Callers don't want that in their
//! face — they just want `(app_id, title)` of whichever window was focused
//! before zofi grabbed input.
//!
//! The trait deliberately stays narrow: one method today, with room for
//! workspace/pid extensions later (see `zskins#14`). Each backend silently
//! degrades to `None` on any error — a missing compositor IPC is the
//! common case, not an exceptional one, and crash-on-failure would make
//! zofi unusable outside the one compositor we happened to test on.

mod hyprland;
mod noop;
mod sway;

pub use hyprland::HyprlandIpc;
pub use noop::NoopIpc;
pub use sway::SwayIpc;

/// One-way read interface to whatever compositor is running. Implementers
/// live in the sibling modules; pick one at runtime via [`detect`].
pub trait CompositorIpc: Send + Sync {
    /// `(app_id, title)` of the window that held keyboard focus when this
    /// was called, or `None` if nothing is focused or the IPC failed.
    fn focused_window(&self) -> Option<(String, String)>;
}

/// Pick the first compositor backend whose detection signal is set in the
/// environment. Detection happens in a fixed order: sway → Hyprland →
/// noop. The returned trait object is always usable — the noop fallback
/// just answers `None` so callers don't need to special-case "no
/// compositor detected".
pub fn detect() -> Box<dyn CompositorIpc> {
    if std::env::var("SWAYSOCK").is_ok() || std::env::var("I3SOCK").is_ok() {
        return Box::new(SwayIpc);
    }
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        return Box::new(HyprlandIpc);
    }
    Box::new(NoopIpc)
}
