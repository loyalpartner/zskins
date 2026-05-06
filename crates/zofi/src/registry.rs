//! Runtime list of launcher sources.
//!
//! Replaces the old compile-time `SOURCES` const so that some sources
//! (notably [`crate::sources::windows::WindowsSource`], which captures a live
//! Wayland connection) can be built once at startup and then handed to the
//! launcher. Construction-by-factory was OK when every source was a
//! self-contained `load()` call, but breaks down once a source needs an
//! external resource that can only be acquired once.

use std::sync::Arc;

use crate::source::Source;
use crate::usage::UsageTracker;

/// One row in the source switcher bar plus the live `Source` it drives.
pub struct SourceEntry {
    pub source: Box<dyn Source>,
}

impl SourceEntry {
    pub fn from_source(source: Box<dyn Source>) -> Self {
        Self { source }
    }

    pub fn name(&self) -> &'static str {
        self.source.name()
    }

    pub fn icon(&self) -> &'static str {
        self.source.icon()
    }
}

/// Owns every registered source. The launcher borrows entries by index; the
/// order here is exactly the order shown in the switcher bar.
pub struct SourceRegistry {
    entries: Vec<SourceEntry>,
    /// Shared MRU/frecency store. The launcher writes to this on every
    /// successful activation; sources read from it via `Source::weight`.
    /// Optional so unit tests can construct a registry without wiring the DB.
    tracker: Option<Arc<UsageTracker>>,
}

impl SourceRegistry {
    pub fn new(entries: Vec<SourceEntry>) -> Self {
        Self {
            entries,
            tracker: None,
        }
    }

    pub fn with_tracker(mut self, tracker: Arc<UsageTracker>) -> Self {
        self.tracker = Some(tracker);
        self
    }

    pub fn tracker(&self) -> Option<&Arc<UsageTracker>> {
        self.tracker.as_ref()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Idiomatic companion to `len`; clippy flags `len`-without-`is_empty`.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[SourceEntry] {
        &self.entries
    }

    pub fn get(&self, ix: usize) -> &SourceEntry {
        &self.entries[ix]
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, SourceEntry> {
        self.entries.iter_mut()
    }

    /// Find the registry index for a CLI-given source name.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.name() == name)
    }

    /// Comma-joined list of names — used when reporting an unknown source.
    pub fn names_joined(&self) -> String {
        self.entries
            .iter()
            .map(|e| e.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::ActivateOutcome;
    use gpui::AnyElement;

    struct StubSource(&'static str);
    impl Source for StubSource {
        fn name(&self) -> &'static str {
            self.0
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
        fn render_item(&self, _: usize, _: bool, _: &ztheme::Theme) -> AnyElement {
            unimplemented!()
        }
        fn activate(&self, _: usize) -> ActivateOutcome {
            ActivateOutcome::Quit
        }
    }

    fn entry(name: &'static str) -> SourceEntry {
        SourceEntry::from_source(Box::new(StubSource(name)))
    }

    #[test]
    fn position_finds_registered_name() {
        let r = SourceRegistry::new(vec![entry("apps"), entry("files")]);
        assert_eq!(r.position("files"), Some(1));
        assert_eq!(r.position("apps"), Some(0));
        assert_eq!(r.position("missing"), None);
    }

    #[test]
    fn names_joined_lists_in_order() {
        let r = SourceRegistry::new(vec![entry("apps"), entry("clipboard"), entry("files")]);
        assert_eq!(r.names_joined(), "apps, clipboard, files");
    }

    #[test]
    fn len_and_is_empty_match_entries() {
        let empty = SourceRegistry::new(vec![]);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());

        let r = SourceRegistry::new(vec![entry("a")]);
        assert_eq!(r.len(), 1);
        assert!(!r.is_empty());
    }

    #[test]
    fn registry_without_tracker_returns_none() {
        let r = SourceRegistry::new(vec![entry("a")]);
        assert!(r.tracker().is_none());
    }

    #[test]
    fn registry_with_tracker_exposes_it() {
        // The launcher's activate path calls `registry.tracker().record(...)` —
        // this exercises the getter so that wiring doesn't silently break.
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        let r = SourceRegistry::new(vec![entry("a")]).with_tracker(tracker.clone());
        let got = r.tracker().expect("tracker should round-trip");
        assert!(Arc::ptr_eq(got, &tracker));

        // And it's a live tracker: record → frecency_bonus reflects the write.
        got.record("apps", "x");
        assert!(got.frecency_bonus("apps", "x") > 0);
    }
}
