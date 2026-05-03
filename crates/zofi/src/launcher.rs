use std::collections::HashMap;

use gpui::{
    actions, div, img, prelude::*, px, uniform_list, AnyElement, App, Context, Entity, FocusHandle,
    Focusable, FontWeight, HighlightStyle, Hsla, KeyBinding, MouseButton, ObjectFit,
    ScrollStrategy, StyledText, UniformListScrollHandle, Window,
};

use crate::highlight;
use crate::input::TextInput;
use crate::registry::SourceRegistry;
use crate::source::{
    ActivateOutcome, Layout, Preview, PreviewChrome, PreviewPill, Source, SourceMeta,
};
use crate::theme;

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
        ToggleMimePane,
        TogglePeek,
        CopyImage,
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
        // Peek: toggle full-res overlay for peekable sources (Windows).
        // Space is safe to steal — Launcher context has no space binding,
        // and we intercept it before IME inserts it into the query.
        KeyBinding::new("space", TogglePeek, Some("Launcher")),
        // Copy image to clipboard. Registered on Launcher so it fires even
        // when TextInput has focus; see `input_key_bindings` where plain
        // `ctrl-c` was moved to `ctrl-shift-c` so this binding wins.
        KeyBinding::new("ctrl-c", CopyImage, Some("Launcher")),
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

/// One position in the flat switcher bar.
///
/// The bar shows each registry entry's tabs side by side: a plain `Registry`
/// for sources without children, or `UnionAll` + one `UnionChild` per child
/// for `UnionSource`-style entries. This way the user sees every reachable
/// view as a peer pill (no nested levels), and Ctrl+Tab cycles all of them
/// in a single linear order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum BarSlot {
    /// Outer registry entry without sub-filter state (used for plain tabs).
    Registry(usize),
    /// Registry entry that exposes children, filtered to a specific child.
    /// `(registry_idx, sub_filter_idx)`.
    UnionChild(usize, usize),
    /// Registry entry that exposes children, with `sub_filter = None`
    /// (the "all" / combined view).
    UnionAll(usize),
}

impl BarSlot {
    pub(crate) fn registry_idx(self) -> usize {
        match self {
            BarSlot::Registry(i) | BarSlot::UnionAll(i) | BarSlot::UnionChild(i, _) => i,
        }
    }

    pub(crate) fn sub_filter(self) -> Option<usize> {
        match self {
            BarSlot::UnionChild(_, j) => Some(j),
            _ => None,
        }
    }
}

/// Walk every registered source and expand it into one or more flat slots.
/// Pure function so it can be unit-tested without GPUI. For each entry:
/// - if `sub_sources()` is non-empty: emit `UnionAll(i)` then one
///   `UnionChild(i, j)` per child — the union view comes first so it lines
///   up with the outer registry order.
/// - otherwise: emit a single `Registry(i)`.
pub(crate) fn build_bar_slots(sources: &[&dyn Source]) -> Vec<BarSlot> {
    let mut slots = Vec::with_capacity(sources.len());
    for (i, src) in sources.iter().enumerate() {
        let children = src.sub_sources();
        if children.is_empty() {
            slots.push(BarSlot::Registry(i));
        } else {
            slots.push(BarSlot::UnionAll(i));
            for j in 0..children.len() {
                slots.push(BarSlot::UnionChild(i, j));
            }
        }
    }
    slots
}

/// Linear next-slot index with wrap. Used by `Ctrl+Tab` (forward) when called
/// with `current + 1`, and by `Ctrl+Shift+Tab` (backward) when called with
/// `current + len - 1`. Returns 0 on empty input so callers don't have to
/// special-case it (and we have one less panic vector).
pub(crate) fn next_slot(slots: &[BarSlot], current: usize) -> usize {
    if slots.is_empty() {
        return 0;
    }
    current % slots.len()
}

/// Build prefix-char → slot-index map from sources and their children.
/// Each source's `prefix()` maps to its outer slot (Registry or UnionAll).
/// Each union child's `SourceMeta::prefix` maps to its UnionChild slot.
pub(crate) fn build_prefix_map(sources: &[&dyn Source], slots: &[BarSlot]) -> HashMap<char, usize> {
    let mut map = HashMap::new();
    for (slot_ix, slot) in slots.iter().enumerate() {
        match *slot {
            BarSlot::Registry(i) => {
                if let Some(ch) = sources[i].prefix() {
                    map.insert(ch, slot_ix);
                }
            }
            BarSlot::UnionAll(i) => {
                if let Some(ch) = sources[i].prefix() {
                    map.insert(ch, slot_ix);
                }
            }
            BarSlot::UnionChild(i, j) => {
                let children = sources[i].sub_sources();
                if let Some(meta) = children.get(j) {
                    if let Some(ch) = meta.prefix {
                        map.insert(ch, slot_ix);
                    }
                }
            }
        }
    }
    map
}

pub struct Launcher {
    registry: SourceRegistry,
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
    /// Which UnionSource child, if any, the switcher bar has narrowed to.
    /// `None` = "all" tab (or a non-union active source — then the field is
    /// just dead storage). Mirrors `Source::set_sub_filter` so the UI can
    /// render the correct selection without re-reading from the source.
    sub_filter: Option<usize>,
    /// Cached `Source::sub_sources()` for the active source. Empty for
    /// sources that don't expose children. Cached because `render_source_bar`
    /// runs every frame and we don't want to walk the source each time.
    sub_sources: Vec<SourceMeta>,
    /// Flat bar layout: every reachable view (outer + union children) as a
    /// single linear list. Built once at startup — the registry doesn't
    /// change at runtime, so the slot layout is stable.
    slots: Vec<BarSlot>,
    /// Index into `slots` of the currently-active view. Updated together
    /// with `active` / `sub_filter` whenever the user clicks a tab or
    /// presses Ctrl+Tab.
    active_slot: usize,
    /// The slot shown at startup — backspace-on-empty returns here.
    default_slot: usize,
    /// Prefix character → slot index. Built once at startup from each
    /// source's `prefix()` and each union child's `SourceMeta::prefix`.
    prefix_map: HashMap<char, usize>,
    text_input: Entity<TextInput>,
    focus_handle: FocusHandle,
    /// Last query seen by the observer — used to skip redundant
    /// re-processing when `set_text` fires a second notify.
    last_query: String,
    /// When true, render the full-resolution peek overlay instead of the
    /// normal list+preview body. Flipped by Space when the active source's
    /// `can_peek()` returns true.
    peek_active: bool,
    /// Short-lived status message (e.g. "Copied image"). Cleared on any
    /// input/selection change; no timer — the next render tick shows it, and
    /// the subsequent action wipes it.
    toast: Option<String>,
}

impl Launcher {
    pub fn new(
        mut registry: SourceRegistry,
        initial: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Wire pulses for every source up-front. Sources are built before the
        // launcher (in main) so we can't lazy-init them here, and async
        // sources need their pulse channel hooked into our cx the moment they
        // exist.
        for entry in registry.iter_mut() {
            wire_pulse(entry.source.as_mut(), cx);
        }
        let active = initial.min(registry.len().saturating_sub(1));
        let active_source = registry.get(active).source.as_ref();
        let sub_sources = active_source.sub_sources();
        let filtered = active_source.filter("");
        let placeholder = active_source.placeholder();
        let text_input = cx.new(|cx| TextInput::new(placeholder, cx));

        // Build the flat slot layout once. Borrow each entry's `&dyn Source`
        // into a temporary slice so `build_bar_slots` stays GPUI-free and
        // unit-testable. The first slot whose `registry_idx == active` and
        // sub_filter is `None` (UnionAll for unions, Registry otherwise)
        // becomes the initial active slot.
        let source_refs: Vec<&dyn Source> = registry
            .entries()
            .iter()
            .map(|e| e.source.as_ref())
            .collect();
        let slots = build_bar_slots(&source_refs);
        let active_slot = slots
            .iter()
            .position(|s| {
                s.registry_idx() == active
                    && matches!(s, BarSlot::Registry(_) | BarSlot::UnionAll(_))
            })
            .unwrap_or(0);
        let prefix_map = build_prefix_map(&source_refs, &slots);

        // Wire backspace-on-empty callback (runs inside TextInput's update,
        // but on_empty_backspace only touches Launcher state, not TextInput).
        let launcher_entity = cx.entity().downgrade();
        text_input.update(cx, |input, _cx| {
            let entity = launcher_entity.clone();
            input.set_on_empty_backspace(Box::new(move |cx| {
                if let Some(launcher) = entity.upgrade() {
                    launcher.update(cx, |this, cx| {
                        this.on_empty_backspace(cx);
                    });
                }
            }));
        });

        window.focus(&text_input.focus_handle(cx), cx);

        let launcher = Self {
            registry,
            active,
            filtered,
            items: Pane::new(),
            mimes: Pane::new(),
            left_pane: LeftPane::Items,
            mime_cache: Vec::new(),
            primary_mime_ix: usize::MAX,
            sub_filter: None,
            sub_sources,
            slots,
            active_slot,
            default_slot: active_slot,
            prefix_map,
            text_input,
            focus_handle: cx.focus_handle(),
            last_query: String::new(),
            peek_active: false,
            toast: None,
        };

        // Observe TextInput from the outside — fires AFTER TextInput's
        // update completes, so we can freely read TextInput and notify
        // the Launcher without any reentrant-borrow issues.
        cx.observe(&launcher.text_input, |this: &mut Launcher, input, cx| {
            let query = input.read(cx).content().to_string();
            this.on_input_change(&query, cx);
        })
        .detach();

        launcher
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
        self.registry.get(self.active).source.as_ref()
    }

    /// Core slot-switch: updates active source, sub-filter, panes, and
    /// filtered results. Does NOT touch TextInput or call cx.notify() —
    /// callers handle text reset and repaint to avoid observer reentrance.
    ///
    /// `current_query` is the text the user has currently typed. Callers pass
    /// it in because some sites (notably `on_empty_backspace`, which fires
    /// from inside TextInput's own backspace listener) cannot synchronously
    /// `read` TextInput without panicking the entity-map borrow check.
    fn switch_to_slot(&mut self, slot_ix: usize, current_query: &str) -> bool {
        let Some(&slot) = self.slots.get(slot_ix) else {
            return false;
        };
        let new_registry = slot.registry_idx();
        let new_sub = slot.sub_filter();
        if slot_ix == self.active_slot {
            return false;
        }
        let registry_changed = new_registry != self.active;
        self.active_slot = slot_ix;
        self.active = new_registry;
        self.sub_filter = new_sub;

        let entry = self.registry.get(new_registry);
        entry.source.set_sub_filter(new_sub);
        self.sub_sources = entry.source.sub_sources();

        let query = if registry_changed { "" } else { current_query };
        self.filtered = entry.source.filter(query);
        self.items.reset();
        self.mimes.reset();
        self.left_pane = LeftPane::Items;
        self.mime_cache.clear();
        true
    }

    /// Reset TextInput after a source switch and notify for repaint.
    /// The text_input write is deferred: this fn is reached from the
    /// `cx.observe(&text_input, ...)` callback chain, which may still be
    /// inside text_input's own update() lock — synchronously calling
    /// `text_input.update` from there panics with "already being updated".
    /// Deferring also keeps the case where we're invoked from a non-input
    /// callback (tab click / empty backspace) correct: the closure runs
    /// next tick on a clean stack.
    fn finish_slot_switch(&mut self, cx: &mut Context<Self>) {
        let placeholder = self.source().placeholder();
        self.last_query = String::new();
        let text_input = self.text_input.clone();
        let entity = cx.entity().downgrade();
        cx.defer(move |cx| {
            text_input.update(cx, |input, cx| {
                input.set_placeholder(placeholder);
                input.set_text("", cx);
            });
            if let Some(this) = entity.upgrade() {
                this.update(cx, |_, cx| cx.notify());
            }
        });
    }

    /// Full slot activation with focus restore. Used by bar tab clicks and
    /// Ctrl+Tab cycling where GPUI mouse handling may steal focus.
    fn apply_slot(&mut self, slot_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current_query = self.text_input.read(cx).content().to_string();
        if self.switch_to_slot(slot_ix, &current_query) {
            self.finish_slot_switch(cx);
            let handle = self.text_input.focus_handle(cx);
            cx.defer_in(window, move |_, window, cx| {
                window.focus(&handle, cx);
            });
        }
    }

    fn update_filter(&mut self, query: &str) {
        self.filtered = self.source().filter(query);
        self.items.reset();
        self.mimes.reset();
        self.left_pane = LeftPane::Items;
        self.mime_cache.clear();
    }

    /// Called on every text change. If the query is a single prefix char,
    /// switch source and defer clearing the input. Otherwise re-filter.
    /// Called by the observer when TextInput content changes. Runs AFTER
    /// TextInput's update completes, so we can safely read/write it.
    fn on_input_change(&mut self, query: &str, cx: &mut Context<Self>) {
        if query == self.last_query {
            return;
        }
        self.last_query = query.to_string();

        if let Some(ch) = query.chars().next() {
            if query.len() == ch.len_utf8() {
                if let Some(&slot_ix) = self.prefix_map.get(&ch) {
                    if self.switch_to_slot(slot_ix, query) {
                        self.finish_slot_switch(cx);
                    }
                    return;
                }
            }
        }
        self.update_filter(query);
        cx.notify();
    }

    /// Called when backspace is pressed on an empty input. Invoked from
    /// inside TextInput's backspace listener — TextInput is currently being
    /// updated, so we MUST NOT `read` it here. Empty by definition.
    fn on_empty_backspace(&mut self, cx: &mut Context<Self>) {
        if self.active_slot == self.default_slot {
            return;
        }
        if self.switch_to_slot(self.default_slot, "") {
            self.finish_slot_switch(cx);
        }
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
        self.toast = None;
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
        self.toast = None;
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        // Capture the MRU key before activate — activate may mutate source
        // state (file picker navigation), so reading item_key afterwards is
        // racy. The pair is `(source_name, item_key)` and is keyed on the
        // active source, not any nested child: UnionSource's item_key/name
        // already route to the right child for us.
        let record = match self.filtered.get(self.items.selected) {
            Some(&idx) => self
                .source()
                .item_key(idx)
                .map(|key| (self.source().source_name(idx), key)),
            None => None,
        };

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

        if let Some((source_name, key)) = record {
            if let Some(tracker) = self.registry.tracker() {
                tracker.record(source_name, &key);
            }
        }
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
        if self.peek_active {
            self.peek_active = false;
            cx.notify();
            return;
        }
        if self.left_pane == LeftPane::Mimes {
            self.left_pane = LeftPane::Items;
            cx.notify();
        } else {
            cx.quit();
        }
    }

    fn toggle_peek(&mut self, _: &TogglePeek, _: &mut Window, cx: &mut Context<Self>) {
        if !self.source().can_peek() {
            return;
        }
        if self.filtered.get(self.items.selected).is_none() {
            return;
        }
        self.peek_active = !self.peek_active;
        self.toast = None;
        cx.notify();
    }

    fn copy_image(&mut self, _: &CopyImage, _: &mut Window, cx: &mut Context<Self>) {
        if !self.source().can_copy_image() {
            return;
        }
        let Some(&item_ix) = self.filtered.get(self.items.selected) else {
            return;
        };
        let Some(bytes) = self.source().copy_image_bytes(item_ix) else {
            tracing::info!("copy_image: no image bytes for selection");
            self.toast = Some("No image".into());
            cx.notify();
            return;
        };
        // Hand the bytes off to zofi-clipd via IPC. The daemon records the
        // image in clipboard history and becomes the wayland selection
        // owner, so the selection survives `cx.quit()` and the user can
        // paste anywhere. Falls back to a toast if the daemon isn't running.
        let req = zofi_clipd::ipc::Request::SetSelection {
            mime: "image/png".into(),
            bytes: bytes.as_ref().clone(),
        };
        match zofi_clipd::ipc::send(&req) {
            Ok(zofi_clipd::ipc::Response::Ok) => {
                tracing::info!("copy_image: clipd holding image/png");
                cx.quit();
            }
            Ok(zofi_clipd::ipc::Response::Error { message }) => {
                tracing::warn!("copy_image: clipd refused: {message}");
                self.toast = Some(format!("Copy failed: {message}"));
                cx.notify();
            }
            Err(e) => {
                tracing::warn!("copy_image: clipd ipc failed: {e}");
                self.toast = Some(format!("Copy failed: {e} (is `zofi clipd` running?)"));
                cx.notify();
            }
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
        if self.slots.is_empty() {
            return;
        }
        let next = next_slot(&self.slots, self.active_slot + 1);
        self.apply_slot(next, window, cx);
    }

    fn prev_source(&mut self, _: &PrevSource, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.slots.len();
        if n == 0 {
            return;
        }
        let prev = next_slot(&self.slots, self.active_slot + n - 1);
        self.apply_slot(prev, window, cx);
    }

    fn render_row(&self, list_ix: usize, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let entry_ix = self.filtered[list_ix];
        let sel = list_ix == self.items.selected;
        let content = self.source().render_item(entry_ix, sel);

        let mut row = div().h(theme::ITEM_HEIGHT).py(px(2.0));
        if sel {
            row = row.child(
                div()
                    .size_full()
                    .flex()
                    .child(
                        // 3px accent bar with 6px top/bottom margin and
                        // rounded right corners — matches the mockup's
                        // status-strip silhouette rather than a full-height
                        // wall.
                        div()
                            .w(px(3.0))
                            .my(px(6.0))
                            .rounded_r(px(2.0))
                            .bg(theme::accent()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .ml(px(6.0))
                            .mr(px(6.0))
                            .rounded(theme::ITEM_RADIUS)
                            .bg(theme::accent_soft())
                            .child(content),
                    ),
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

        row.id(list_ix).cursor_pointer().on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                // Click activates the clicked row, not whichever row the
                // keyboard cursor happens to be on. Force the pane and
                // selection before dispatching so `confirm` reads them.
                this.left_pane = LeftPane::Items;
                this.items.selected = list_ix;
                this.confirm(&Confirm, window, cx);
            }),
        )
    }

    fn render_mime_row(
        &self,
        list_ix: usize,
        mime: &str,
        primary: bool,
        cx: &mut Context<Self>,
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
                    .flex()
                    .child(
                        div()
                            .w(px(3.0))
                            .my(px(6.0))
                            .rounded_r(px(2.0))
                            .bg(theme::accent()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .ml(px(6.0))
                            .mr(px(6.0))
                            .rounded(theme::ITEM_RADIUS)
                            .bg(theme::accent_soft())
                            .child(content),
                    ),
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
            cx.listener(move |this, _, window, cx| {
                this.left_pane = LeftPane::Mimes;
                this.mimes.selected = list_ix;
                this.confirm(&Confirm, window, cx);
            }),
        )
    }

    fn render_preview_pane(&self) -> AnyElement {
        let item_ix = match self.filtered.get(self.items.selected) {
            Some(&i) => i,
            None => return div().size_full().bg(theme::preview_bg()).into_any_element(),
        };
        // Header/metadata chrome is an opt-in per source. Mime view suppresses
        // it: the right pane is showing a mime variant, not the item itself,
        // so the item-level title/metadata would be misleading.
        let chrome = match self.left_pane {
            LeftPane::Items => self.source().preview_chrome(item_ix),
            LeftPane::Mimes => None,
        };
        let preview = match self.left_pane {
            LeftPane::Items => self.source().preview(item_ix),
            LeftPane::Mimes => match self.mime_cache.get(self.mimes.selected) {
                Some(m) => self.source().preview_for_mime(item_ix, m),
                None => self.source().preview(item_ix),
            },
        };

        let body_container = div().flex_1().min_h_0().overflow_hidden();
        let body: AnyElement = match preview {
            Some(Preview::Text(s)) => {
                let text: String = s
                    .lines()
                    .take(PREVIEW_TEXT_MAX_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                body_container
                    .id("preview-text")
                    .overflow_y_scroll()
                    .px(px(20.0))
                    .py(px(16.0))
                    .text_size(theme::PREVIEW_FONT_SIZE)
                    .line_height(px(22.0))
                    .text_color(theme::fg())
                    .child(text)
                    .into_any_element()
            }
            Some(Preview::Code { text, lang }) => {
                let code: String = text
                    .lines()
                    .take(PREVIEW_TEXT_MAX_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                let runs = highlight::highlight(&code, &lang, theme::fg());
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
                body_container
                    .id("preview-code")
                    .overflow_y_scroll()
                    .px(px(20.0))
                    .py(px(16.0))
                    .text_size(theme::PREVIEW_FONT_SIZE)
                    .line_height(px(22.0))
                    .text_color(theme::fg())
                    .child(StyledText::new(code).with_highlights(highlights))
                    .into_any_element()
            }
            Some(Preview::Image(image)) => body_container
                .px(px(20.0))
                .py(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    // Bordered surface fills the padded body, then the
                    // image is contained inside it. Splitting the wrapper
                    // (size_full) from the image (max_w_full + Contain)
                    // keeps the padding visible: the previous version let
                    // the img's max_h_full collapse the parent's py.
                    div()
                        .size_full()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme::panel_border())
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            img(image)
                                .max_w_full()
                                .max_h_full()
                                .object_fit(ObjectFit::Contain),
                        ),
                )
                .into_any_element(),
            None => body_container
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::fg_dim())
                .text_size(theme::FONT_SIZE_SM)
                .child("(no preview)")
                .into_any_element(),
        };

        let header = chrome.as_ref().map(preview_header);
        let footer = chrome
            .as_ref()
            .filter(|c| !c.metadata.is_empty())
            .map(preview_metadata_strip);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::preview_bg())
            .children(header)
            .child(body)
            .children(footer)
            .into_any_element()
    }

    /// Full-screen peek overlay: dark backdrop + centered full-resolution
    /// screenshot. Rendered in place of the normal panel when `peek_active`
    /// is set; all action routes stay wired so keys work identically.
    fn render_peek(&self, cx: &mut Context<Self>) -> gpui::Div {
        let item_ix = self.filtered.get(self.items.selected).copied();
        let peek_img = item_ix.and_then(|ix| self.source().peek_image(ix));
        let status = item_ix
            .and_then(|ix| self.source().item_key(ix))
            .unwrap_or_default();

        let image_child: AnyElement = match peek_img {
            Some(img) => img_el_for_peek(img),
            None => div()
                .text_color(theme::fg_dim())
                .text_size(theme::FONT_SIZE)
                .child("(no preview available)")
                .into_any_element(),
        };

        let toast_bar = self.toast.as_ref().map(|t| {
            div()
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(theme::selected_bg())
                .text_color(theme::fg_accent())
                .text_size(theme::FONT_SIZE_SM)
                .child(t.clone())
        });

        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::toggle_peek))
            .on_action(cx.listener(Self::copy_image))
            .size_full()
            .bg(gpui::rgba(0x0000_00cc))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.dispatch_action(&TogglePeek);
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .text_size(theme::FONT_SIZE_SM)
                    .text_color(theme::fg_dim())
                    .child(
                        div()
                            .text_color(theme::fg_accent())
                            .font_weight(FontWeight::MEDIUM)
                            .child(status),
                    )
                    .child(div().child("space exit · ctrl-c copy · enter switch · ↑↓ next"))
                    .children(toast_bar),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    // Swallow clicks on the image so only the backdrop exits peek.
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(image_child),
            )
    }

    fn render_source_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut bar = div().flex().items_center().gap(px(4.0));
        // Flat layout: every reachable view is a peer pill. UnionAll uses
        // its outer registry icon; UnionChild uses its child's icon. This
        // collapses the previous two-level (outer tabs + sub-filter tabs)
        // navigation into one row — there are no nested levels to discover.
        for (ix, slot) in self.slots.iter().enumerate() {
            let icon = self.slot_icon(*slot);
            let label = self.slot_label(*slot);
            let tint = self.slot_tint(*slot);
            let selected = ix == self.active_slot;
            let tab = tab_pill(
                ("bar-slot", ix),
                icon,
                label,
                tint,
                selected,
                cx.entity().downgrade(),
                move |this, window, cx| this.apply_slot(ix, window, cx),
            );
            bar = bar.child(tab);
        }
        bar
    }

    /// Resolve the glyph for a slot. Cheap (constant string per registry/
    /// child entry) so we re-derive it per render rather than caching.
    fn slot_icon(&self, slot: BarSlot) -> &'static str {
        match slot {
            BarSlot::Registry(i) | BarSlot::UnionAll(i) => self.registry.get(i).icon(),
            BarSlot::UnionChild(i, j) => {
                let children = self.registry.get(i).source.sub_sources();
                // Defensive: if the child set shrank between startup and
                // now (shouldn't happen — registry is immutable at runtime
                // — but UnionSource::sub_sources isn't statically frozen)
                // fall back to the parent's icon rather than panicking.
                children
                    .get(j)
                    .map(|m| m.icon)
                    .unwrap_or(self.registry.get(i).icon())
            }
        }
    }

    /// Human label shown alongside the slot icon in the source bar.
    /// "UnionAll" collapses to "All" so the union tab reads as the
    /// combined view rather than whatever arbitrary name the registry
    /// gave the union source (usually "all" / "launch").
    fn slot_label(&self, slot: BarSlot) -> String {
        match slot {
            BarSlot::UnionAll(_) => "All".into(),
            BarSlot::Registry(i) => short_label(self.registry.get(i).name()),
            BarSlot::UnionChild(i, j) => {
                let children = self.registry.get(i).source.sub_sources();
                children
                    .get(j)
                    .map(|m| short_label(m.name))
                    .unwrap_or_else(|| short_label(self.registry.get(i).name()))
            }
        }
    }

    /// Collect `(prefix_char, short_name)` pairs for rendering as chip
    /// hints next to the search input. Walks the registry in registration
    /// order, and for union sources expands out to each child's prefix (so
    /// e.g. `@ win` and `> apps` both show up even though they're children
    /// of a single union). Deduped by prefix char; first occurrence wins.
    fn prefix_hints(&self) -> Vec<(char, String)> {
        let mut out: Vec<(char, String)> = Vec::new();
        let mut seen: std::collections::HashSet<char> = std::collections::HashSet::new();
        for entry in self.registry.entries() {
            let children = entry.source.sub_sources();
            if children.is_empty() {
                if let Some(ch) = entry.source.prefix() {
                    if seen.insert(ch) {
                        out.push((ch, shorten_source_name(entry.source.name())));
                    }
                }
            } else {
                for meta in children {
                    if let Some(ch) = meta.prefix {
                        if seen.insert(ch) {
                            out.push((ch, shorten_source_name(meta.name)));
                        }
                    }
                }
            }
        }
        out
    }

    /// Icon tint for a slot. UnionAll uses the generic accent; per-source
    /// tabs route through `theme::category()` keyed on the source's name
    /// (or the child's name for UnionChild).
    fn slot_tint(&self, slot: BarSlot) -> Hsla {
        match slot {
            BarSlot::UnionAll(_) => theme::accent(),
            BarSlot::Registry(i) => theme::category(self.registry.get(i).name()),
            BarSlot::UnionChild(i, j) => {
                let children = self.registry.get(i).source.sub_sources();
                children
                    .get(j)
                    .map(|m| theme::category(m.name))
                    .unwrap_or_else(theme::accent)
            }
        }
    }
}

/// Uppercase the first character of `s`, leaving the rest untouched.
/// First-char-only is fine for our source names ("apps" → "Apps").
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Compact form of a source name. Currently only "clipboard" gets
/// trimmed — kept as a `match` so adding future abbreviations is a
/// one-liner. Returns the lowercase short form; capitalisation is the
/// caller's job (the source bar capitalises, the prefix chip row doesn't).
fn short_source_name(name: &str) -> &str {
    match name {
        "clipboard" => "clip",
        other => other,
    }
}

/// Tab label: capitalised, abbreviated source name (e.g. `clipboard`
/// → `Clip`). Used by the bottom source bar.
fn short_label(name: &str) -> String {
    capitalize(short_source_name(name))
}

/// Lowercase short label for prefix-hint chips next to the search input.
fn shorten_source_name(name: &str) -> String {
    short_source_name(name).to_string()
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.peek_active {
            return self.render_peek(cx).into_any_element();
        }
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
                        cx.processor(|this, range: std::ops::Range<usize>, _w, cx| {
                            range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
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
                        cx.processor(|this, range: std::ops::Range<usize>, _w, cx| {
                            range
                                .map(|ix| {
                                    let mime =
                                        this.mime_cache.get(ix).map(String::as_str).unwrap_or("");
                                    this.render_mime_row(ix, mime, ix == this.primary_mime_ix, cx)
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
        let prefix_hints = self.prefix_hints();

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
            .on_action(cx.listener(Self::toggle_peek))
            .on_action(cx.listener(Self::copy_image))
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
                    // Global type stack for the launcher chrome — Fira Sans
                    // for body, deliberately leaving monospace pills /
                    // counter / kbd to opt into Fira Code per-element below.
                    // GPUI doesn't accept a CSS-style fallback list, so we
                    // commit to one family that's reliably installed on
                    // Arch + most Linux desktops.
                    .font_family("Fira Sans")
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.0))
                            .px(px(18.0))
                            .h(px(44.0))
                            .child(div().flex_1().child(self.text_input.clone()))
                            .child(
                                // Prefix hint chips: dim pills reminding the
                                // user which character jumps to which source.
                                // Hidden when there are no registered prefixes.
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .flex_shrink_0()
                                    .font_family("Fira Code")
                                    .text_size(px(11.0))
                                    .text_color(theme::fg_dim())
                                    .children(prefix_hints.iter().map(|(ch, name)| {
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(4.0))
                                            .px(px(6.0))
                                            .py(px(2.0))
                                            .rounded(px(3.0))
                                            .bg(theme::kbd_bg())
                                            .child(
                                                div().text_color(theme::fg()).child(ch.to_string()),
                                            )
                                            .child(div().child(name.clone()))
                                    })),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family("Fira Code")
                                    .text_size(px(11.0))
                                    .text_color(theme::fg_dim())
                                    .child(pos),
                            ),
                    )
                    .child(div().h(px(1.0)).bg(theme::bar_border()))
                    .children(banner)
                    .child(body)
                    .child(
                        div()
                            .h(px(44.0))
                            .px(px(14.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(16.0))
                            .border_t_1()
                            .border_color(theme::bar_border())
                            .bg(theme::panel_bg())
                            .text_size(theme::FONT_SIZE_SM)
                            .child(source_bar)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(14.0))
                                    .child(key_hint(
                                        match self.left_pane {
                                            LeftPane::Items => "Mimes",
                                            LeftPane::Mimes => "Items",
                                        },
                                        &["tab"],
                                        KEY_NORMAL,
                                    ))
                                    .child(key_hint("Source", &["ctrl", "tab"], KEY_NORMAL))
                                    .when(self.source().can_peek(), |d| {
                                        d.child(key_hint("Peek", &["space"], KEY_NORMAL))
                                    })
                                    .when(self.source().can_copy_image(), |d| {
                                        d.child(key_hint("Copy", &["⌃C"], KEY_NORMAL))
                                    })
                                    .child(key_hint("Close", &["esc"], KEY_NORMAL))
                                    .child(key_hint("Activate", &["↵"], KEY_PRIMARY)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Factored-out switcher-bar tab: pill styling + click handler. Both the
/// multi-source and sub-filter bars use this so selection, hover, and
/// focus-restore stay in sync across the two paths.
fn tab_pill(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: String,
    tint: Hsla,
    selected: bool,
    entity: gpui::WeakEntity<Launcher>,
    on_click: impl Fn(&mut Launcher, &mut Window, &mut Context<Launcher>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let bg = if selected {
        theme::accent_soft()
    } else {
        gpui::transparent_black()
    };
    let fg = if selected {
        theme::fg_accent()
    } else {
        theme::fg_dim()
    };
    let border_color = if selected {
        theme::accent()
    } else {
        // Transparent placeholder keeps every tab the same height so the
        // underline appearing on selection doesn't shift its neighbours.
        gpui::transparent_black()
    };
    div()
        .id(id)
        .cursor_pointer()
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_b_2()
        .border_color(border_color)
        .bg(bg)
        .text_size(theme::FONT_SIZE_SM)
        .text_color(fg)
        .hover(|s| s.bg(theme::hover_bg()))
        .flex()
        .items_center()
        .gap(px(6.0))
        // Icon is rendered inside a fixed 16x16 chip so per-glyph metric
        // differences (◉ vs ◱ vs ▣) don't translate to per-tab size jitter.
        // Tint applies only to the glyph; label keeps fg/fg_dim so the
        // selected state stays the dominant visual signal.
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .rounded(px(3.0))
                .bg(theme::kbd_bg())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.0))
                .text_color(tint)
                .child(icon),
        )
        .child(div().child(label))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            if let Some(this) = entity.upgrade() {
                this.update(cx, |this, cx| on_click(this, window, cx));
            }
        })
}

/// Centered full-res peek image, scaled down with `Contain` so a 4K capture
/// still fits the overlay without cropping.
fn img_el_for_peek(image: std::sync::Arc<gpui::Image>) -> AnyElement {
    img(image)
        .max_w_full()
        .max_h_full()
        .object_fit(ObjectFit::Contain)
        .rounded(px(6.0))
        .into_any_element()
}

/// Title + status pills above the preview body. The whole thing is one
/// row separated from the body by a bottom border — lines up with the
/// search-bar separator style.
fn preview_header(c: &PreviewChrome) -> gpui::Div {
    let mut row = div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(16.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(theme::panel_border())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(14.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::fg())
                .child(c.title.clone()),
        );
    for pill in &c.pills {
        row = row.child(render_pill(pill));
    }
    row
}

/// Single status pill: green on translucent green for `active=true`,
/// neutral dim for everything else.
fn render_pill(p: &PreviewPill) -> gpui::Div {
    let (fg, bg) = if p.active {
        (theme::pill_active_fg(), theme::pill_active_bg())
    } else {
        (theme::fg_dim(), theme::kbd_bg())
    };
    let mut row = div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(999.0))
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.0));
    if p.active {
        // Solid green dot to read as "live indicator", matching the
        // status-led convention from the mockup.
        row = row.child(
            div()
                .w(px(6.0))
                .h(px(6.0))
                .rounded(px(999.0))
                .bg(theme::pill_active_fg()),
        );
    }
    row.child(p.text.clone())
}

/// Bottom metadata strip: `(label, value)` pairs in a dim monospaced row.
/// Caller is responsible for skipping this when `metadata` is empty —
/// rendering an empty border would just leave a visual hairline.
fn preview_metadata_strip(c: &PreviewChrome) -> gpui::Div {
    let mut row = div()
        .flex()
        .gap(px(18.0))
        .px(px(16.0))
        .py(px(8.0))
        .border_t_1()
        .border_color(theme::panel_border())
        .text_size(px(11.0))
        .text_color(theme::fg_dim());
    for (k, v) in &c.metadata {
        row = row.child(
            div()
                .flex()
                .gap(px(6.0))
                .child(div().text_color(theme::fg()).child(k.clone()))
                .child(div().child(v.clone())),
        );
    }
    row
}

/// `primary=true` flag for [`key_hint`] — used for the dominant action
/// (typically `Activate ↵`) so the bottom bar reads with a clear focal
/// point. Named constants beat bare `true`/`false` at the call sites.
const KEY_PRIMARY: bool = true;
const KEY_NORMAL: bool = false;

fn key_hint(label: &str, keys: &[&str], primary: bool) -> gpui::Div {
    let label_color = if primary {
        gpui::white()
    } else {
        theme::fg_accent()
    };
    let label_weight = if primary {
        FontWeight::BOLD
    } else {
        FontWeight::MEDIUM
    };

    let key_bg = if primary {
        theme::kbd_accent_bg()
    } else {
        theme::kbd_bg()
    };
    let key_fg = if primary {
        theme::kbd_accent_fg()
    } else {
        theme::kbd_fg()
    };
    let key_border = if primary {
        theme::kbd_accent_border()
    } else {
        theme::kbd_border()
    };

    let mut row = div().flex().items_center().gap(px(6.0)).child(
        div()
            .text_color(label_color)
            .font_weight(label_weight)
            .child(label.to_string()),
    );
    // Render each key as its own kbd pill so combos read as
    // `ctrl` `tab` rather than one chunky `ctrl-tab` blob — matches
    // the keycap convention from the mockup.
    let kbd_row = div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .children(keys.iter().map(|k| {
            div()
                .px(px(5.0))
                .py(px(1.0))
                .min_w(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.0))
                .border_1()
                .border_b_2()
                .bg(key_bg)
                .border_color(key_border)
                .text_color(key_fg)
                .text_size(px(10.5))
                .font_family("Fira Code")
                .child(k.to_string())
        }));
    row = row.child(kbd_row);
    row
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ActivateOutcome, SourceMeta};
    use gpui::AnyElement;

    /// Stub source: optionally exposes a fixed list of children. Built
    /// purely to exercise `build_bar_slots` without GPUI.
    struct Stub {
        name: &'static str,
        children: Vec<SourceMeta>,
        prefix_ch: Option<char>,
    }

    impl Stub {
        fn plain(name: &'static str) -> Self {
            Self {
                name,
                children: Vec::new(),
                prefix_ch: None,
            }
        }
        fn with_children(name: &'static str, children: Vec<SourceMeta>) -> Self {
            Self {
                name,
                children,
                prefix_ch: None,
            }
        }
        fn with_prefix(mut self, ch: char) -> Self {
            self.prefix_ch = Some(ch);
            self
        }
    }

    impl Source for Stub {
        fn name(&self) -> &'static str {
            self.name
        }
        fn icon(&self) -> &'static str {
            "x"
        }
        fn prefix(&self) -> Option<char> {
            self.prefix_ch
        }
        fn placeholder(&self) -> &'static str {
            ""
        }
        fn empty_text(&self) -> &'static str {
            ""
        }
        fn filter(&self, _: &str) -> Vec<usize> {
            Vec::new()
        }
        fn render_item(&self, _: usize, _: bool) -> AnyElement {
            unimplemented!("not used in slot tests")
        }
        fn activate(&self, _: usize) -> ActivateOutcome {
            ActivateOutcome::Quit
        }
        fn sub_sources(&self) -> Vec<SourceMeta> {
            self.children.clone()
        }
    }

    fn meta(name: &'static str) -> SourceMeta {
        SourceMeta {
            name,
            icon: "?",
            prefix: None,
        }
    }

    fn meta_with_prefix(name: &'static str, ch: char) -> SourceMeta {
        SourceMeta {
            name,
            icon: "?",
            prefix: Some(ch),
        }
    }

    fn refs(v: &[Box<dyn Source>]) -> Vec<&dyn Source> {
        v.iter().map(|b| b.as_ref()).collect()
    }

    #[test]
    fn build_bar_slots_flat_registry_yields_one_slot_per_entry() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Stub::plain("apps")),
            Box::new(Stub::plain("files")),
            Box::new(Stub::plain("clipboard")),
        ];
        let slots = build_bar_slots(&refs(&sources));
        assert_eq!(
            slots,
            vec![
                BarSlot::Registry(0),
                BarSlot::Registry(1),
                BarSlot::Registry(2),
            ]
        );
    }

    #[test]
    fn build_bar_slots_expands_union_into_all_plus_children() {
        // Mirrors the production composition: union(2 children), files,
        // clipboard → expect 5 flat slots in a fixed order.
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Stub::with_children(
                "launch",
                vec![meta("windows"), meta("apps")],
            )),
            Box::new(Stub::plain("files")),
            Box::new(Stub::plain("clipboard")),
        ];
        let slots = build_bar_slots(&refs(&sources));
        assert_eq!(
            slots,
            vec![
                BarSlot::UnionAll(0),
                BarSlot::UnionChild(0, 0),
                BarSlot::UnionChild(0, 1),
                BarSlot::Registry(1),
                BarSlot::Registry(2),
            ]
        );
    }

    #[test]
    fn build_bar_slots_empty_registry_yields_no_slots() {
        let sources: Vec<Box<dyn Source>> = vec![];
        let slots = build_bar_slots(&refs(&sources));
        assert!(slots.is_empty());
    }

    #[test]
    fn next_slot_wraps_at_end() {
        let slots = vec![
            BarSlot::Registry(0),
            BarSlot::Registry(1),
            BarSlot::Registry(2),
        ];
        // Ctrl+Tab from last slot: caller passes current+1 = 3 → wraps to 0.
        assert_eq!(next_slot(&slots, 3), 0);
        // Mid-cycle: current+1 = 2 → 2 (no wrap).
        assert_eq!(next_slot(&slots, 2), 2);
        // Backwards from slot 0: caller passes current+len-1 = 2 → 2.
        assert_eq!(next_slot(&slots, 2), 2);
    }

    #[test]
    fn next_slot_handles_empty_input_without_panic() {
        let slots: Vec<BarSlot> = Vec::new();
        // Modulo would divide by zero — function must short-circuit.
        assert_eq!(next_slot(&slots, 0), 0);
        assert_eq!(next_slot(&slots, 99), 0);
    }

    #[test]
    fn bar_slot_registry_idx_and_sub_filter_round_trip() {
        // Both fields in one place so the (registry, Option<sub>) tuple
        // the launcher reads stays glued to the slot variant definition.
        assert_eq!(BarSlot::Registry(2).registry_idx(), 2);
        assert_eq!(BarSlot::Registry(2).sub_filter(), None);
        assert_eq!(BarSlot::UnionAll(0).registry_idx(), 0);
        assert_eq!(BarSlot::UnionAll(0).sub_filter(), None);
        assert_eq!(BarSlot::UnionChild(0, 1).registry_idx(), 0);
        assert_eq!(BarSlot::UnionChild(0, 1).sub_filter(), Some(1));
    }

    // -- prefix_map tests --

    #[test]
    fn prefix_map_maps_plain_source_prefix_to_registry_slot() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Stub::plain("apps")),
            Box::new(Stub::plain("files").with_prefix('/')),
        ];
        let r = refs(&sources);
        let slots = build_bar_slots(&r);
        let map = build_prefix_map(&r, &slots);
        assert_eq!(map.get(&'/'), Some(&1)); // Registry(1)
        assert!(!map.contains_key(&'>'));
    }

    #[test]
    fn prefix_map_maps_union_child_prefix_to_child_slot() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Stub::with_children(
                "launch",
                vec![
                    meta_with_prefix("windows", '@'),
                    meta_with_prefix("apps", '>'),
                ],
            )),
            Box::new(Stub::plain("files").with_prefix('/')),
        ];
        let r = refs(&sources);
        let slots = build_bar_slots(&r);
        // slots: UnionAll(0), UnionChild(0,0), UnionChild(0,1), Registry(1)
        let map = build_prefix_map(&r, &slots);
        assert_eq!(map.get(&'@'), Some(&1)); // UnionChild(0,0)
        assert_eq!(map.get(&'>'), Some(&2)); // UnionChild(0,1)
        assert_eq!(map.get(&'/'), Some(&3)); // Registry(1)
    }

    #[test]
    fn prefix_map_empty_when_no_prefixes() {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(Stub::plain("apps")),
            Box::new(Stub::plain("files")),
        ];
        let r = refs(&sources);
        let slots = build_bar_slots(&r);
        let map = build_prefix_map(&r, &slots);
        assert!(map.is_empty());
    }
}
