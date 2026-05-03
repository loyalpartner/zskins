mod desktop;
mod icon;

use std::process::{Command, Stdio};
use std::sync::Arc;

use gpui::{div, img, prelude::*, AnyElement, FontWeight, ObjectFit};

use crate::source::{ActivateOutcome, InspectorCard, InspectorRow, Layout, Preview, Source};
use crate::theme;
use crate::usage::UsageTracker;

pub use desktop::DesktopEntry;

/// Matches `UsageTracker` rows — shared with `WindowsSource` via the tracker's
/// `source` column. Keeping it as a const avoids typo divergence between the
/// two write paths.
pub const SOURCE_NAME: &str = "apps";

pub struct AppsSource {
    entries: Vec<DesktopEntry>,
    tracker: Arc<UsageTracker>,
}

impl AppsSource {
    pub fn load(tracker: Arc<UsageTracker>) -> Self {
        let entries = desktop::load_entries();
        tracing::info!("loaded {} desktop entries", entries.len());
        let entries = icon::resolve_icons(entries);
        Self { entries, tracker }
    }

    /// Expose the preloaded `.desktop` entries so other sources (notably
    /// `WindowsSource`) can reuse their already-decoded `icon_data` instead of
    /// re-resolving through `icon-theme`.
    pub fn entries(&self) -> &[DesktopEntry] {
        &self.entries
    }
}

impl Source for AppsSource {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    fn icon(&self) -> &'static str {
        "▣"
    }

    fn prefix(&self) -> Option<char> {
        Some('>')
    }

    fn placeholder(&self) -> &'static str {
        "Search applications..."
    }

    fn empty_text(&self) -> &'static str {
        "No matching applications"
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        if q.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.search_key.contains(&q))
                .map(|(i, _)| i)
                .collect()
        }
    }

    fn render_item(&self, ix: usize, selected: bool) -> AnyElement {
        let entry = &self.entries[ix];
        // file_stem matches the WM class window managers report, so it makes
        // a more useful secondary line than the GTK icon name.
        let subtitle = entry.file_stem.clone();
        div()
            .h_full()
            .px(theme::PAD_X)
            .flex()
            .items_center()
            .gap(theme::GAP)
            .child(render_icon(entry))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(gpui::px(1.0))
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(theme::FONT_SIZE)
                            .font_weight(if selected {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if selected { gpui::white() } else { theme::fg() })
                            .child(entry.name.clone()),
                    )
                    .when(!subtitle.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(theme::FONT_SIZE_SM)
                                .text_color(theme::fg_dim())
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(subtitle),
                        )
                    }),
            )
            .into_any_element()
    }

    fn activate(&self, ix: usize) -> ActivateOutcome {
        launch(&self.entries[ix]);
        ActivateOutcome::Quit
    }

    fn weight(&self, ix: usize) -> i32 {
        let Some(entry) = self.entries.get(ix) else {
            return 0;
        };
        if entry.file_stem.is_empty() {
            return 0;
        }
        self.tracker.frecency_bonus(SOURCE_NAME, &entry.file_stem)
    }

    fn item_key(&self, ix: usize) -> Option<String> {
        let entry = self.entries.get(ix)?;
        if entry.file_stem.is_empty() {
            return None;
        }
        Some(entry.file_stem.clone())
    }

    fn layout(&self) -> Layout {
        Layout::ListAndPreview
    }

    // No `preview_chrome`: `Preview::Inspector` carries its own header,
    // and the launcher suppresses the chrome strip for that variant.

    fn preview(&self, ix: usize) -> Option<Preview> {
        let entry = self.entries.get(ix)?;
        let mut rows = Vec::new();
        if let Some(ref name) = entry.icon_name {
            rows.push(InspectorRow {
                label: "Icon name".into(),
                value: name.clone(),
                mono: true,
            });
        }
        if let Some(ref path) = entry.icon_path {
            rows.push(InspectorRow {
                label: "Icon path".into(),
                value: path.to_string_lossy().into_owned(),
                mono: true,
            });
        }
        if !entry.desktop_path.as_os_str().is_empty() {
            rows.push(InspectorRow {
                label: "Desktop file".into(),
                value: entry.desktop_path.to_string_lossy().into_owned(),
                mono: true,
            });
        }
        rows.push(InspectorRow {
            label: "Exec".into(),
            value: entry.exec.clone(),
            mono: true,
        });
        if let Some(ref wm) = entry.startup_wm_class {
            rows.push(InspectorRow {
                label: "WM Class".into(),
                value: wm.clone(),
                mono: true,
            });
        }
        Some(Preview::Inspector(InspectorCard {
            icon: entry.icon_data.clone(),
            title: entry.name.to_string(),
            subtitle: Some("Application".into()),
            rows,
        }))
    }
}

fn render_icon(entry: &DesktopEntry) -> gpui::Div {
    if let Some(ref data) = entry.icon_data {
        div().size(theme::ICON_SIZE).flex_shrink_0().child(
            img(data.clone())
                .size(theme::ICON_SIZE)
                .object_fit(ObjectFit::Contain),
        )
    } else if let Some(ref path) = entry.icon_path {
        div().size(theme::ICON_SIZE).flex_shrink_0().child(
            img(path.clone())
                .size(theme::ICON_SIZE)
                .object_fit(ObjectFit::Contain),
        )
    } else {
        div()
            .size(theme::ICON_SIZE)
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme::fg_dim())
            .text_size(theme::FONT_SIZE_SM)
            .child("\u{25cb}")
    }
}

fn launch(entry: &DesktopEntry) {
    let exec = desktop::strip_field_codes(&entry.exec);
    let parts: Vec<&str> = exec.split_whitespace().collect();
    if let Some((cmd, args)) = parts.split_first() {
        match Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => tracing::info!("launched: {}", entry.name),
            Err(e) => tracing::error!("failed to launch {}: {e}", entry.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageTracker;

    fn entry(name: &str, stem: &str) -> DesktopEntry {
        DesktopEntry {
            name: name.to_string().into(),
            exec: String::new(),
            icon_name: None,
            icon_path: None,
            icon_data: None,
            search_key: name.to_lowercase(),
            file_stem: stem.to_string(),
            desktop_path: std::path::PathBuf::new(),
            startup_wm_class: None,
        }
    }

    fn source_with(entries: Vec<DesktopEntry>, tracker: Arc<UsageTracker>) -> AppsSource {
        AppsSource { entries, tracker }
    }

    #[test]
    fn item_key_returns_file_stem() {
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        let s = source_with(vec![entry("Firefox", "firefox")], tracker);
        assert_eq!(s.item_key(0).as_deref(), Some("firefox"));
    }

    #[test]
    fn item_key_is_none_when_file_stem_is_empty() {
        // Some .desktop entries synthesised at runtime (e.g. bulk import paths)
        // may not have a file stem — such rows opt out of MRU cleanly rather
        // than corrupting the DB with an empty key.
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        let s = source_with(vec![entry("NoStem", "")], tracker);
        assert_eq!(s.item_key(0), None);
    }

    #[test]
    fn weight_is_zero_by_default() {
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        let s = source_with(vec![entry("Firefox", "firefox")], tracker);
        assert_eq!(s.weight(0), 0);
    }

    #[test]
    fn weight_reflects_tracker_bonus_after_record() {
        // Record activations through the shared tracker; weight() must pick
        // up the resulting frecency bonus immediately on the next read.
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        for _ in 0..5 {
            tracker.record(SOURCE_NAME, "firefox");
        }
        let s = source_with(vec![entry("Firefox", "firefox")], tracker);
        assert!(s.weight(0) > 0);
    }
}
