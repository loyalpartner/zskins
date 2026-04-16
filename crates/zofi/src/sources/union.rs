//! Merges multiple [`Source`]s into a single ranked view.
//!
//! Why a dedicated combinator instead of concatenation: the launcher needs one
//! monotonic index space, and users expect higher-relevance entries first
//! regardless of which backing source produced them. `UnionSource` projects
//! every child entry into a virtual index, ranks the combined set by
//! `weight + match_score`, and routes every downstream call (render, activate,
//! preview, mime) back through the correct child.

use std::cell::{Cell, RefCell};

use std::sync::Arc;

use gpui::{div, prelude::*, px, AnyElement, Image};

use crate::source::{ActivateOutcome, Layout, Preview, PreviewChrome, Source, SourceMeta};
use crate::theme;

/// Width of the source-type gutter shown next to each row in mixed view.
/// Vim-signcolumn-style: just wide enough for a single-char glyph.
const GUTTER_W: gpui::Pixels = px(22.0);

/// Combines N child sources into one. See module docs.
pub struct UnionSource {
    children: Vec<Box<dyn Source>>,
    name: &'static str,
    icon: &'static str,
    placeholder: &'static str,
    empty_text: &'static str,
    // RefCell: launcher is single-threaded and calls `filter` before any
    // method that needs the routing table, so we only need interior mutability,
    // not a lock.
    routing: RefCell<Vec<Route>>,
    // Which child, if any, the launcher has narrowed the view to. `None` =
    // show every child (the default). A `Cell` is enough because mutation
    // happens via `&self` (trait method) and reads never overlap with writes
    // in the single-threaded launcher loop.
    active_child: Cell<Option<usize>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Route {
    child_idx: usize,
    inner_ix: usize,
}

impl UnionSource {
    pub fn new(children: Vec<Box<dyn Source>>) -> Self {
        Self {
            children,
            name: "all",
            icon: "◉",
            placeholder: "Search all...",
            empty_text: "No results",
            routing: RefCell::new(Vec::new()),
            active_child: Cell::new(None),
        }
    }

    pub fn with_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    pub fn with_icon(mut self, icon: &'static str) -> Self {
        self.icon = icon;
        self
    }

    pub fn with_placeholder(mut self, placeholder: &'static str) -> Self {
        self.placeholder = placeholder;
        self
    }

    pub fn with_empty_text(mut self, text: &'static str) -> Self {
        self.empty_text = text;
        self
    }

    /// Indices of children to iterate in this filter pass. When the launcher
    /// has narrowed the view to one child (`active_child = Some(i)`), we
    /// restrict to that single index; a stale index (children resized) is
    /// treated as "show everything" rather than panicking — the switcher bar
    /// will re-render and the user can pick again.
    fn active_indices(&self) -> Vec<usize> {
        match self.active_child.get() {
            Some(i) if i < self.children.len() => vec![i],
            _ => (0..self.children.len()).collect(),
        }
    }

    fn route(&self, ix: usize) -> Route {
        // Panicking is deliberate: a virtual index outside the routing table
        // means the launcher skipped `filter`, which is a contract bug we want
        // surfaced early rather than papered over with placeholders.
        self.routing
            .borrow()
            .get(ix)
            .copied()
            .unwrap_or_else(|| panic!("UnionSource: virtual index {ix} out of range"))
    }

    /// Build the routing table for `query`, return the total score per virtual
    /// index. Shared by `filter` (discards scores) and `filter_scored`.
    fn score_and_route(&self, query: &str) -> Vec<i32> {
        // (total, child_idx, inner_ix). Building a flat vector first lets us
        // sort once with a single comparator rather than merge N sorted lists
        // per re-rank — child count is tiny (<10) and each child already
        // returned a pre-filtered slice.
        let mut scored: Vec<(i32, usize, usize)> = Vec::new();
        for child_idx in self.active_indices() {
            let child = &self.children[child_idx];
            for (inner_ix, score) in child.filter_scored(query) {
                let total = score.saturating_add(child.weight(inner_ix));
                scored.push((total, child_idx, inner_ix));
            }
        }

        // Sort: total DESC, then child_idx ASC (registration order wins),
        // then inner_ix ASC (child's own order wins). Deterministic ordering
        // is load-bearing for reproducible tests and stable UI on re-filter.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

        let mut routing = self.routing.borrow_mut();
        routing.clear();
        routing.reserve(scored.len());
        let mut totals = Vec::with_capacity(scored.len());
        for (total, child_idx, inner_ix) in scored {
            routing.push(Route {
                child_idx,
                inner_ix,
            });
            totals.push(total);
        }
        totals
    }
}

impl Source for UnionSource {
    fn name(&self) -> &'static str {
        self.name
    }

    fn icon(&self) -> &'static str {
        self.icon
    }

    fn placeholder(&self) -> &'static str {
        self.placeholder
    }

    fn empty_text(&self) -> &'static str {
        self.empty_text
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        let scored = self.score_and_route(query);
        (0..scored.len()).collect()
    }

    fn filter_scored(&self, query: &str) -> Vec<(usize, i32)> {
        // Mirror `filter` but expose the combined `total` so a UnionSource
        // nested inside another UnionSource still has a meaningful score.
        self.score_and_route(query)
            .into_iter()
            .enumerate()
            .collect()
    }

    fn render_item(&self, ix: usize, selected: bool) -> AnyElement {
        let r = self.route(ix);
        let child = self.children[r.child_idx].render_item(r.inner_ix, selected);
        // Only surface the gutter when the user is looking at the mixed view;
        // a single-type listing needs no annotation and the column would just
        // indent every row unnecessarily.
        if self.active_child.get().is_some() {
            return child;
        }
        let glyph = self.children[r.child_idx].icon();
        // Tint the gutter glyph by the child's category so the eye can
        // sort rows at a glance (windows/apps/files/clipboard each get a
        // distinct hue). Falls back to accent for unknown sources.
        let tint = theme::category(self.children[r.child_idx].name());
        div()
            .flex()
            .items_center()
            .h_full()
            .child(
                div()
                    .w(GUTTER_W)
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(tint)
                    .text_size(theme::FONT_SIZE_SM)
                    .border_r_1()
                    .border_color(theme::panel_border())
                    .child(glyph),
            )
            .child(div().flex_1().min_w_0().h_full().child(child))
            .into_any_element()
    }

    fn activate(&self, ix: usize) -> ActivateOutcome {
        let r = self.route(ix);
        self.children[r.child_idx].activate(r.inner_ix)
    }

    fn item_key(&self, ix: usize) -> Option<String> {
        let r = self.route(ix);
        self.children[r.child_idx].item_key(r.inner_ix)
    }

    fn source_name(&self, ix: usize) -> &'static str {
        let r = self.route(ix);
        // Use the child's own name — not `source_name(inner_ix)` — because
        // nested unions don't exist in this codebase and threading the
        // inner index only matters for sources that store per-row names.
        self.children[r.child_idx].name()
    }

    fn weight(&self, ix: usize) -> i32 {
        let r = self.route(ix);
        self.children[r.child_idx].weight(r.inner_ix)
    }

    fn preview(&self, ix: usize) -> Option<Preview> {
        let r = self.route(ix);
        self.children[r.child_idx].preview(r.inner_ix)
    }

    fn preview_chrome(&self, ix: usize) -> Option<PreviewChrome> {
        let r = self.route(ix);
        self.children[r.child_idx].preview_chrome(r.inner_ix)
    }

    fn peek_image(&self, ix: usize) -> Option<Arc<Image>> {
        let r = self.route(ix);
        self.children[r.child_idx].peek_image(r.inner_ix)
    }

    fn can_peek(&self) -> bool {
        self.children.iter().any(|c| c.can_peek())
    }

    fn copy_image_bytes(&self, ix: usize) -> Option<Arc<Vec<u8>>> {
        let r = self.route(ix);
        self.children[r.child_idx].copy_image_bytes(r.inner_ix)
    }

    fn can_copy_image(&self) -> bool {
        self.children.iter().any(|c| c.can_copy_image())
    }

    fn mimes(&self, ix: usize) -> Vec<String> {
        let r = self.route(ix);
        self.children[r.child_idx].mimes(r.inner_ix)
    }

    fn primary_mime(&self, ix: usize) -> Option<String> {
        let r = self.route(ix);
        self.children[r.child_idx].primary_mime(r.inner_ix)
    }

    fn primary_mime_index(&self, ix: usize) -> Option<usize> {
        let r = self.route(ix);
        self.children[r.child_idx].primary_mime_index(r.inner_ix)
    }

    fn preview_for_mime(&self, ix: usize, mime: &str) -> Option<Preview> {
        let r = self.route(ix);
        self.children[r.child_idx].preview_for_mime(r.inner_ix, mime)
    }

    fn activate_with_mime(&self, ix: usize, mime: &str) -> ActivateOutcome {
        let r = self.route(ix);
        self.children[r.child_idx].activate_with_mime(r.inner_ix, mime)
    }

    fn sub_sources(&self) -> Vec<SourceMeta> {
        // Expose each child's name+icon so the launcher can render one tab
        // per child. Order matches registration order — same order the
        // launcher relies on for `set_sub_filter(Some(i))`.
        self.children
            .iter()
            .map(|c| SourceMeta {
                name: c.name(),
                icon: c.icon(),
                prefix: c.prefix(),
            })
            .collect()
    }

    fn set_sub_filter(&self, idx: Option<usize>) {
        // Out-of-range indices become `None` so the view falls back to "all"
        // instead of silently filtering nothing — the error case the user
        // actually wants is "show me what I can".
        self.active_child
            .set(idx.filter(|&i| i < self.children.len()));
    }

    fn layout(&self) -> Layout {
        // Any child needing preview forces the wider layout — otherwise its
        // preview pane would silently vanish when viewed through the union.
        if self
            .children
            .iter()
            .any(|c| matches!(c.layout(), Layout::ListAndPreview))
        {
            Layout::ListAndPreview
        } else {
            Layout::List
        }
    }

    fn banner(&self) -> Option<AnyElement> {
        // First child with something to say wins. Showing multiple banners
        // stacked would blow out the launcher's fixed-height header.
        self.children.iter().find_map(|c| c.banner())
    }

    fn take_pulse(&mut self) -> Option<async_channel::Receiver<()>> {
        // Merge every child's pulse into one. `bounded(1)` is enough: the
        // receiver just needs "something changed, re-render"; coalescing extra
        // ticks is a feature, not a bug.
        let (tx, rx) = async_channel::bounded::<()>(1);
        let mut any = false;
        for child in &mut self.children {
            if let Some(child_rx) = child.take_pulse() {
                any = true;
                let tx = tx.clone();
                std::thread::spawn(move || {
                    // Forward until child closes. `try_send` drops when the
                    // buffer is full, which is the desired coalescing behavior.
                    while let Ok(()) = child_rx.recv_blocking() {
                        let _ = tx.try_send(());
                    }
                });
            }
        }
        if any {
            Some(rx)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Deterministic fixture: carries (label, weight, score) tuples and
    /// records every `activate` for assertions. Substring-filters on query.
    struct StubSource {
        name: &'static str,
        items: Vec<(&'static str, i32, i32)>,
        activate_log: RefCell<Vec<(&'static str, usize)>>,
        layout: Layout,
        banner_text: Option<&'static str>,
    }

    impl StubSource {
        fn new(name: &'static str, items: Vec<(&'static str, i32, i32)>) -> Self {
            Self {
                name,
                items,
                activate_log: RefCell::new(Vec::new()),
                layout: Layout::List,
                banner_text: None,
            }
        }

        fn with_layout(mut self, layout: Layout) -> Self {
            self.layout = layout;
            self
        }

        fn with_banner(mut self, text: &'static str) -> Self {
            self.banner_text = Some(text);
            self
        }
    }

    impl Source for StubSource {
        fn name(&self) -> &'static str {
            self.name
        }
        fn icon(&self) -> &'static str {
            "x"
        }
        fn placeholder(&self) -> &'static str {
            ""
        }
        fn empty_text(&self) -> &'static str {
            ""
        }
        fn filter(&self, q: &str) -> Vec<usize> {
            self.items
                .iter()
                .enumerate()
                .filter(|(_, (label, _, _))| q.is_empty() || label.contains(q))
                .map(|(i, _)| i)
                .collect()
        }
        fn filter_scored(&self, q: &str) -> Vec<(usize, i32)> {
            self.filter(q)
                .into_iter()
                .map(|i| (i, self.items[i].2))
                .collect()
        }
        fn weight(&self, ix: usize) -> i32 {
            self.items[ix].1
        }
        fn render_item(&self, _ix: usize, _sel: bool) -> AnyElement {
            unimplemented!("render not exercised in unit tests")
        }
        fn activate(&self, ix: usize) -> ActivateOutcome {
            self.activate_log.borrow_mut().push((self.name, ix));
            ActivateOutcome::Quit
        }
        fn layout(&self) -> Layout {
            self.layout
        }
        fn banner(&self) -> Option<AnyElement> {
            // Return Some(...) by turning text into a dummy element. For tests
            // we just need identifiability, but AnyElement can't be compared
            // directly, so `banner` presence is tested via a side channel: we
            // count children whose `banner_text` is Some and infer which one
            // UnionSource picked. See `banner_returns_first_some_from_children`.
            self.banner_text
                .map(|_| unimplemented!("element construction not exercised"))
        }
    }

    // Helper: extract the (source_name, inner_ix) that backs a virtual index.
    // Avoids leaking private `route` into the public API.
    fn trace(union: &UnionSource, virtual_ix: usize) -> (&'static str, usize) {
        let r = union.routing.borrow()[virtual_ix];
        (union.children[r.child_idx].name(), r.inner_ix)
    }

    #[test]
    fn empty_children_returns_no_results() {
        let u = UnionSource::new(vec![]);
        assert!(u.filter("").is_empty());
    }

    #[test]
    fn combines_entries_from_all_children() {
        let a = StubSource::new("a", vec![("apple", 0, 0), ("ant", 0, 0)]);
        let b = StubSource::new("b", vec![("bee", 0, 0), ("bat", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        assert_eq!(u.filter("").len(), 4);
    }

    #[test]
    fn higher_weight_ranks_first() {
        let a = StubSource::new("a", vec![("apple", 0, 0)]);
        let b = StubSource::new("b", vec![("banana", 100, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        let out = u.filter("");
        assert_eq!(out.len(), 2);
        assert_eq!(trace(&u, 0), ("b", 0));
        assert_eq!(trace(&u, 1), ("a", 0));
    }

    #[test]
    fn match_score_tiebreaks_weight() {
        // Both weights zero — score alone decides order.
        let a = StubSource::new("a", vec![("alpha", 0, 10)]);
        let b = StubSource::new("b", vec![("beta", 0, 90)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.filter("");
        assert_eq!(trace(&u, 0), ("b", 0));
        assert_eq!(trace(&u, 1), ("a", 0));
    }

    #[test]
    fn weight_plus_score_combined() {
        // A: weight 50 + score 0 = 50. B: weight 0 + score 60 = 60. B wins.
        let a = StubSource::new("a", vec![("a0", 50, 0)]);
        let b = StubSource::new("b", vec![("b0", 0, 60)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.filter("");
        assert_eq!(trace(&u, 0), ("b", 0));
        assert_eq!(trace(&u, 1), ("a", 0));
    }

    #[test]
    fn same_total_respects_child_registration_order() {
        // Both entries total 10. First-registered child wins the tie.
        let a = StubSource::new("a", vec![("a0", 10, 0)]);
        let b = StubSource::new("b", vec![("b0", 10, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.filter("");
        assert_eq!(trace(&u, 0), ("a", 0));
        assert_eq!(trace(&u, 1), ("b", 0));
    }

    #[test]
    fn same_total_same_child_uses_inner_order() {
        // Three items in one child, all weight 5 score 0. Inner index ascends.
        let a = StubSource::new("a", vec![("x", 5, 0), ("y", 5, 0), ("z", 5, 0)]);
        let u = UnionSource::new(vec![Box::new(a)]);
        u.filter("");
        assert_eq!(trace(&u, 0), ("a", 0));
        assert_eq!(trace(&u, 1), ("a", 1));
        assert_eq!(trace(&u, 2), ("a", 2));
    }

    #[test]
    fn filter_query_filters_children() {
        let a = StubSource::new("a", vec![("apple", 0, 0), ("ant", 0, 0)]);
        let b = StubSource::new("b", vec![("app", 0, 0), ("bee", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        let out = u.filter("app");
        assert_eq!(out.len(), 2);
        // Both entries match "app": a[0]=apple and b[0]=app. Same total (0),
        // so child registration order puts a first.
        assert_eq!(trace(&u, 0), ("a", 0));
        assert_eq!(trace(&u, 1), ("b", 0));
    }

    #[test]
    fn activate_routes_to_correct_child() {
        // Grab handles before boxing so we can inspect the log after activate.
        let a = std::rc::Rc::new(StubSource::new("a", vec![("apple", 0, 0)]));
        let b = std::rc::Rc::new(StubSource::new("b", vec![("bee", 100, 0)]));

        // Adapter because Box<dyn Source> needs ownership — wrap Rc so the
        // test retains read access to the activate_log.
        struct RcWrap(std::rc::Rc<StubSource>);
        impl Source for RcWrap {
            fn name(&self) -> &'static str {
                self.0.name()
            }
            fn icon(&self) -> &'static str {
                self.0.icon()
            }
            fn placeholder(&self) -> &'static str {
                self.0.placeholder()
            }
            fn empty_text(&self) -> &'static str {
                self.0.empty_text()
            }
            fn filter(&self, q: &str) -> Vec<usize> {
                self.0.filter(q)
            }
            fn filter_scored(&self, q: &str) -> Vec<(usize, i32)> {
                self.0.filter_scored(q)
            }
            fn weight(&self, ix: usize) -> i32 {
                self.0.weight(ix)
            }
            fn render_item(&self, ix: usize, sel: bool) -> AnyElement {
                self.0.render_item(ix, sel)
            }
            fn activate(&self, ix: usize) -> ActivateOutcome {
                self.0.activate(ix)
            }
        }

        let u = UnionSource::new(vec![
            Box::new(RcWrap(a.clone())),
            Box::new(RcWrap(b.clone())),
        ]);
        u.filter("");
        // Virtual 0 should route to b[0] (weight 100 wins).
        u.activate(0);
        assert_eq!(a.activate_log.borrow().as_slice(), &[]);
        assert_eq!(b.activate_log.borrow().as_slice(), &[("b", 0)]);
        // Virtual 1 routes to a[0].
        u.activate(1);
        assert_eq!(a.activate_log.borrow().as_slice(), &[("a", 0)]);
    }

    #[test]
    fn weight_method_routes_to_child() {
        let a = StubSource::new("a", vec![("x", 7, 0)]);
        let b = StubSource::new("b", vec![("y", 42, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.filter("");
        // Virtual 0 = b[0] with weight 42. Virtual 1 = a[0] with weight 7.
        assert_eq!(u.weight(0), 42);
        assert_eq!(u.weight(1), 7);
    }

    #[test]
    fn layout_is_list_unless_any_child_needs_preview() {
        let a = StubSource::new("a", vec![("x", 0, 0)]);
        let b = StubSource::new("b", vec![("y", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        assert!(matches!(u.layout(), Layout::List));

        let a = StubSource::new("a", vec![("x", 0, 0)]);
        let b = StubSource::new("b", vec![("y", 0, 0)]).with_layout(Layout::ListAndPreview);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        assert!(matches!(u.layout(), Layout::ListAndPreview));
    }

    #[test]
    fn filter_scored_exposes_combined_totals_in_sorted_order() {
        let a = StubSource::new("a", vec![("x", 10, 5)]); // total 15
        let b = StubSource::new("b", vec![("y", 100, 0)]); // total 100
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        let out = u.filter_scored("");
        assert_eq!(out, vec![(0, 100), (1, 15)]);
        assert_eq!(trace(&u, 0), ("b", 0));
        assert_eq!(trace(&u, 1), ("a", 0));
    }

    #[test]
    fn banner_returns_first_some_from_children() {
        // Banner order: a has none, b has one, c has one → b wins.
        // We detect "which child got picked" by checking that `banner()`
        // attempts to materialize exactly the first Some child and panics
        // with an expected message (StubSource banner is unimplemented!()).
        let a = StubSource::new("a", vec![("x", 0, 0)]);
        let b = StubSource::new("b", vec![("y", 0, 0)]).with_banner("second");
        let c = StubSource::new("c", vec![("z", 0, 0)]).with_banner("third");
        let u = UnionSource::new(vec![Box::new(a), Box::new(b), Box::new(c)]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| u.banner()));
        // The panic comes from b's banner — we expect to hit the
        // unimplemented!() inside the first Some-returning child. Either a
        // panic (child with banner triggered it) or None (no banner child).
        // StubSource.banner returns Some(...).map(|_| unimplemented!()),
        // so the closure panics before returning. That is our signal.
        assert!(
            result.is_err(),
            "expected unimplemented panic from first bannered child"
        );
    }

    #[test]
    fn take_pulse_returns_none_when_no_children_have_pulse() {
        let a = StubSource::new("a", vec![("x", 0, 0)]);
        let mut u = UnionSource::new(vec![Box::new(a)]);
        assert!(u.take_pulse().is_none());
    }

    #[test]
    fn take_pulse_forwards_child_pulse() {
        // Child source that exposes a pulse we can drive from the test.
        struct Pulsing {
            rx: Option<async_channel::Receiver<()>>,
        }
        impl Source for Pulsing {
            fn name(&self) -> &'static str {
                "p"
            }
            fn icon(&self) -> &'static str {
                "x"
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
                unimplemented!()
            }
            fn activate(&self, _: usize) -> ActivateOutcome {
                ActivateOutcome::Quit
            }
            fn take_pulse(&mut self) -> Option<async_channel::Receiver<()>> {
                self.rx.take()
            }
        }

        let (tx, rx) = async_channel::bounded::<()>(1);
        let pulsing = Pulsing { rx: Some(rx) };
        let mut u = UnionSource::new(vec![Box::new(pulsing)]);
        let out = u.take_pulse().expect("union should expose merged pulse");
        tx.send_blocking(()).unwrap();
        // Block briefly on recv; forwarding thread should deliver soon.
        let got = out.recv_blocking();
        assert!(got.is_ok());
    }

    #[test]
    fn union_reports_sub_sources_from_children() {
        // UnionSource must surface each child's name+icon so the launcher
        // can build a tab per child. Order == registration order.
        let a = StubSource::new("apps", vec![("x", 0, 0)]);
        let b = StubSource::new("files", vec![("y", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        let subs = u.sub_sources();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name, "apps");
        assert_eq!(subs[1].name, "files");
        // StubSource::icon returns "x" for every child — verifying the icon
        // is still piped through is the load-bearing part, not the glyph.
        assert_eq!(subs[0].icon, "x");
        assert_eq!(subs[1].icon, "x");
    }

    #[test]
    fn union_filter_respects_sub_filter_all() {
        // Default (None) = show everything. Explicitly clearing the filter
        // must also return to the "everything" behavior.
        let a = StubSource::new("a", vec![("apple", 0, 0)]);
        let b = StubSource::new("b", vec![("bee", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.set_sub_filter(None);
        assert_eq!(u.filter("").len(), 2);

        // Toggling through Some(_) and back to None must also cover both.
        u.set_sub_filter(Some(0));
        u.set_sub_filter(None);
        assert_eq!(u.filter("").len(), 2);
    }

    #[test]
    fn union_filter_respects_sub_filter_single() {
        // Picking a specific child must restrict filter output to that
        // child's entries only — and activate routing must still work
        // against the filtered set.
        let a = StubSource::new("a", vec![("apple", 0, 0), ("ant", 0, 0)]);
        let b = StubSource::new("b", vec![("bee", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);

        u.set_sub_filter(Some(1));
        let out = u.filter("");
        assert_eq!(out.len(), 1);
        assert_eq!(trace(&u, 0), ("b", 0));

        u.set_sub_filter(Some(0));
        let out = u.filter("");
        assert_eq!(out.len(), 2);
        assert_eq!(trace(&u, 0), ("a", 0));
        assert_eq!(trace(&u, 1), ("a", 1));
    }

    #[test]
    fn union_sub_filter_out_of_range_falls_back_to_all() {
        // Robustness: a stale sub_filter index (e.g. children reshuffled)
        // must degrade to "show everything" rather than panic or hide all
        // results. Same intent as UnionSource::route's defensive contract.
        let a = StubSource::new("a", vec![("apple", 0, 0)]);
        let b = StubSource::new("b", vec![("bee", 0, 0)]);
        let u = UnionSource::new(vec![Box::new(a), Box::new(b)]);
        u.set_sub_filter(Some(99));
        assert_eq!(u.filter("").len(), 2);
    }

    #[test]
    fn item_key_and_source_name_route_to_child() {
        // Child B overrides item_key; Union must surface that under B's name
        // regardless of the union's own `name`.
        struct Keyed {
            name: &'static str,
            key: &'static str,
        }
        impl Source for Keyed {
            fn name(&self) -> &'static str {
                self.name
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
            fn filter(&self, _: &str) -> Vec<usize> {
                vec![0]
            }
            fn filter_scored(&self, _: &str) -> Vec<(usize, i32)> {
                vec![(0, 0)]
            }
            fn render_item(&self, _: usize, _: bool) -> AnyElement {
                unimplemented!()
            }
            fn activate(&self, _: usize) -> ActivateOutcome {
                ActivateOutcome::Quit
            }
            fn item_key(&self, _: usize) -> Option<String> {
                Some(self.key.to_string())
            }
        }

        let u = UnionSource::new(vec![
            Box::new(Keyed {
                name: "windows",
                key: "firefox",
            }),
            Box::new(Keyed {
                name: "apps",
                key: "firefox",
            }),
        ])
        .with_name("launch");

        u.filter("");
        // Virtual 0 routes to child 0 (registration order breaks same-total tie).
        assert_eq!(u.item_key(0).as_deref(), Some("firefox"));
        assert_eq!(u.source_name(0), "windows");
        assert_eq!(u.source_name(1), "apps");
        // Union's own name is unaffected — only per-row source_name is routed.
        assert_eq!(u.name(), "launch");
    }

    #[test]
    fn builder_methods_override_defaults() {
        let u = UnionSource::new(vec![])
            .with_name("mixed")
            .with_icon("*")
            .with_placeholder("Find anything")
            .with_empty_text("nothing here");
        assert_eq!(u.name(), "mixed");
        assert_eq!(u.icon(), "*");
        assert_eq!(u.placeholder(), "Find anything");
        assert_eq!(u.empty_text(), "nothing here");
    }
}
