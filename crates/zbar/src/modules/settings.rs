//! Gear pill on the right edge of the bar that opens a small popup for
//! switching between built-in palettes. Writes the selection to disk via
//! [`ztheme::save`] so the change persists; the running app picks up the
//! same change either through this click handler (immediate) or through
//! the file watcher (out-of-band edits to `config.toml`).
//!
//! The popup itself is a thin layer-shell window — the same approach
//! [`crate::modules::network_popup`] takes — so it stacks above the bar
//! without claiming exclusive zone.

use crate::theme;
use gpui::{
    actions, div, layer_shell::*, point, prelude::*, px, App, AppContext, Bounds, Context,
    DisplayId, FocusHandle, Focusable, KeyBinding, MouseButton, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
};
use ztheme::Theme;

actions!(zbar_settings, [Dismiss]);

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", Dismiss, Some("SettingsPopup"))]
}

pub struct SettingsModule {
    display_id: Option<DisplayId>,
    popup: Option<WindowHandle<SettingsPopup>>,
}

impl SettingsModule {
    pub fn new(display_id: Option<DisplayId>) -> Self {
        Self {
            display_id,
            popup: None,
        }
    }

    fn toggle(&mut self, cx: &mut App) {
        if let Some(handle) = self.popup.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            return;
        }

        let opts = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(220.), px(120.)),
            })),
            display_id: self.display_id,
            app_id: Some("zbar-settings".to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "zbar-settings".to_string(),
                layer: Layer::Top,
                anchor: Anchor::TOP | Anchor::RIGHT,
                margin: Some((px(0.), px(8.), px(0.), px(0.))),
                keyboard_interactivity: KeyboardInteractivity::OnDemand,
                exclusive_zone: None,
                ..Default::default()
            }),
            ..Default::default()
        };

        match cx.open_window(opts, |_, cx| cx.new(SettingsPopup::new)) {
            Ok(handle) => self.popup = Some(handle),
            Err(e) => tracing::warn!("settings: failed to open popup: {e}"),
        }
    }
}

impl Render for SettingsModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().clone();
        let t = *cx.global::<Theme>();
        // Pulled out of theme::pill so we can attach a click handler — the
        // shared helper would need cx for the click anyway.
        div()
            .id("zbar-settings-pill")
            .px(theme::PILL_PX)
            .py(theme::PILL_PY)
            .rounded_md()
            .bg(t.surface)
            .hover(move |s| s.bg(t.surface_hover))
            .text_color(t.fg)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                entity.update(cx, |m, cx| m.toggle(cx));
            })
            .child("\u{2699}")
    }
}

// ---------------------------------------------------------------------------
// Popup
// ---------------------------------------------------------------------------

pub struct SettingsPopup {
    focus_handle: FocusHandle,
}

impl SettingsPopup {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }
}

impl Focusable for SettingsPopup {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.global::<Theme>();
        let active = ztheme::name_for(&t);

        let mut col = div()
            .key_context("SettingsPopup")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .bg(t.bg)
            .text_color(t.fg)
            .text_size(theme::FONT_SIZE)
            .border_1()
            .border_color(t.border)
            .rounded_md()
            .p_2()
            .flex()
            .flex_col()
            .gap_1();

        col = col.child(
            div()
                .text_color(t.fg_dim)
                .text_size(px(11.0))
                .child("Theme"),
        );

        col = col.child(theme_row(
            "catppuccin-mocha",
            "Catppuccin Mocha",
            active,
            &t,
        ));
        col = col.child(theme_row(
            "catppuccin-latte",
            "Catppuccin Latte",
            active,
            &t,
        ));
        col
    }
}

fn theme_row(
    name: &'static str,
    label: &'static str,
    active: Option<&str>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let is_selected = active == Some(name);
    let bg = if is_selected {
        t.accent_soft
    } else {
        gpui::Hsla::transparent_black()
    };
    let fg = if is_selected { t.accent } else { t.fg };
    div()
        .id(gpui::ElementId::Name(format!("theme-row-{name}").into()))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .hover({
            let hover_bg = t.hover_bg;
            move |s| s.bg(hover_bg)
        })
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            // Persist first — the watcher will broadcast the change to
            // every running zbar / zofi anyway, but also update this
            // process's global immediately so the click feels instant.
            if let Err(e) = ztheme::save(name) {
                tracing::warn!(theme = name, error = %e, "settings: save theme failed");
                return;
            }
            let new_theme = ztheme::theme_from_name(name);
            cx.set_global::<Theme>(new_theme);
            cx.refresh_windows();
            window.remove_window();
        })
        .child(
            div()
                .w(px(14.0))
                .child(if is_selected { "\u{2713}" } else { " " }),
        )
        .child(div().flex_1().child(label))
}
