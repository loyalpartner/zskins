//! Shared icon-theme → `Arc<gpui::Image>` resolution.
//!
//! Both `AppsSource` (batch, at load time) and `WindowsSource` (streaming, per
//! event) need the same lookup chain:
//!
//! ```text
//! name → IconCache::lookup → PathBuf → fs::read → gpui::Image
//! ```
//!
//! Centralising it here keeps the format→MIME mapping and ID allocation in one
//! place so the two call sites can't drift.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Monotonic image IDs — gpui uses these to key its texture cache, so every
/// `Image` we create must get a unique one. Shared across all callers to avoid
/// accidental ID collisions when both sources construct images concurrently.
static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh image id for callers that build `gpui::Image` outside this
/// module (e.g. in-memory PNG buffers like window thumbnails). Shared atomic so
/// every `Image` across the process has a unique texture-cache key.
pub(crate) fn next_image_id() -> u64 {
    NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Read an icon file from disk and wrap it in a `gpui::Image`. Returns `None`
/// if the file is unreadable or its extension isn't a format gpui supports —
/// callers then fall back to a placeholder glyph.
pub(crate) fn load_image_from_path(path: &Path) -> Option<Arc<gpui::Image>> {
    let bytes = fs::read(path).ok()?;
    let format = format_from_ext(path)?;
    Some(Arc::new(gpui::Image {
        format,
        bytes,
        id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed),
    }))
}

/// Look up an icon by name in the cache, then load it into memory. Convenience
/// wrapper used by streaming consumers (WindowsSource) that resolve one icon
/// per event rather than a whole batch up front.
pub(crate) fn resolve_by_name(
    cache: &icon_theme::IconCache,
    name: &str,
) -> Option<Arc<gpui::Image>> {
    // Honour absolute paths directly — matches the AppsSource convention where
    // a `.desktop` Icon= can be a full filesystem path rather than a theme name.
    if name.starts_with('/') {
        return load_image_from_path(Path::new(name));
    }
    for candidate in name_candidates(name) {
        if let Some(path) = cache.lookup(&candidate) {
            return load_image_from_path(path);
        }
    }
    None
}

/// Generate icon-name candidates for a Wayland `app_id`. Covers the common
/// conventions we see in the wild:
///   - verbatim (`foot`, `google-chrome`)
///   - lowercased (`Feishu` → `feishu`)
///   - reverse-DNS last segment (`com.bytedance.Feishu` → `Feishu` → `feishu`)
///   - version/distro suffix stripped (`firefox-esr` → `firefox`)
pub(crate) fn name_candidates(app_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(4);
    let push = |out: &mut Vec<String>, v: String| {
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    };

    push(&mut out, app_id.to_string());
    push(&mut out, app_id.to_lowercase());

    if let Some(last) = app_id.rsplit('.').next() {
        if last != app_id {
            push(&mut out, last.to_string());
            push(&mut out, last.to_lowercase());
        }
    }

    for base in out.clone() {
        if let Some(stem) = base.split('-').next() {
            if stem != base {
                push(&mut out, stem.to_string());
            }
        }
    }

    out
}

/// Build an `app_id → Arc<Image>` map from already-loaded `.desktop` entries.
///
/// `AppsSource` has done the heavy lifting — every entry with a preloaded icon
/// is indexed under every plausible matching key so `WindowsSource` can look up
/// a Wayland `app_id` without a second filesystem scan. Keys considered:
/// * `name` (e.g. `"Feishu"`)
/// * `icon_name` (e.g. `"feishu"`)
/// * `file_stem` (e.g. `"org.feishu.client"`)
/// * `startup_wm_class` (e.g. `"Feishu"`)
///
/// All keys are lowercased; the same `Arc<Image>` is shared by every key that
/// points at one entry, so the map stays cheap even with collisions.
pub(crate) fn index_apps_by_app_id(
    entries: &[crate::sources::apps::DesktopEntry],
) -> HashMap<String, Arc<gpui::Image>> {
    let mut map: HashMap<String, Arc<gpui::Image>> = HashMap::new();
    for entry in entries {
        let Some(icon) = entry.icon_data.clone() else {
            continue;
        };
        let mut keys: Vec<String> = Vec::with_capacity(4);
        keys.push(entry.name.to_lowercase());
        if let Some(ref icon_name) = entry.icon_name {
            keys.push(icon_name.to_lowercase());
        }
        if !entry.file_stem.is_empty() {
            keys.push(entry.file_stem.to_lowercase());
        }
        if let Some(ref wm_class) = entry.startup_wm_class {
            keys.push(wm_class.to_lowercase());
        }
        for key in keys {
            if key.is_empty() {
                continue;
            }
            // First entry wins on collision — matches AppsSource's
            // "user overrides system" precedence.
            map.entry(key).or_insert_with(|| icon.clone());
        }
    }
    map
}

/// Compose an icon resolver that prefers `.desktop`-sourced icons (already in
/// memory) and falls back to `icon-theme` for app-ids that don't correspond to
/// an installed `.desktop` entry. Used by the launcher to give `WindowsSource`
/// the same icon pool `AppsSource` sees — critical for third-party apps
/// (Feishu, proprietary installers) whose icons live at absolute paths that
/// the icon-theme spec can't discover on its own.
/// Fallback resolver used when the `.desktop` app-map misses. Boxed + `Send +
/// Sync` because the composite resolver is shared across threads.
type FallbackResolver = Arc<dyn Fn(&str) -> Option<Arc<gpui::Image>> + Send + Sync>;

pub(crate) fn build_window_icon_resolver(
    entries: &[crate::sources::apps::DesktopEntry],
) -> crate::sources::windows::IconResolver {
    let app_map = Arc::new(index_apps_by_app_id(entries));
    // Lazy: building a second `IconCache` doubles the `/usr/share/icons` scan
    // we already paid for in `AppsSource::resolve_icons`. Most app_ids resolve
    // through `app_map` (an installed .desktop exists), so the theme fallback
    // is rarely needed. Defer the scan until the first miss.
    let cache: Arc<std::sync::OnceLock<icon_theme::IconCache>> =
        Arc::new(std::sync::OnceLock::new());
    let fallback: FallbackResolver = Arc::new(move |name: &str| {
        let cache = cache.get_or_init(|| icon_theme::IconCache::new(&["apps"]));
        resolve_by_name(cache, name)
    });
    compose_resolver(app_map, fallback)
}

/// Split the closure so tests can inject a fake fallback without touching
/// the filesystem-backed `IconCache`.
fn compose_resolver(
    app_map: Arc<HashMap<String, Arc<gpui::Image>>>,
    fallback: FallbackResolver,
) -> crate::sources::windows::IconResolver {
    Arc::new(move |app_id: &str| {
        for candidate in name_candidates(app_id) {
            if let Some(icon) = app_map.get(&candidate.to_lowercase()) {
                return Some(icon.clone());
            }
        }
        fallback(app_id)
    })
}

pub(crate) fn format_from_ext(path: &Path) -> Option<gpui::ImageFormat> {
    match path.extension()?.to_str()? {
        "svg" | "svgz" => Some(gpui::ImageFormat::Svg),
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::apps::DesktopEntry;
    use std::sync::Mutex;

    #[test]
    fn verbatim_first() {
        assert_eq!(name_candidates("foot")[0], "foot");
    }

    #[test]
    fn lowercases_capitalized() {
        let c = name_candidates("Feishu");
        assert!(c.contains(&"Feishu".into()));
        assert!(c.contains(&"feishu".into()));
    }

    #[test]
    fn unwraps_reverse_dns() {
        let c = name_candidates("com.bytedance.Feishu");
        assert!(c.contains(&"Feishu".into()));
        assert!(c.contains(&"feishu".into()));
    }

    #[test]
    fn strips_version_suffix() {
        let c = name_candidates("firefox-esr");
        assert!(c.contains(&"firefox".into()));
    }

    #[test]
    fn deduplicates() {
        let c = name_candidates("foot");
        assert_eq!(c.iter().filter(|s| s.as_str() == "foot").count(), 1);
    }

    #[test]
    fn handles_empty() {
        assert!(name_candidates("").is_empty());
    }

    // ------------------------------------------------------------------
    // DesktopEntry → app_id index tests
    // ------------------------------------------------------------------

    fn stub_icon() -> Arc<gpui::Image> {
        Arc::new(gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes: Vec::new(),
            id: 0,
        })
    }

    fn entry(
        name: &str,
        icon_name: Option<&str>,
        file_stem: &str,
        wm_class: Option<&str>,
        with_icon: bool,
    ) -> DesktopEntry {
        DesktopEntry {
            name: name.to_string().into(),
            exec: String::new(),
            icon_name: icon_name.map(str::to_string),
            icon_path: None,
            icon_data: if with_icon { Some(stub_icon()) } else { None },
            search_key: name.to_lowercase(),
            file_stem: file_stem.to_string(),
            desktop_path: std::path::PathBuf::new(),
            startup_wm_class: wm_class.map(str::to_string),
        }
    }

    #[test]
    fn index_apps_by_name() {
        // An entry whose Name="Feishu" must be reachable under the lowercase
        // "feishu" key — the common Wayland app_id for that app.
        let entries = vec![entry("Feishu", None, "", None, true)];
        let map = index_apps_by_app_id(&entries);
        assert!(map.contains_key("feishu"));
    }

    #[test]
    fn index_apps_by_icon_name() {
        // icon_name differs from name. The icon_name must still be a lookup
        // key — many apps name themselves "Visual Studio Code" with icon
        // "code".
        let entries = vec![entry("Visual Studio Code", Some("code"), "", None, true)];
        let map = index_apps_by_app_id(&entries);
        assert!(map.contains_key("code"));
    }

    #[test]
    fn index_apps_by_file_stem() {
        // The .desktop file stem (e.g. "org.feishu.client") frequently matches
        // the compositor's app_id for GTK apps.
        let entries = vec![entry("Feishu", None, "org.feishu.client", None, true)];
        let map = index_apps_by_app_id(&entries);
        assert!(map.contains_key("org.feishu.client"));
    }

    #[test]
    fn index_apps_by_wm_class() {
        // StartupWMClass is the explicit hint from the .desktop spec linking
        // a window to its entry — indispensable for X11/XWayland apps.
        let entries = vec![entry("Feishu Thing", None, "", Some("Feishu"), true)];
        let map = index_apps_by_app_id(&entries);
        assert!(map.contains_key("feishu"));
    }

    #[test]
    fn index_skips_entries_without_icon_data() {
        // No icon_data → no map entry: we're only indexing what we can
        // actually render.
        let entries = vec![entry("Ghost", Some("ghost"), "ghost", None, false)];
        let map = index_apps_by_app_id(&entries);
        assert!(map.is_empty());
    }

    #[test]
    fn composite_resolver_hits_apps_before_theme() {
        // Hit path: app_map matches → fallback must not be invoked.
        let icon = stub_icon();
        let mut app_map: HashMap<String, Arc<gpui::Image>> = HashMap::new();
        app_map.insert("feishu".into(), icon.clone());
        let calls: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let calls_clone = calls.clone();
        let fallback: FallbackResolver = Arc::new(move |_: &str| {
            *calls_clone.lock().unwrap() += 1;
            None
        });
        let resolver = compose_resolver(Arc::new(app_map), fallback);

        let got = resolver("Feishu").expect("app_map should hit");
        assert!(Arc::ptr_eq(&got, &icon));
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "fallback must not be called on hit"
        );
    }

    #[test]
    fn composite_resolver_falls_back_to_theme_on_miss() {
        // Miss path: app_map empty → fallback is the only source.
        let fallback_icon = stub_icon();
        let fallback_icon_clone = fallback_icon.clone();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let fallback: FallbackResolver = Arc::new(move |name: &str| {
            calls_clone.lock().unwrap().push(name.to_string());
            Some(fallback_icon_clone.clone())
        });
        let resolver = compose_resolver(Arc::new(HashMap::new()), fallback);

        let got = resolver("unknown-app").expect("fallback should hit");
        assert!(Arc::ptr_eq(&got, &fallback_icon));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["unknown-app".to_string()]
        );
    }
}
