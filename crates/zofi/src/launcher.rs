use gpui::{
    actions, div, img, prelude::*, px, uniform_list, AnyElement, App, Context, Entity, FocusHandle,
    Focusable, FontWeight, HighlightStyle, KeyBinding, MouseButton, ObjectFit, ScrollStrategy,
    StyledText, UniformListScrollHandle, Window,
};

use crate::highlight;
use crate::input::TextInput;
use crate::source::{ActivateOutcome, Layout, Preview, Source};
use crate::theme;
use crate::SOURCES;

const PREVIEW_TEXT_MAX_LINES: usize = 200;

actions!(
    zofi,
    [
        MoveUp,
        MoveDown,
        Confirm,
        Dismiss,
        NextSource,
        PrevSource,
        ToggleMimePane
    ]
);

pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("up", MoveUp, Some("Launcher")),
        KeyBinding::new("down", MoveDown, Some("Launcher")),
        KeyBinding::new("ctrl-p", MoveUp, Some("Launcher")),
        KeyBinding::new("ctrl-n", MoveDown, Some("Launcher")),
        KeyBinding::new("enter", Confirm, Some("Launcher")),
        KeyBinding::new("tab", ToggleMimePane, Some("Launcher")),
        KeyBinding::new("ctrl-tab", NextSource, Some("Launcher")),
        KeyBinding::new("ctrl-shift-tab", PrevSource, Some("Launcher")),
        KeyBinding::new("escape", Dismiss, None),
    ];
    bindings.extend(crate::input::input_key_bindings());
    bindings
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum LeftPane {
    Items,
    Mimes,
}

/// Cursor + scroll state for one of the left-column lists. Items pane and
/// mimes pane each own one of these.
struct Pane {
    selected: usize,
    scroll: UniformListScrollHandle,
}

impl Pane {
    fn new() -> Self {
        Self {
            selected: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
    }

    fn move_down(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
    }

    fn reset(&mut self) {
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
}

pub struct Launcher {
    sources: Vec<Option<Box<dyn Source>>>,
    active: usize,
    filtered: Vec<usize>,
    items: Pane,
    mimes: Pane,
    /// Toggled by Tab — only meaningful when the selected item has ≥2 mimes.
    left_pane: LeftPane,
    /// Captured at toggle time so per-frame render avoids cloning Vec<String>
    /// out of the source on every list row. Empty in `Items` mode.
    mime_cache: Vec<String>,
    /// Index of `Source::primary_mime` within `mime_cache`, or `usize::MAX`.
    primary_mime_ix: usize,
    text_input: Entity<TextInput>,
    focus_handle: FocusHandle,
}

impl Launcher {
    pub fn new(initial: usize, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut sources: Vec<Option<Box<dyn Source>>> = (0..SOURCES.len()).map(|_| None).collect();
        let mut new_source = (SOURCES[initial].factory)();
        wire_pulse(new_source.as_mut(), cx);
        sources[initial] = Some(new_source);
        let active = initial;

        let active_source = sources[active].as_ref().unwrap();
        let filtered = active_source.filter("");
        let placeholder = active_source.placeholder();
        let text_input = cx.new(|cx| TextInput::new(placeholder, cx));

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
            sources,
            active,
            filtered,
            items: Pane::new(),
            mimes: Pane::new(),
            left_pane: LeftPane::Items,
            mime_cache: Vec::new(),
            primary_mime_ix: usize::MAX,
            text_input,
            focus_handle: cx.focus_handle(),
        }
    }

    fn refresh_mime_cache(&mut self) {
        let item_ix = match self.filtered.get(self.items.selected) {
            Some(&i) => i,
            None => {
                self.mime_cache.clear();
                self.primary_mime_ix = usize::MAX;
                return;
            }
        };
        self.mime_cache = self.source().mimes(item_ix);
        self.primary_mime_ix = self
            .source()
            .primary_mime_index(item_ix)
            .unwrap_or(usize::MAX);
    }

    fn source(&self) -> &dyn Source {
        self.sources[self.active].as_deref().unwrap()
    }

    fn switch_source(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= SOURCES.len() || ix == self.active {
            return;
        }
        if self.sources[ix].is_none() {
            let mut new_source = (SOURCES[ix].factory)();
            wire_pulse(new_source.as_mut(), cx);
            self.sources[ix] = Some(new_source);
        }
        self.active = ix;
        self.filtered = self.sources[ix].as_ref().unwrap().filter("");
        self.items.reset();
        self.mimes.reset();
        self.left_pane = LeftPane::Items;
        self.mime_cache.clear();

        let placeholder = self.sources[ix].as_ref().unwrap().placeholder();
        self.text_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder);
            input.set_text("", cx);
        });
        window.focus(&self.text_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn update_filter(&mut self, query: &str) {
        self.filtered = self.source().filter(query);
        self.items.reset();
        self.mimes.reset();
        self.left_pane = LeftPane::Items;
        self.mime_cache.clear();
    }

    /// Re-runs the source's filter without resetting the user's cursor.
    /// Called from pulse — async sources grow `entries` while the user is
    /// already navigating; we just want the new matches to show up.
    fn refilter(&mut self, query: &str) {
        self.filtered = self.source().filter(query);
        if self.items.selected >= self.filtered.len() {
            self.items.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        match self.left_pane {
            LeftPane::Items => {
                self.items.move_up();
                self.mimes.reset();
            }
            LeftPane::Mimes => self.mimes.move_up(),
        }
        cx.notify();
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        match self.left_pane {
            LeftPane::Items => {
                self.items.move_down(self.filtered.len());
                self.mimes.reset();
            }
            LeftPane::Mimes => self.mimes.move_down(self.mime_cache.len()),
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let outcome = match self.filtered.get(self.items.selected) {
            Some(&idx) => match self.left_pane {
                LeftPane::Items => self.source().activate(idx),
                LeftPane::Mimes => match self.mime_cache.get(self.mimes.selected) {
                    Some(mime) => self.source().activate_with_mime(idx, mime),
                    None => self.source().activate(idx),
                },
            },
            None => ActivateOutcome::Quit,
        };
        match outcome {
            ActivateOutcome::Quit => cx.quit(),
            ActivateOutcome::Refresh => {
                self.text_input
                    .update(cx, |input, cx| input.set_text("", cx));
                self.update_filter("");
                cx.notify();
            }
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.left_pane == LeftPane::Mimes {
            self.left_pane = LeftPane::Items;
            cx.notify();
        } else {
            cx.quit();
        }
    }

    fn toggle_mime_pane(&mut self, _: &ToggleMimePane, _: &mut Window, cx: &mut Context<Self>) {
        match self.left_pane {
            LeftPane::Items => {
                self.refresh_mime_cache();
                if self.mime_cache.len() < 2 {
                    self.mime_cache.clear();
                    return;
                }
                self.left_pane = LeftPane::Mimes;
                self.mimes.reset();
            }
            LeftPane::Mimes => {
                self.left_pane = LeftPane::Items;
                self.mime_cache.clear();
            }
        }
        cx.notify();
    }

    fn next_source(&mut self, _: &NextSource, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active + 1) % SOURCES.len();
        self.switch_source(next, window, cx);
    }

    fn prev_source(&mut self, _: &PrevSource, window: &mut Window, cx: &mut Context<Self>) {
        let prev = (self.active + SOURCES.len() - 1) % SOURCES.len();
        self.switch_source(prev, window, cx);
    }

    fn render_row(&self, list_ix: usize) -> gpui::Stateful<gpui::Div> {
        let entry_ix = self.filtered[list_ix];
        let sel = list_ix == self.items.selected;
        let content = self.source().render_item(entry_ix, sel);

        let mut row = div().h(theme::ITEM_HEIGHT).py(px(2.0));
        if sel {
            row = row.child(
                div()
                    .size_full()
                    .mx(px(6.0))
                    .rounded(theme::ITEM_RADIUS)
                    .bg(theme::selected_bg())
                    .child(content),
            );
        } else {
            row = row
                .child(
                    div()
                        .size_full()
                        .mx(px(6.0))
                        .rounded(theme::ITEM_RADIUS)
                        .child(content),
                )
                .hover(|s| s.bg(theme::hover_bg()));
        }

        row.id(list_ix)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.dispatch_action(&Confirm);
            })
    }

    fn render_mime_row(
        &self,
        list_ix: usize,
        mime: &str,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let sel = list_ix == self.mimes.selected;
        let label = if primary {
            format!("● {mime}")
        } else {
            format!("  {mime}")
        };

        let content = div()
            .h_full()
            .px(theme::PAD_X)
            .flex()
            .items_center()
            .text_size(theme::FONT_SIZE)
            .text_color(if sel { theme::fg_accent() } else { theme::fg() })
            .child(label);

        let mut row = div().h(theme::ITEM_HEIGHT).py(px(2.0));
        if sel {
            row = row.child(
                div()
                    .size_full()
                    .mx(px(6.0))
                    .rounded(theme::ITEM_RADIUS)
                    .bg(theme::selected_bg())
                    .child(content),
            );
        } else {
            row = row
                .child(
                    div()
                        .size_full()
                        .mx(px(6.0))
                        .rounded(theme::ITEM_RADIUS)
                        .child(content),
                )
                .hover(|s| s.bg(theme::hover_bg()));
        }
        row.id(("mime", list_ix)).cursor_pointer().on_mouse_down(
            MouseButton::Left,
            move |_, _, cx| {
                cx.dispatch_action(&Confirm);
            },
        )
    }

    fn render_preview_pane(&self) -> AnyElement {
        let item_ix = match self.filtered.get(self.items.selected) {
            Some(&i) => i,
            None => return div().size_full().bg(theme::preview_bg()).into_any_element(),
        };
        let preview = match self.left_pane {
            LeftPane::Items => self.source().preview(item_ix),
            LeftPane::Mimes => match self.mime_cache.get(self.mimes.selected) {
                Some(m) => self.source().preview_for_mime(item_ix, m),
                None => self.source().preview(item_ix),
            },
        };

        let pane = div()
            .size_full()
            .bg(theme::preview_bg())
            .px(px(20.0))
            .py(px(16.0))
            .overflow_hidden();
        match preview {
            Some(Preview::Text(s)) => {
                let body: String = s
                    .lines()
                    .take(PREVIEW_TEXT_MAX_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                pane.id("preview-text")
                    .overflow_y_scroll()
                    .text_size(theme::PREVIEW_FONT_SIZE)
                    .line_height(px(22.0))
                    .text_color(theme::fg())
                    .child(body)
                    .into_any_element()
            }
            Some(Preview::Code { text, lang }) => {
                let body: String = text
                    .lines()
                    .take(PREVIEW_TEXT_MAX_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                let runs = highlight::highlight(&body, &lang, theme::fg());
                let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = runs
                    .into_iter()
                    .map(|(r, color)| {
                        (
                            r,
                            HighlightStyle {
                                color: Some(color),
                                ..Default::default()
                            },
                        )
                    })
                    .collect();
                pane.id("preview-code")
                    .overflow_y_scroll()
                    .text_size(theme::PREVIEW_FONT_SIZE)
                    .line_height(px(22.0))
                    .text_color(theme::fg())
                    .child(StyledText::new(body).with_highlights(highlights))
                    .into_any_element()
            }
            Some(Preview::Image(image)) => pane
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(image)
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain)
                        .rounded(px(4.0)),
                )
                .into_any_element(),
            None => pane
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::fg_dim())
                .text_size(theme::FONT_SIZE_SM)
                .child("(no preview)")
                .into_any_element(),
        }
    }

    fn render_source_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut bar = div().flex().items_center().gap(px(8.0)).pl(theme::PAD_X);
        for (ix, entry) in SOURCES.iter().enumerate() {
            let sel = ix == self.active;
            let id = ("source-tab", ix);
            let icon = entry.icon;
            let bg = if sel {
                theme::selected_bg()
            } else {
                gpui::transparent_black()
            };
            let fg = if sel {
                theme::fg_accent()
            } else {
                theme::fg_dim()
            };
            let entity = cx.entity().downgrade();
            let tab = div()
                .id(id)
                .cursor_pointer()
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .bg(bg)
                .text_size(px(15.0))
                .text_color(fg)
                .hover(|s| s.bg(theme::hover_bg()))
                .child(icon)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    if let Some(this) = entity.upgrade() {
                        this.update(cx, |this, cx| {
                            this.switch_source(ix, window, cx);
                            // Defer focus restore until after this event
                            // cycle — calling window.focus inline runs before
                            // GPUI's own mouse-event handling, which then
                            // moves focus to the clicked element.
                            let handle = this.text_input.focus_handle(cx);
                            cx.defer_in(window, move |_, window, cx| {
                                window.focus(&handle, cx);
                            });
                        });
                    }
                });
            bar = bar.child(tab);
        }
        bar
    }
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.filtered.len();
        let pos = if count > 0 {
            format!("{}/{}", self.items.selected + 1, count)
        } else {
            "0/0".to_string()
        };
        let empty_text = self.source().empty_text();

        let list_pane = match self.left_pane {
            LeftPane::Items if count > 0 => div()
                .size_full()
                .child(
                    uniform_list(
                        "row-list",
                        count,
                        cx.processor(|this, range: std::ops::Range<usize>, _w, _cx| {
                            range.map(|ix| this.render_row(ix)).collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.items.scroll)
                    .size_full(),
                )
                .into_any_element(),
            LeftPane::Items => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::fg_dim())
                .text_size(theme::FONT_SIZE)
                .child(empty_text)
                .into_any_element(),
            LeftPane::Mimes => div()
                .size_full()
                .child(
                    uniform_list(
                        "mime-list",
                        self.mime_cache.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _w, _cx| {
                            range
                                .map(|ix| {
                                    let mime =
                                        this.mime_cache.get(ix).map(String::as_str).unwrap_or("");
                                    this.render_mime_row(ix, mime, ix == this.primary_mime_ix)
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.mimes.scroll)
                    .size_full(),
                )
                .into_any_element(),
        };

        let layout = self.source().layout();
        let (panel_w, panel_h) = match layout {
            Layout::List => (theme::PANEL_W, theme::PANEL_H),
            Layout::ListAndPreview => (
                theme::SPLIT_LIST_W + theme::SPLIT_PREVIEW_W,
                theme::SPLIT_PANEL_H,
            ),
        };

        let body: AnyElement = match layout {
            Layout::List => div()
                .flex_1()
                .pt(px(4.0))
                .overflow_hidden()
                .child(list_pane)
                .into_any_element(),
            Layout::ListAndPreview => div()
                .flex_1()
                .flex()
                .flex_row()
                .overflow_hidden()
                .child(
                    div()
                        .w(theme::SPLIT_LIST_W)
                        .h_full()
                        .pt(px(4.0))
                        .child(list_pane),
                )
                .child(div().w(px(1.0)).h_full().bg(theme::bar_border()))
                .child(
                    div()
                        .w(theme::SPLIT_PREVIEW_W)
                        .h_full()
                        .child(self.render_preview_pane()),
                )
                .into_any_element(),
        };

        let banner = self.source().banner();
        let source_bar = self.render_source_bar(cx);

        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::next_source))
            .on_action(cx.listener(Self::prev_source))
            .on_action(cx.listener(Self::toggle_mime_pane))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.dispatch_action(&Dismiss);
            })
            .child(
                div()
                    .w(panel_w)
                    .h(panel_h)
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
                    .children(banner)
                    .child(body)
                    .child(
                        div()
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(theme::bar_border())
                            .text_size(theme::FONT_SIZE_SM)
                            .child(source_bar)
                            .child(
                                div()
                                    .pr(theme::PAD_X)
                                    .flex()
                                    .items_center()
                                    .gap(px(16.0))
                                    .child(key_hint(
                                        match self.left_pane {
                                            LeftPane::Items => "Mimes",
                                            LeftPane::Mimes => "Items",
                                        },
                                        "tab",
                                    ))
                                    .child(key_hint("Source", "ctrl-tab"))
                                    .child(key_hint("Close", "esc"))
                                    .child(key_hint("Activate", "enter")),
                            ),
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

/// Subscribe to a source's growth pulses and re-render on each. Used by
/// async sources (filesystem walk, future network fetches) so the UI
/// reflects new entries as they arrive — fzf-style.
/// Subscribe to a source's growth pulses and refresh the filtered list,
/// preserving the user's cursor position. Used by sources that grow
/// asynchronously (filesystem walk).
fn wire_pulse(source: &mut dyn Source, cx: &mut Context<Launcher>) {
    let Some(rx) = source.take_pulse() else {
        return;
    };
    cx.spawn(async move |weak, cx| {
        while rx.recv().await.is_ok() {
            let updated = weak.update(cx, |this, cx| {
                let query = this.text_input.read(cx).content().clone();
                this.refilter(&query);
                cx.notify();
            });
            if updated.is_err() {
                break;
            }
        }
    })
    .detach();
}
