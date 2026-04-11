use std::sync::Arc;
use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::*,
};
use zbar::backend::WorkspaceBackend;
use zbar::modules::clock::ClockModule;
use zbar::modules::workspaces::WorkspacesModule;
use zbar::theme;

pub struct Bar {
    workspaces: Entity<WorkspacesModule>,
    clock: Entity<ClockModule>,
}

impl Bar {
    pub fn new(
        backend: Option<Arc<dyn WorkspaceBackend>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspaces = cx.new(|cx| WorkspacesModule::new(backend, cx));
        let clock = cx.new(ClockModule::new);
        Bar { workspaces, clock }
    }
}

impl Render for Bar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .px(theme::PADDING_X)
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(theme::FONT_SIZE)
            .child(
                div().flex_1().flex().items_center().gap(theme::MODULE_GAP)
                    .child(self.workspaces.clone())
            )
            .child(
                div().flex_1().flex().items_center().justify_center()
                    .child("title")
            )
            .child(
                div().flex_1().flex().items_center().justify_end()
                    .child(self.clock.clone())
            )
    }
}
