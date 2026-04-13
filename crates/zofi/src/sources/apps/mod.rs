mod desktop;
mod icon;

use std::process::{Command, Stdio};

use gpui::{div, img, prelude::*, AnyElement, FontWeight, ObjectFit};

use crate::source::{ActivateOutcome, Source};
use crate::theme;

pub use desktop::DesktopEntry;

pub struct AppsSource {
    entries: Vec<DesktopEntry>,
}

impl AppsSource {
    pub fn load() -> Self {
        let entries = desktop::load_entries();
        tracing::info!("loaded {} desktop entries", entries.len());
        let entries = icon::resolve_icons(entries);
        Self { entries }
    }
}

impl Source for AppsSource {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn icon(&self) -> &'static str {
        "⊞"
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
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme::FONT_SIZE)
                    .font_weight(if selected {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if selected {
                        theme::fg_accent()
                    } else {
                        theme::fg()
                    })
                    .child(entry.name.clone()),
            )
            .into_any_element()
    }

    fn activate(&self, ix: usize) -> ActivateOutcome {
        launch(&self.entries[ix]);
        ActivateOutcome::Quit
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
