use gpui::{
    div, prelude::*, px, App, Context, DisplayId, Entity, IntoElement, ParentElement, Render,
    Styled, Window,
};
use std::sync::Arc;
use zbar::backend::WorkspaceBackend;
use zbar::modules::battery::BatteryModule;
use zbar::modules::brightness::BrightnessModule;
use zbar::modules::cpu_mem::CpuMemModule;
use zbar::modules::network::NetworkModule;
// Clock lives inside the QuickSettings cluster; no standalone clock module.
use zbar::modules::quicksettings::QuickSettingsModule;
use zbar::modules::settings::SettingsModule;
pub use zbar::modules::tray::TrayModule;
use zbar::modules::volume::VolumeModule;
pub use zbar::modules::window_title::WindowTitleModule;
use zbar::modules::workspaces::WorkspacesModule;
use zbar::theme;
use ztheme::Theme;

pub struct Bar {
    display_id: Option<DisplayId>,
    workspaces: Entity<WorkspacesModule>,
    window_title: Entity<WindowTitleModule>,
    tray: Entity<TrayModule>,
    network: Entity<NetworkModule>,
    volume: Entity<VolumeModule>,
    brightness: Entity<BrightnessModule>,
    battery: Entity<BatteryModule>,
    cpu_mem: Entity<CpuMemModule>,
    settings: Entity<SettingsModule>,
    quicksettings: Entity<QuickSettingsModule>,
}

impl Bar {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        display_id: Option<DisplayId>,
        output_name: Option<String>,
        tray: Entity<TrayModule>,
        window_title: Entity<WindowTitleModule>,
        volume: Entity<VolumeModule>,
        brightness: Entity<BrightnessModule>,
        battery: Entity<BatteryModule>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_global::<Theme>(|_, cx| cx.notify()).detach();
        let network = cx.new(|cx| NetworkModule::new(display_id, cx));
        let quicksettings = cx.new(|cx| {
            QuickSettingsModule::new(
                display_id,
                volume.clone(),
                brightness.clone(),
                battery.clone(),
                network.clone(),
                cx,
            )
        });
        Bar {
            display_id,
            workspaces: cx.new(|cx| WorkspacesModule::new(backend, output_name, cx)),
            // WindowTitleModule subscribes to a compositor IPC stream (sway
            // socket or `niri msg` subprocess) — one per process is plenty.
            window_title,
            // TrayModule holds a DBus SNI host — only one can run per process,
            // so the same Entity is shared across all bars. GPUI renders it
            // correctly in every window, and `cx.notify()` re-renders them all.
            tray,
            network,
            volume,
            brightness,
            battery,
            cpu_mem: cx.new(CpuMemModule::new),
            settings: cx.new(|cx| SettingsModule::new(display_id, cx)),
            quicksettings,
        }
    }
}

fn separator(cx: &App) -> impl IntoElement {
    let t = cx.global::<Theme>();
    div().h(px(14.0)).w_px().bg(t.separator)
}

impl Render for Bar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity().entity_id();
        let t = *cx.global::<Theme>();
        // Stamp the tray entity with our display_id so its right-click
        // handlers — built during the upcoming render — capture the right
        // output for popup placement.
        let bar_display = self.display_id;
        self.tray
            .update(cx, |tray, _| tray.set_render_display(bar_display));
        div()
            .size_full()
            .flex()
            .items_center()
            .px(theme::PADDING_X)
            .bg(t.bg)
            .text_color(t.fg)
            .text_size(theme::FONT_SIZE)
            .on_mouse_move(move |_: &gpui::MouseMoveEvent, _window, cx| cx.notify(entity_id))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .child(self.workspaces.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.window_title.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(theme::MODULE_GAP)
                    .child(self.tray.clone())
                    .child(separator(cx))
                    .child(self.network.clone())
                    .child(separator(cx))
                    .child(self.volume.clone())
                    .child(self.brightness.clone())
                    .child(separator(cx))
                    .child(self.cpu_mem.clone())
                    .child(self.battery.clone())
                    .child(separator(cx))
                    .child(self.quicksettings.clone())
                    .child(separator(cx))
                    .child(self.settings.clone()),
            )
    }
}
