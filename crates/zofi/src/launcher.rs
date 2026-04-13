use gpui::{
    actions, div, prelude::*, px, uniform_list, App, Context, Entity, FocusHandle, Focusable,
    FontWeight, KeyBinding, MouseButton, ScrollStrategy, UniformListScrollHandle, Window,
};

use crate::input::TextInput;
use crate::source::Source;
use crate::theme;

actions!(zofi, [MoveUp, MoveDown, Confirm, Dismiss]);

pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("up", MoveUp, Some("Launcher")),
        KeyBinding::new("down", MoveDown, Some("Launcher")),
        KeyBinding::new("ctrl-p", MoveUp, Some("Launcher")),
        KeyBinding::new("ctrl-n", MoveDown, Some("Launcher")),
        KeyBinding::new("enter", Confirm, Some("Launcher")),
        KeyBinding::new("escape", Dismiss, None),
    ];
    bindings.extend(crate::input::input_key_bindings());
    bindings
}

pub struct Launcher {
    source: Box<dyn Source>,
    filtered: Vec<usize>,
    selected: usize,
    text_input: Entity<TextInput>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
}

impl Launcher {
    pub fn new(source: Box<dyn Source>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filtered = source.filter("");
        let text_input = cx.new(|cx| TextInput::new(source.placeholder(), cx));

        let launcher_entity = cx.entity().downgrade();
        text_input.update(cx, |input, _cx| {
            input.set_on_change(Box::new(move |query, cx| {
                if let Some(launcher) = launcher_entity.upgrade() {
                    launcher.update(cx, |this, cx| {
                        this.update_filter(query);
                        cx.notify();
                    });
                }
            }));
        });

        window.focus(&text_input.focus_handle(cx), cx);

        Self {
            source,
            filtered,
            selected: 0,
            text_input,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
        }
    }

    fn update_filter(&mut self, query: &str) {
        self.filtered = self.source.filter(query);
        self.selected = 0;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        self.scroll_handle
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        }
        self.scroll_handle
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.source.activate(idx);
        }
        cx.quit();
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn render_row(&self, list_ix: usize) -> gpui::Stateful<gpui::Div> {
        let entry_ix = self.filtered[list_ix];
        let sel = list_ix == self.selected;
        let content = self.source.render_item(entry_ix, sel);

        let mut row = div().h(theme::ITEM_HEIGHT);
        if sel {
            row = row.child(
                div()
                    .size_full()
                    .mx(px(4.0))
                    .rounded(theme::ITEM_RADIUS)
                    .bg(theme::selected_bg())
                    .child(content),
            );
        } else {
            row = row.hover(|s| s.bg(theme::hover_bg())).child(content);
        }

        row.id(list_ix)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.dispatch_action(&Confirm);
            })
    }
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.filtered.len();
        let pos = if count > 0 {
            format!("{}/{}", self.selected + 1, count)
        } else {
            "0/0".to_string()
        };
        let empty_text = self.source.empty_text();

        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.dispatch_action(&Dismiss);
            })
            .child(
                div()
                    .w(theme::PANEL_W)
                    .h(theme::PANEL_H)
                    .flex()
                    .flex_col()
                    .bg(theme::panel_bg())
                    .rounded(theme::PANEL_RADIUS)
                    .border_1()
                    .border_color(theme::panel_border())
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px(theme::PAD_X)
                            .h(px(44.0))
                            .child(div().flex_1().child(self.text_input.clone()))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(theme::FONT_SIZE_SM)
                                    .text_color(theme::fg_dim())
                                    .child(pos),
                            ),
                    )
                    .child(div().h(px(1.0)).bg(theme::bar_border()))
                    .child(
                        div()
                            .flex_1()
                            .pt(px(4.0))
                            .overflow_hidden()
                            .child(if count > 0 {
                                div().size_full().child(
                                    uniform_list(
                                        "row-list",
                                        count,
                                        cx.processor(
                                            |this, range: std::ops::Range<usize>, _w, _cx| {
                                                range
                                                    .map(|ix| this.render_row(ix))
                                                    .collect::<Vec<_>>()
                                            },
                                        ),
                                    )
                                    .track_scroll(&self.scroll_handle)
                                    .size_full(),
                                )
                            } else {
                                div()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(theme::fg_dim())
                                    .text_size(theme::FONT_SIZE)
                                    .child(empty_text)
                            }),
                    )
                    .child(
                        div()
                            .h(px(30.0))
                            .px(theme::PAD_X)
                            .flex()
                            .items_center()
                            .justify_end()
                            .border_t_1()
                            .border_color(theme::bar_border())
                            .text_size(theme::FONT_SIZE_SM)
                            .gap(px(16.0))
                            .child(key_hint("Close", "esc"))
                            .child(key_hint("Open", "enter")),
                    ),
            )
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn key_hint(label: &str, key: &str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(
            div()
                .text_color(theme::fg_accent())
                .font_weight(FontWeight::MEDIUM)
                .child(label.to_string()),
        )
        .child(div().text_color(theme::fg_dim()).child(key.to_string()))
}
