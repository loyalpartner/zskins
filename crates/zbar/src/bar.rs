use gpui::{
    div, prelude::*, px, App, Context, DisplayId, Entity, IntoElement, ParentElement, Render,
    Styled, Window,
};
use std::sync::Arc;
use zbar::backend::WorkspaceBackend;
use zbar::modules::battery::BatteryModule;
use zbar::modules::brightness::BrightnessModule;
use zbar::modules::clock::ClockModule;
use zbar::modules::cpu_mem::CpuMemModule;
use zbar::modules::network::NetworkModule;
use zbar::modules::settings::SettingsModule;
pub use zbar::modules::tray::TrayModule;
use zbar::modules::volume::VolumeModule;
pub use zbar::modules::window_title::WindowTitleModule;
use zbar::modules::workspaces::WorkspacesModule;
use zbar::theme;
use ztheme::Theme;

pub struct Bar {
    workspaces: Entity<WorkspacesModule>,
    window_title: Entity<WindowTitleModule>,
    tray: Entity<TrayModule>,
    network: Entity<NetworkModule>,
    volume: Entity<VolumeModule>,
    brightness: Entity<BrightnessModule>,
    battery: Entity<BatteryModule>,
    cpu_mem: Entity<CpuMemModule>,
    clock: Entity<ClockModule>,
    settings: Entity<SettingsModule>,
}

impl Bar {
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        display_id: Option<DisplayId>,
        output_name: Option<String>,
        tray: Entity<TrayModule>,
        window_title: Entity<WindowTitleModule>,
        cx: &mut Context<Self>,
    ) -> Self {
        Bar {
            workspaces: cx.new(|cx| WorkspacesModule::new(backend, output_name, cx)),
            // WindowTitleModule subscribes to a compositor IPC stream (sway
            // socket or `niri msg` subprocess) — one per process is plenty.
            window_title,
            // TrayModule holds a DBus SNI host — only one can run per process,
            // so the same Entity is shared across all bars. GPUI renders it
            // correctly in every window, and `cx.notify()` re-renders them all.
            tray,
            network: cx.new(|cx| NetworkModule::new(display_id, cx)),
            volume: cx.new(VolumeModule::new),
            brightness: cx.new(BrightnessModule::new),
            battery: cx.new(BatteryModule::new),
            cpu_mem: cx.new(CpuMemModule::new),
            clock: cx.new(ClockModule::new),
            settings: cx.new(|_| SettingsModule::new(display_id)),
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
                    .child(self.clock.clone())
                    .child(separator(cx))
                    .child(self.settings.clone()),
            )
    }
}
