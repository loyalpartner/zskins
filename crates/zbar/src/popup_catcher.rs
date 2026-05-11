//! Click-outside dismissal helper for layer-shell popups.
//!
//! For each available display, [`open_catchers`] opens a transparent full-screen
//! layer-shell surface on [`Layer::Top`] that fires the supplied callback when
//! clicked. Place the popup itself on [`Layer::Overlay`] so its body absorbs
//! clicks before they reach a catcher. The catcher honors other surfaces'
//! exclusive zones (`exclusive_zone: None`), so the bar's reserved strip is
//! *not* covered — right-clicks on bar tray icons can still reach the bar to
//! switch from one popup to another.
//!
//! A process-wide [`PopupRegistry`] global lets popup modules dismiss one
//! another: each module registers a close-channel sender on construction, and
//! whatever module is about to open a new popup calls [`dismiss_others`]
//! first.

use gpui::{
    div, layer_shell::*, prelude::*, App, AppContext, Bounds, Context, Global, MouseButton,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};
use std::sync::Arc;

/// Callback fired when the catcher is clicked. Cloned once per display.
pub type DismissFn = Arc<dyn Fn(&mut Window, &mut App) + Send + Sync + 'static>;

pub struct PopupCatcher {
    on_dismiss: DismissFn,
}

impl PopupCatcher {
    pub fn new(on_dismiss: DismissFn) -> Self {
        Self { on_dismiss }
    }
}

impl Render for PopupCatcher {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let f = self.on_dismiss.clone();
        div()
            .size_full()
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                f(window, cx);
            })
    }
}

/// Open one catcher per display. The catcher's `on_mouse_down` invokes the
/// shared `on_dismiss` callback. `namespace` is used both as the layer-shell
/// surface namespace and as the GTK-style app id (helps debugging via
/// `swaymsg -t get_tree`).
pub fn open_catchers(
    cx: &mut App,
    namespace: &str,
    on_dismiss: DismissFn,
) -> Vec<WindowHandle<PopupCatcher>> {
    let mut handles = Vec::new();
    for display in cx.displays() {
        let id = display.id();
        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds::maximized(Some(id), cx))),
            display_id: Some(id),
            app_id: Some(namespace.to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: namespace.to_string(),
                layer: Layer::Top,
                anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                margin: None,
                keyboard_interactivity: KeyboardInteractivity::None,
                // Honor other surfaces' exclusive zones (e.g. the bar) so
                // right-clicks on tray icons still reach the bar.
                exclusive_zone: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let cb = on_dismiss.clone();
        match cx.open_window(opts, move |_, cx| cx.new(|_| PopupCatcher::new(cb))) {
            Ok(h) => handles.push(h),
            Err(e) => tracing::warn!("popup_catcher: failed on {id:?}: {e}"),
        }
    }
    handles
}

/// Close every catcher window in `handles`.
pub fn close_catchers(cx: &mut App, handles: Vec<WindowHandle<PopupCatcher>>) {
    for h in handles {
        let _ = h.update(cx, |_, window, _| window.remove_window());
    }
}

/// [`open_catchers`] shorthand: synthesizes the dismissal callback from a
/// unit-channel sender. The catcher fires `try_send(())` on click and then
/// removes its own window; the receiving module handles the rest.
pub fn open_catchers_for(
    cx: &mut App,
    namespace: &str,
    close_tx: async_channel::Sender<()>,
) -> Vec<WindowHandle<PopupCatcher>> {
    let on_dismiss: DismissFn = Arc::new(move |window, _| {
        let _ = close_tx.try_send(());
        window.remove_window();
    });
    open_catchers(cx, namespace, on_dismiss)
}

// ---------------------------------------------------------------------------
// Cross-module dismissal registry
// ---------------------------------------------------------------------------

/// Token returned by [`register`]. Used to skip the caller's own sender when
/// dismissing other popups, so a module doesn't churn itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PopupKind(usize);

/// Process-wide registry of "please close yourself" senders, one per popup
/// module. Stored as a [`Global`] so modules in different files can find
/// each other without circular imports.
#[derive(Default)]
pub struct PopupRegistry {
    entries: Vec<(PopupKind, async_channel::Sender<()>)>,
    next_id: usize,
}

impl Global for PopupRegistry {}

/// Ensure a [`PopupRegistry`] global exists and register `sender` on it.
pub fn register(cx: &mut App, sender: async_channel::Sender<()>) -> PopupKind {
    if !cx.has_global::<PopupRegistry>() {
        cx.set_global(PopupRegistry::default());
    }
    cx.update_global::<PopupRegistry, _>(|reg, _| {
        let kind = PopupKind(reg.next_id);
        reg.next_id += 1;
        reg.entries.push((kind, sender));
        kind
    })
}

/// Fire every registered close signal except the one owned by `self_kind`.
/// Modules call this immediately before opening their own popup so any
/// other open popup gets dismissed first.
pub fn dismiss_others(cx: &mut App, self_kind: PopupKind) {
    if !cx.has_global::<PopupRegistry>() {
        return;
    }
    cx.update_global::<PopupRegistry, _>(|reg, _| {
        for (kind, tx) in &reg.entries {
            if *kind != self_kind {
                let _ = tx.try_send(());
            }
        }
    });
}

/// Convenience for popup modules: register a unit-channel with the hub and
/// spawn a relay task that invokes `on_close` against the owning entity on
/// every signal. Returns the [`PopupKind`] token (for `dismiss_others`) plus
/// the sender (handy for plumbing the same channel into catchers or other
/// close paths). The spawned task ends when the entity is dropped.
pub fn register_entity<E, F>(
    cx: &mut Context<E>,
    mut on_close: F,
) -> (PopupKind, async_channel::Sender<()>)
where
    E: 'static,
    F: FnMut(&mut E, &mut Context<E>) + 'static,
{
    let (tx, rx) = async_channel::bounded::<()>(4);
    cx.spawn(async move |this, cx| {
        while rx.recv().await.is_ok() {
            if this.update(cx, |m, cx| on_close(m, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    let kind = register(cx, tx.clone());
    (kind, tx)
}
