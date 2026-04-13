use std::sync::Arc;

use gpui::{AnyElement, Image};

pub enum Preview {
    Text(String),
    /// Plain text + a language hint for syntax highlighting (e.g. "rs", "py").
    /// The launcher renders via the syntect-backed highlighter; unknown langs
    /// fall back to plain text.
    Code {
        text: String,
        lang: String,
    },
    Image(Arc<Image>),
}

/// What the launcher does after `activate` / `activate_with_mime` returns.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ActivateOutcome {
    /// Quit zofi. Standard launch / paste / open behavior.
    Quit,
    /// Stay open and refresh the listing. Source has mutated its own state
    /// (e.g. file picker navigated into a directory).
    Refresh,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Layout {
    /// Single panel: list only.
    List,
    /// Wider panel split into a list on the left and a preview pane on the right.
    ListAndPreview,
}

/// A data source the launcher lists, filters, renders, and activates.
pub trait Source: 'static {
    fn name(&self) -> &'static str;
    /// Single Unicode glyph used in the source switcher bar.
    fn icon(&self) -> &'static str;
    fn placeholder(&self) -> &'static str;
    fn empty_text(&self) -> &'static str;

    fn filter(&self, query: &str) -> Vec<usize>;

    /// `selected` only drives content highlight; the row container, hover, and
    /// click handling live in the launcher.
    fn render_item(&self, ix: usize, selected: bool) -> AnyElement;

    fn activate(&self, ix: usize) -> ActivateOutcome;

    /// Preview content for the given index. Only consulted when
    /// `layout()` is `ListAndPreview`.
    fn preview(&self, _ix: usize) -> Option<Preview> {
        None
    }

    /// Mime variants this entry was captured with. Returning ≥2 enables the
    /// secondary mime-list pane (Tab swaps the left column to it).
    fn mimes(&self, _ix: usize) -> Vec<String> {
        Vec::new()
    }

    /// Index into `mimes(ix)` of the variant `activate(ix)` defaults to. The
    /// launcher uses this to mark which row in the mime list is the implicit
    /// choice. Default: search by `primary_mime` string (override for O(1)).
    fn primary_mime_index(&self, ix: usize) -> Option<usize> {
        let primary = self.primary_mime(ix)?;
        self.mimes(ix).iter().position(|m| m == &primary)
    }

    /// The mime `activate(ix)` defaults to. Override at least one of this or
    /// `primary_mime_index`.
    fn primary_mime(&self, _ix: usize) -> Option<String> {
        None
    }

    /// Preview the entry under the chosen mime. Default delegates to `preview`.
    fn preview_for_mime(&self, ix: usize, _mime: &str) -> Option<Preview> {
        self.preview(ix)
    }

    /// Activate using a specific mime. Default delegates to `activate`.
    fn activate_with_mime(&self, ix: usize, _mime: &str) -> ActivateOutcome {
        self.activate(ix)
    }

    fn layout(&self) -> Layout {
        Layout::List
    }

    /// Optional notification rendered above the list (e.g. daemon-not-running
    /// warnings). The launcher reserves space for it; sources that don't need
    /// one return None.
    fn banner(&self) -> Option<AnyElement> {
        None
    }

    /// Optional pulse channel for sources that grow asynchronously (e.g. a
    /// background filesystem walk). Each `()` on the channel asks the
    /// launcher to re-render so the new entries become visible. The launcher
    /// takes the receiver once on construction; subsequent calls return None.
    fn take_pulse(&mut self) -> Option<async_channel::Receiver<()>> {
        None
    }
}
