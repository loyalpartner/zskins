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
    /// Inspector card: a hero block (icon + title + subtitle) above a list
    /// of click-to-copy rows. Sources just supply the data; the launcher
    /// owns clipboard + toast wiring (it's the only layer with
    /// `Context<Launcher>` and the toast field). When this variant is
    /// returned, `preview_chrome` is ignored — the card has its own header.
    Inspector(InspectorCard),
}

pub struct InspectorCard {
    pub icon: Option<Arc<Image>>,
    pub title: String,
    pub subtitle: Option<String>,
    pub rows: Vec<InspectorRow>,
}

pub struct InspectorRow {
    pub label: String,
    pub value: String,
    /// Hint to render the value in monospace (paths, identifiers).
    pub mono: bool,
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

/// A small badge shown in the preview header (e.g. "active" for the
/// currently-focused window). `active=true` uses the green pill palette;
/// `active=false` uses the neutral dim palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewPill {
    pub text: String,
    pub active: bool,
}

/// Header + metadata strip chrome surrounding a preview. Sources opt in
/// via [`Source::preview_chrome`] — returning `None` keeps the preview
/// pane flush (no title bar, no bottom strip) the way it was before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewChrome {
    pub title: String,
    pub pills: Vec<PreviewPill>,
    /// (label, value) pairs rendered in a monospaced strip under the
    /// preview body — useful for stable identifiers like app IDs or
    /// desktop file stems that don't belong in the title.
    pub metadata: Vec<(String, String)>,
}

/// Metadata for a source entry in UI (name + icon glyph).
///
/// Declared here so [`Source::sub_sources`] can return it without exposing
/// concrete source types to the launcher — the launcher only needs the
/// glyph and the label when rendering the switcher bar's child-filter tabs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SourceMeta {
    pub name: &'static str,
    pub icon: &'static str,
    pub prefix: Option<char>,
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

    /// Prefix character that activates this source from the input box.
    /// Typing this character (when it's the first keystroke) switches to
    /// this source without the character appearing in the query. `None`
    /// means no prefix shortcut is registered.
    fn prefix(&self) -> Option<char> {
        None
    }

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

    /// Optional header chrome shown around the preview body — a title,
    /// status pills, and a metadata strip. Returning `None` keeps the
    /// preview pane as a plain body (the pre-chrome behavior).
    fn preview_chrome(&self, _ix: usize) -> Option<PreviewChrome> {
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

    // --- Composition hooks used by UnionSource ---
    //
    // UnionSource merges multiple sources into one ordered list. To do that it
    // needs two orthogonal signals per entry:
    //
    // * `weight`: a *static* per-entry bias (e.g. a clipboard pin, an app's
    //   launch-frequency score). Independent of the current query.
    // * `filter_scored`: the *dynamic* match quality for the current query
    //   (prefix vs substring vs fuzzy, etc.).
    //
    // Keeping them separate lets the union ranker combine them however it
    // wants (typically `weight + match_score`) without each source having to
    // know about the union. Both have defaults so existing sources compile
    // unchanged and behave exactly as before (no weight, no match scoring).

    /// Static per-entry weight. Larger values rank earlier. Default: `0`.
    fn weight(&self, _ix: usize) -> i32 {
        0
    }

    /// Score-annotated version of [`Source::filter`]. Each returned entry is
    /// `(ix, match_score)` where `match_score` distinguishes prefix / substring
    /// / fuzzy matches, etc. Default forwards to `filter` with score `0`, so
    /// sources opt in only when they have a meaningful ranking signal.
    fn filter_scored(&self, query: &str) -> Vec<(usize, i32)> {
        self.filter(query).into_iter().map(|i| (i, 0)).collect()
    }

    // --- UnionSource child-filter hooks ---
    //
    // Collapsing apps + windows + files + clipboard into a single UnionSource
    // gives one search box but loses the "only show windows" affordance the
    // multi-source switcher bar used to provide. These two defaults are a
    // non-breaking way to expose that back to the launcher without leaking
    // UnionSource's internals or requiring every source to know about it:
    // non-union sources simply return empty / no-op.

    /// Expose this source's child sources for UI filtering. Returns one entry
    /// per child (preserving registration order). Non-union sources return
    /// an empty vector — the launcher treats that as "no child tabs to show".
    fn sub_sources(&self) -> Vec<SourceMeta> {
        Vec::new()
    }

    /// Restrict subsequent `filter` / `filter_scored` calls to a single child
    /// (by index into [`Source::sub_sources`]), or `None` to cover all
    /// children. Only [`crate::sources::union::UnionSource`] honors this;
    /// other sources ignore it. The launcher must re-run `filter` after
    /// toggling this for the change to reach the UI — `set_sub_filter`
    /// deliberately doesn't mutate the current result set so the same call
    /// site works whether the query has changed or not.
    fn set_sub_filter(&self, _idx: Option<usize>) {}

    /// Stable per-entry key for MRU/frecency tracking. `Some(_)` means this
    /// source opts in — after activation the launcher records
    /// `(source_name(ix), item_key(ix))` into [`crate::usage::UsageTracker`],
    /// and [`Source::weight`] should fold the tracker's bonus into its score.
    /// Sources that don't have a meaningful stable key (e.g. file picker,
    /// clipboard history rows) return `None` and are excluded from frecency.
    fn item_key(&self, _ix: usize) -> Option<String> {
        None
    }

    /// MRU-writing source name for entry `ix`. Defaults to [`Source::name`]
    /// so single-source implementations need no override. `UnionSource`
    /// routes this to the child owning the row so `(apps, firefox)` and
    /// `(windows, firefox)` end up in separate rows of the usage table even
    /// though both were activated via the same union's "launch" tab.
    fn source_name(&self, _ix: usize) -> &'static str {
        self.name()
    }

    /// Full-resolution image for peek mode. The launcher paints this 1:1 over
    /// the overlay surface when the user presses Space, giving a crisp
    /// native-sized preview (the regular `preview` pane is downscaled).
    /// Default `None`; override on image-bearing sources.
    fn peek_image(&self, _ix: usize) -> Option<Arc<Image>> {
        None
    }

    /// Whether this source can enter peek mode. Space is a no-op unless this
    /// returns `true`.
    fn can_peek(&self) -> bool {
        false
    }

    /// PNG-encoded image bytes for clipboard copy. Ctrl+C on the selected row
    /// writes them as `image/png`. Default `None` means "row has no image".
    fn copy_image_bytes(&self, _ix: usize) -> Option<Arc<Vec<u8>>> {
        None
    }

    /// Whether this source has copyable images. Ctrl+C is a no-op otherwise.
    fn can_copy_image(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Source impl: only the required methods. UI-facing methods
    /// (`render_item`) panic because the tests never touch them.
    struct Stub {
        items: Vec<&'static str>,
    }

    impl Source for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn icon(&self) -> &'static str {
            "?"
        }
        fn placeholder(&self) -> &'static str {
            ""
        }
        fn empty_text(&self) -> &'static str {
            ""
        }
        fn filter(&self, _query: &str) -> Vec<usize> {
            (0..self.items.len()).collect()
        }
        fn render_item(&self, _ix: usize, _selected: bool) -> AnyElement {
            unimplemented!("not exercised in unit tests")
        }
        fn activate(&self, _ix: usize) -> ActivateOutcome {
            ActivateOutcome::Quit
        }
    }

    #[test]
    fn default_weight_is_zero() {
        let s = Stub {
            items: vec!["a", "b", "c"],
        };
        assert_eq!(s.weight(0), 0);
        assert_eq!(s.weight(1), 0);
        assert_eq!(s.weight(2), 0);
    }

    #[test]
    fn default_filter_scored_forwards_filter_with_zero_scores() {
        struct PickTwo;
        impl Source for PickTwo {
            fn name(&self) -> &'static str {
                "pick2"
            }
            fn icon(&self) -> &'static str {
                "?"
            }
            fn placeholder(&self) -> &'static str {
                ""
            }
            fn empty_text(&self) -> &'static str {
                ""
            }
            fn filter(&self, _query: &str) -> Vec<usize> {
                vec![0, 2]
            }
            fn render_item(&self, _ix: usize, _selected: bool) -> AnyElement {
                unimplemented!()
            }
            fn activate(&self, _ix: usize) -> ActivateOutcome {
                ActivateOutcome::Quit
            }
        }

        let s = PickTwo;
        assert_eq!(s.filter_scored(""), vec![(0, 0), (2, 0)]);
    }

    #[test]
    fn overridden_weight_is_used() {
        struct Weighted;
        impl Source for Weighted {
            fn name(&self) -> &'static str {
                "weighted"
            }
            fn icon(&self) -> &'static str {
                "?"
            }
            fn placeholder(&self) -> &'static str {
                ""
            }
            fn empty_text(&self) -> &'static str {
                ""
            }
            fn filter(&self, _query: &str) -> Vec<usize> {
                Vec::new()
            }
            fn render_item(&self, _ix: usize, _selected: bool) -> AnyElement {
                unimplemented!()
            }
            fn activate(&self, _ix: usize) -> ActivateOutcome {
                ActivateOutcome::Quit
            }
            fn weight(&self, ix: usize) -> i32 {
                (ix as i32) * 10 - 5
            }
        }

        let s = Weighted;
        assert_eq!(s.weight(0), -5);
        assert_eq!(s.weight(1), 5);
        assert_eq!(s.weight(3), 25);
    }

    #[test]
    fn overridden_filter_scored_returns_custom_scores() {
        struct Scored;
        impl Source for Scored {
            fn name(&self) -> &'static str {
                "scored"
            }
            fn icon(&self) -> &'static str {
                "?"
            }
            fn placeholder(&self) -> &'static str {
                ""
            }
            fn empty_text(&self) -> &'static str {
                ""
            }
            fn filter(&self, _query: &str) -> Vec<usize> {
                // Intentionally different from filter_scored to prove the
                // launcher will call the scored path directly, not via filter.
                vec![0]
            }
            fn render_item(&self, _ix: usize, _selected: bool) -> AnyElement {
                unimplemented!()
            }
            fn activate(&self, _ix: usize) -> ActivateOutcome {
                ActivateOutcome::Quit
            }
            fn filter_scored(&self, _query: &str) -> Vec<(usize, i32)> {
                vec![(1, 100), (4, 50), (7, 10)]
            }
        }

        let s = Scored;
        assert_eq!(s.filter_scored(""), vec![(1, 100), (4, 50), (7, 10)]);
    }

    #[test]
    fn non_union_source_returns_empty_sub_sources() {
        // Default impl must stay empty so launcher's "does this source have
        // children?" check stays correct without modifying every source.
        let s = Stub { items: vec![] };
        assert!(s.sub_sources().is_empty());
        // set_sub_filter default is a no-op; asserting it doesn't panic is
        // enough — there's no observable state.
        s.set_sub_filter(Some(0));
        s.set_sub_filter(None);
    }
}
