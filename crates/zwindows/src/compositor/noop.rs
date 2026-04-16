//! Fallback backend. Always reports "no focused window" — used when
//! detection can't identify the compositor, so callers don't need
//! special-case `Option<Box<dyn CompositorIpc>>` handling.

use super::CompositorIpc;

pub struct NoopIpc;

impl CompositorIpc for NoopIpc {
    fn focused_window(&self) -> Option<(String, String)> {
        None
    }
}
