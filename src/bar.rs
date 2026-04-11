use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::*,
};
use crate::modules::clock::ClockModule;
use crate::theme;

pub struct Bar {
    clock: Entity<ClockModule>,
}

impl Bar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let clock = cx.new(ClockModule::new);
        Bar { clock }
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
                    .child("workspaces")
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
