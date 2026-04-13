use gpui::{
    div, prelude::*, px, Context, DisplayId, Entity, IntoElement, ParentElement, Render, Styled,
    Window,
};
use std::sync::Arc;
use zbar::backend::WorkspaceBackend;
use zbar::modules::battery::BatteryModule;
use zbar::modules::brightness::BrightnessModule;
use zbar::modules::clock::ClockModule;
use zbar::modules::cpu_mem::CpuMemModule;
use zbar::modules::network::NetworkModule;
use zbar::modules::volume::VolumeModule;
use zbar::modules::window_title::WindowTitleModule;
use zbar::modules::workspaces::WorkspacesModule;
use zbar::theme;

pub struct Bar {
    workspaces: Entity<WorkspacesModule>,
    window_title: Entity<WindowTitleModule>,
    network: Entity<NetworkModule>,
    volume: Entity<VolumeModule>,
    brightness: Entity<BrightnessModule>,
    battery: Entity<BatteryModule>,
    cpu_mem: Entity<CpuMemModule>,
    clock: Entity<ClockModule>,
}

impl Bar {
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        display_id: Option<DisplayId>,
        cx: &mut Context<Self>,
    ) -> Self {
        Bar {
            workspaces: cx.new(|cx| WorkspacesModule::new(backend, cx)),
            window_title: cx.new(WindowTitleModule::new),
            network: cx.new(|cx| NetworkModule::new(display_id, cx)),
            volume: cx.new(VolumeModule::new),
            brightness: cx.new(BrightnessModule::new),
            battery: cx.new(BatteryModule::new),
            cpu_mem: cx.new(CpuMemModule::new),
            clock: cx.new(ClockModule::new),
        }
    }
}

fn separator() -> impl IntoElement {
    div().h(px(14.0)).w_px().bg(theme::separator())
}

impl Render for Bar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity().entity_id();
        div()
            .size_full()
            .flex()
            .items_center()
            .px(theme::PADDING_X)
            .bg(theme::bg())
            .text_color(theme::fg())
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
                    .child(self.network.clone())
                    .child(separator())
                    .child(self.volume.clone())
                    .child(self.brightness.clone())
                    .child(separator())
                    .child(self.cpu_mem.clone())
                    .child(self.battery.clone())
                    .child(separator())
                    .child(self.clock.clone()),
            )
    }
}
