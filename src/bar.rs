use gpui::{
    Context, IntoElement, ParentElement, Render, Styled, Window,
    div,
};
use crate::theme;

pub struct Bar {
    // Module handles will be added in later tasks.
}

impl Bar {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Bar {}
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
            // Left segment
            .child(
                div().flex_1().flex().items_center().gap(theme::MODULE_GAP)
                    .child("workspaces")
            )
            // Center segment
            .child(
                div().flex_1().flex().items_center().justify_center()
                    .child("title")
            )
            // Right segment
            .child(
                div().flex_1().flex().items_center().justify_end()
                    .child("clock")
            )
    }
}
