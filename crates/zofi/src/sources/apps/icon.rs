use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rayon::prelude::*;

use super::desktop::DesktopEntry;

const EXTENSIONS: &[&str] = &["svg", "svgz", "png"];

static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Resolve icon paths and preload all icon bytes into memory.
pub fn resolve_icons(mut entries: Vec<DesktopEntry>) -> Vec<DesktopEntry> {
    let themes = detect_themes();
    tracing::info!("icon themes: {:?}", themes);

    let cache = build_cache(&themes);
    tracing::info!("icon cache: {} icons indexed", cache.len());

    // Step 1: assign paths.
    for entry in &mut entries {
        if let Some(ref name) = entry.icon_name {
            if name.starts_with('/') {
                let p = PathBuf::from(name);
                if p.exists() {
                    entry.icon_path = Some(p);
                }
            } else {
                entry.icon_path = cache.get(name.as_str()).cloned();
            }
        }
    }

    // Step 2: parallel-read all icon files into memory.
    let loaded: Vec<Option<Arc<gpui::Image>>> = entries
        .par_iter()
        .map(|entry| {
            let path = entry.icon_path.as_ref()?;
            let bytes = fs::read(path).ok()?;
            let format = format_from_ext(path)?;
            Some(Arc::new(gpui::Image {
                format,
                bytes,
                id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed),
            }))
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

fn format_from_ext(path: &Path) -> Option<gpui::ImageFormat> {
    match path.extension()?.to_str()? {
        "svg" | "svgz" => Some(gpui::ImageFormat::Svg),
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        "xpm" => None, // gpui 不支持 XPM
        _ => None,
    }
}

/// Determine theme search order: user theme → Adwaita → hicolor.
fn detect_themes() -> Vec<String> {
    let mut themes = Vec::new();

    // Try reading GTK/gsettings icon theme.
    if let Some(t) = read_gtk_icon_theme() {
        add_theme_with_parents(&t, &mut themes);
    }

    // Hardcoded fallbacks (like rofi).
    for fallback in &["Adwaita", "gnome", "hicolor"] {
        add_theme_with_parents(fallback, &mut themes);
    }

    themes
}

/// Add a theme and its Inherits= parents, deduplicating.
fn add_theme_with_parents(name: &str, themes: &mut Vec<String>) {
    if themes.iter().any(|t| t == name) {
        return;
    }
    let root = PathBuf::from("/usr/share/icons").join(name);
    if !root.is_dir() {
        return;
    }
    themes.push(name.to_string());

    // Parse Inherits= from index.theme.
    let index = root.join("index.theme");
    if let Ok(content) = fs::read_to_string(&index) {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("Inherits=") {
                for parent in rest.split(',') {
                    let parent = parent.trim();
                    if !parent.is_empty() {
                        add_theme_with_parents(parent, themes);
                    }
                }
                break;
            }
        }
    }
}

/// Try to read the user's icon theme from GTK settings or gsettings.
fn read_gtk_icon_theme() -> Option<String> {
    let config = dirs_config();
    for path in &[
        config.join("gtk-3.0/settings.ini"),
        config.join("gtk-4.0/settings.ini"),
    ] {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("gtk-icon-theme-name") {
                    let val = rest.trim_start_matches(['=', ' '].as_ref()).trim();
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

fn dirs_config() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        })
}

/// Collect all app-icon directories to scan, ordered by priority.
/// Returns (priority, dir_path) pairs.
fn collect_scan_dirs(themes: &[String]) -> Vec<(usize, PathBuf)> {
    // Preferred size subdirectories for apps (best first).
    let size_dirs = [
        "scalable/apps",
        "48x48/apps",
        "32x32/apps",
        "64x64/apps",
        "24x24/apps",
        "96x96/apps",
        "128x128/apps",
        "256x256/apps",
        "22x22/apps",
        "16x16/apps",
        "512x512/apps",
    ];

    // Breeze-style: apps/{size}/
    let breeze_sizes = ["48", "32", "64", "22", "16"];

    let mut dirs = Vec::new();

    for (theme_idx, theme) in themes.iter().enumerate() {
        let root = PathBuf::from("/usr/share/icons").join(theme);

        // Standard layout: {root}/{size}/apps/
        for (size_idx, sub) in size_dirs.iter().enumerate() {
            let dir = root.join(sub);
            if dir.is_dir() {
                dirs.push((theme_idx * 1000 + size_idx, dir));
            }
        }

        // Breeze layout: {root}/apps/{size}/
        let apps_dir = root.join("apps");
        if apps_dir.is_dir() {
            for (size_idx, sz) in breeze_sizes.iter().enumerate() {
                let dir = apps_dir.join(sz);
                if dir.is_dir() {
                    dirs.push((theme_idx * 1000 + 50 + size_idx, dir));
                }
            }
        }
    }

    // /usr/share/pixmaps — lowest priority.
    let pixmaps = PathBuf::from("/usr/share/pixmaps");
    if pixmaps.is_dir() {
        dirs.push((themes.len() * 1000, pixmaps));
    }

    dirs
}

/// Scan directories in parallel, build name→path map (lowest priority wins).
fn build_cache(themes: &[String]) -> HashMap<String, PathBuf> {
    let scan_dirs = collect_scan_dirs(themes);

    let found: Vec<(String, PathBuf, usize)> = scan_dirs
        .par_iter()
        .flat_map(|(priority, dir)| {
            let mut results = Vec::new();
            if let Ok(rd) = fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if !is_icon_file(&path) {
                        continue;
                    }
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        results.push((stem.to_string(), path, *priority));
                    }
                }
            }
            results
        })
        .collect();

    let mut cache: HashMap<String, (PathBuf, usize)> = HashMap::with_capacity(found.len());
    for (name, path, priority) in found {
        cache
            .entry(name)
            .and_modify(|existing| {
                if priority < existing.1 {
                    *existing = (path.clone(), priority);
                }
            })
            .or_insert((path, priority));
    }

    cache.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn is_icon_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| EXTENSIONS.iter().any(|&e| e.eq_ignore_ascii_case(ext)))
}
