//! Bundled SVG asset source. The whole `assets/icons/` tree is embedded at
//! compile time via `include_dir!`, so the binary needs no on-disk data.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use include_dir::{include_dir, Dir, File};

static ICONS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/icons");

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let stripped = path.strip_prefix("icons/").unwrap_or(path);
        Ok(ICONS
            .get_file(stripped)
            .map(|f| Cow::Borrowed(f.contents())))
    }

    fn list(&self, prefix: &str) -> Result<Vec<SharedString>> {
        let stripped = prefix.strip_prefix("icons/").unwrap_or(prefix);
        let mut out = Vec::new();
        collect_files(&ICONS, &mut out);
        out.retain(|p| p.starts_with(stripped));
        Ok(out
            .into_iter()
            .map(|p| SharedString::from(format!("icons/{p}")))
            .collect())
    }
}

fn collect_files(dir: &Dir<'_>, out: &mut Vec<String>) {
    for f in dir.files() {
        push_path(f, out);
    }
    for d in dir.dirs() {
        collect_files(d, out);
    }
}

fn push_path(file: &File<'_>, out: &mut Vec<String>) {
    out.push(file.path().display().to_string());
}
