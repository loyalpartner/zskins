//! Sway (and i3) backend — delegates to the existing `sway_tree::focused_window`
//! walker so we don't duplicate the i3-ipc wire format.

use super::CompositorIpc;

pub struct SwayIpc;

impl CompositorIpc for SwayIpc {
    fn focused_window(&self) -> Option<(String, String)> {
        crate::sway_tree::focused_window().ok().flatten()
    }
}
