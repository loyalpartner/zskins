use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use super::desktop::DesktopEntry;
use crate::sources::icon as shared_icon;

/// Resolve icon paths and preload all icon bytes into memory.
pub fn resolve_icons(mut entries: Vec<DesktopEntry>) -> Vec<DesktopEntry> {
    let cache = icon_theme::IconCache::new(&["apps"]);

    // Step 1: assign paths.
    for entry in &mut entries {
        if let Some(ref name) = entry.icon_name {
            entry.icon_path = if name.starts_with('/') {
                Some(PathBuf::from(name))
            } else {
                cache.lookup(name).map(Path::to_path_buf)
            };
        }
    }

    // Step 2: parallel-read all icon files into memory.
    let loaded: Vec<Option<Arc<gpui::Image>>> = entries
        .par_iter()
        .map(|entry| {
            let path = entry.icon_path.as_ref()?;
            shared_icon::load_image_from_path(path)
        })
        .collect();

    for (entry, data) in entries.iter_mut().zip(loaded) {
        if data.is_some() {
            entry.icon_path = None;
        }
        entry.icon_data = data;
    }

    let preloaded = entries.iter().filter(|e| e.icon_data.is_some()).count();
    tracing::info!("{preloaded}/{} icons preloaded into memory", entries.len());

    entries
}
