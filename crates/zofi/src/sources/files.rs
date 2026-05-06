// Currently unregistered (see crates/zofi/src/main.rs `build_registry`).
// Module is kept compilable so it can be re-enabled by adding it back to
// the registry — silence dead-code lints meanwhile.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{div, prelude::*, svg, AnyElement, FontWeight, ImageFormat, SharedString};
use walkdir::WalkDir;

use crate::source::{ActivateOutcome, Layout, Preview, Source};
use crate::theme;

const MAX_ENTRIES: usize = 200_000;
const MAX_DEPTH: usize = 12;
const PREVIEW_TEXT_BYTES: usize = 100_000;
/// Send a pulse every N entries appended. Larger = fewer re-renders, smoother
/// but laggier; smaller = more reactive but more wakeups.
const PULSE_EVERY: usize = 500;
/// Walker accumulates this many entries locally before taking the inner lock
/// once — reduces contention with `filter()` while typing.
const PUSH_BATCH: usize = 64;

/// Directories never recursed into; project-level junk that's almost never
/// what the user is looking for.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".cache",
    ".local/share/Trash",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
];

static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Single shared state mutated by both the walker thread and the
/// launcher-thread filter calls. Filtering is incremental: walker keeps
/// appending; `filter()` only scans the slice we haven't matched yet.
#[derive(Default)]
struct Inner {
    /// Current root being listed. Changes when the user clicks a directory.
    root: PathBuf,
    entries: Vec<PathBuf>,
    /// Lower-cased relative path strings, parallel to `entries`.
    keys: Vec<String>,
    matched: Vec<usize>,
    query: String,
    scanned: usize,
    /// Bumped on each navigate. Walker threads carry the value they were
    /// spawned with and exit when the live counter has moved on.
    walk_id: u64,
}

pub struct FilesSource {
    inner: Arc<Mutex<Inner>>,
    pulse_tx: async_channel::Sender<()>,
    pulse_rx: Option<async_channel::Receiver<()>>,
}

impl FilesSource {
    pub fn load() -> Self {
        let root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let inner = Arc::new(Mutex::new(Inner {
            root: root.clone(),
            ..Inner::default()
        }));
        let (tx, rx) = async_channel::bounded::<()>(1);
        spawn_walker(inner.clone(), tx.clone(), 0);
        tracing::info!("files source: walking {root:?} in background");
        Self {
            inner,
            pulse_tx: tx,
            pulse_rx: Some(rx),
        }
    }

    /// Switch to a new root: bump walk_id (telling old walker to bail), reset
    /// state, spawn a fresh walker. Pulse channel is reused so the launcher's
    /// already-subscribed task keeps firing.
    fn navigate(&self, new_root: PathBuf) {
        let walk_id;
        {
            let mut g = self.inner.lock().unwrap();
            g.walk_id = g.walk_id.wrapping_add(1);
            walk_id = g.walk_id;
            g.root = new_root.clone();
            g.entries.clear();
            g.keys.clear();
            g.matched.clear();
            g.query.clear();
            g.scanned = 0;
        }
        spawn_walker(self.inner.clone(), self.pulse_tx.clone(), walk_id);
        let _ = self.pulse_tx.try_send(());
        tracing::info!("files source: navigated to {new_root:?}");
    }
}

impl Source for FilesSource {
    fn name(&self) -> &'static str {
        "files"
    }

    fn icon(&self) -> &'static str {
        "▦"
    }

    fn prefix(&self) -> Option<char> {
        Some('/')
    }

    fn placeholder(&self) -> &'static str {
        "Search files..."
    }

    fn empty_text(&self) -> &'static str {
        "No matching files (still walking…)"
    }

    fn layout(&self) -> Layout {
        Layout::ListAndPreview
    }

    /// Incremental fuzzy-substring filter. Three fast paths:
    /// 1. Same query (pulse re-fire) → scan only entries the walker has
    ///    appended since the last call.
    /// 2. Extending the query (user typed another char) → narrow the previous
    ///    `matched` set in place; for new entries, scan with the full query.
    /// 3. Anything else (shrinkage, unrelated query) → full rescan.
    fn filter(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        let mut g = self.inner.lock().unwrap();

        let same = q == g.query;
        let extending = !same && !g.query.is_empty() && q.starts_with(g.query.as_str());

        if same {
            let start = g.scanned;
            let end = g.entries.len();
            if q.is_empty() {
                g.matched.extend(start..end);
            } else {
                for i in start..end {
                    if g.keys[i].contains(&q) {
                        g.matched.push(i);
                    }
                }
            }
            g.scanned = end;
        } else if extending {
            let prev: Vec<usize> = std::mem::take(&mut g.matched);
            for ix in prev {
                if g.keys[ix].contains(&q) {
                    g.matched.push(ix);
                }
            }
            let start = g.scanned;
            let end = g.entries.len();
            for i in start..end {
                if g.keys[i].contains(&q) {
                    g.matched.push(i);
                }
            }
            g.scanned = end;
            g.query = q;
        } else {
            g.matched.clear();
            let n = g.entries.len();
            if q.is_empty() {
                g.matched.extend(0..n);
            } else {
                for i in 0..n {
                    if g.keys[i].contains(&q) {
                        g.matched.push(i);
                    }
                }
            }
            g.scanned = n;
            g.query = q;
        }
        g.matched.clone()
    }

    fn render_item(&self, ix: usize, selected: bool, theme_global: &ztheme::Theme) -> AnyElement {
        let g = self.inner.lock().unwrap();
        let Some(path) = g.entries.get(ix).cloned() else {
            return div().into_any_element();
        };
        let root = g.root.clone();
        drop(g);
        let is_dir = path.is_dir();
        let (icon_path, icon_color) = icon_for(&path, is_dir);
        let (name, parent) = split_name_parent(&path, &root);

        div()
            .h_full()
            .px(theme::PAD_X)
            .flex()
            .items_center()
            .gap(theme::GAP)
            .child(
                svg()
                    .path(icon_path)
                    .size(gpui::px(16.0))
                    .flex_shrink_0()
                    .text_color(icon_color),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(gpui::px(220.0))
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
                        theme_global.fg_accent
                    } else {
                        theme_global.fg
                    })
                    .child(SharedString::from(name)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme::FONT_SIZE_SM)
                    .text_color(theme_global.fg_dim)
                    .child(SharedString::from(parent)),
            )
            .into_any_element()
    }

    fn activate(&self, ix: usize) -> ActivateOutcome {
        let path = match self.inner.lock().unwrap().entries.get(ix).cloned() {
            Some(p) => p,
            None => return ActivateOutcome::Quit,
        };
        if path.is_dir() {
            self.navigate(path);
            return ActivateOutcome::Refresh;
        }
        match Command::new("xdg-open")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => tracing::info!("opened {path:?}"),
            Err(e) => tracing::error!("xdg-open {path:?}: {e}"),
        }
        ActivateOutcome::Quit
    }

    fn take_pulse(&mut self) -> Option<async_channel::Receiver<()>> {
        self.pulse_rx.take()
    }

    fn preview(&self, ix: usize) -> Option<Preview> {
        let path = self.inner.lock().unwrap().entries.get(ix).cloned()?;
        let path = path.as_path();
        if path.is_dir() {
            return Some(Preview::Text(directory_listing(path)));
        }
        if let Some(format) = image_format_from_path(path) {
            if let Ok(bytes) = std::fs::read(path) {
                return Some(Preview::Image(Arc::new(gpui::Image {
                    format,
                    bytes,
                    id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed),
                })));
            }
        }
        let body = text_preview(path);
        let lang = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !lang.is_empty() && crate::highlight::syntax_for(&lang).is_some() {
            Some(Preview::Code { text: body, lang })
        } else {
            Some(Preview::Text(body))
        }
    }
}

use gpui::{rgb, Hsla};

/// (svg path, color) for a file/dir. Colors are language-brand-ish where
/// recognizable, semantically grouped otherwise. All assets are bundled.
fn icon_for(path: &Path, is_dir: bool) -> (&'static str, Hsla) {
    if is_dir {
        return ("icons/file_icons/folder.svg", color(0xe2c275));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match name.as_str() {
        "dockerfile" | ".dockerignore" => return ("icons/file_icons/docker.svg", color(0x2496ed)),
        ".gitignore" | ".gitattributes" | ".gitmodules" => {
            return ("icons/file_icons/git.svg", color(0xf1502f));
        }
        ".editorconfig" => return ("icons/file_icons/editorconfig.svg", color(0x9da3a8)),
        ".eslintrc" | ".eslintrc.json" | ".eslintrc.js" => {
            return ("icons/file_icons/eslint.svg", color(0x4b32c3));
        }
        ".prettierrc" | ".prettierrc.json" => {
            return ("icons/file_icons/prettier.svg", color(0xf7b93e));
        }
        "package.json" | "package-lock.json" => {
            return ("icons/file_icons/javascript.svg", color(0xf1e05a));
        }
        "cargo.toml" | "cargo.lock" => return ("icons/file_icons/rust.svg", color(0xdea584)),
        "makefile" => return ("icons/file_icons/code.svg", color(0x9da3a8)),
        _ => {}
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "rs" => ("icons/file_icons/rust.svg", color(0xdea584)),
        "py" => ("icons/file_icons/python.svg", color(0x3572a5)),
        "js" | "mjs" | "cjs" => ("icons/file_icons/javascript.svg", color(0xf1e05a)),
        "ts" => ("icons/file_icons/typescript.svg", color(0x3178c6)),
        "tsx" | "jsx" => ("icons/file_icons/react.svg", color(0x61dafb)),
        "go" => ("icons/file_icons/go.svg", color(0x00add8)),
        "rb" => ("icons/file_icons/ruby.svg", color(0xcc342d)),
        "java" => ("icons/file_icons/java.svg", color(0xb07219)),
        "kt" | "kts" => ("icons/file_icons/kotlin.svg", color(0xa97bff)),
        "swift" => ("icons/file_icons/swift.svg", color(0xf05138)),
        "c" | "h" => ("icons/file_icons/c.svg", color(0x555555)),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => {
            ("icons/file_icons/cpp.svg", color(0xf34b7d))
        }
        "lua" | "luau" => ("icons/file_icons/lua.svg", color(0x000080)),
        "scala" | "sc" => ("icons/file_icons/scala.svg", color(0xc22d40)),
        "ex" | "exs" => ("icons/file_icons/elixir.svg", color(0x9268bf)),
        "elm" => ("icons/file_icons/elm.svg", color(0x60b5cc)),
        "erl" | "hrl" => ("icons/file_icons/erlang.svg", color(0xb83998)),
        "hs" | "lhs" => ("icons/file_icons/haskell.svg", color(0x5e5086)),
        "ml" | "mli" => ("icons/file_icons/ocaml.svg", color(0xee6a1a)),
        "fs" | "fsx" | "fsi" => ("icons/file_icons/fsharp.svg", color(0xb845fc)),
        "dart" => ("icons/file_icons/dart.svg", color(0x00b4ab)),
        "nim" => ("icons/file_icons/nim.svg", color(0xffe953)),
        "nix" => ("icons/file_icons/nix.svg", color(0x5277c3)),
        "zig" => ("icons/file_icons/zig.svg", color(0xec915c)),
        "v" => ("icons/file_icons/v.svg", color(0x4f87c4)),
        "vy" => ("icons/file_icons/vyper.svg", color(0x3a82c4)),
        "jl" => ("icons/file_icons/julia.svg", color(0xa270ba)),
        "r" => ("icons/file_icons/r.svg", color(0x198ce7)),
        "php" => ("icons/file_icons/php.svg", color(0x777bb4)),
        "tcl" => ("icons/file_icons/tcl.svg", color(0xe4cc98)),
        "wgsl" => ("icons/file_icons/wgsl.svg", color(0x1a5490)),
        "metal" => ("icons/file_icons/metal.svg", color(0x9c1f1f)),
        "tf" | "tfvars" => ("icons/file_icons/terraform.svg", color(0x844fba)),
        "hcl" => ("icons/file_icons/hcl.svg", color(0x844fba)),
        "sh" | "zsh" | "bash" | "fish" => ("icons/file_icons/terminal.svg", color(0x4eaa25)),
        "ipynb" => ("icons/file_icons/jupyter.svg", color(0xf37726)),

        "html" | "htm" => ("icons/file_icons/html.svg", color(0xe34c26)),
        "css" => ("icons/file_icons/css.svg", color(0x1572b6)),
        "scss" | "sass" => ("icons/file_icons/sass.svg", color(0xcd6799)),
        "vue" => ("icons/file_icons/vue.svg", color(0x4fc08d)),
        "astro" => ("icons/file_icons/astro.svg", color(0xff5d01)),
        "graphql" | "gql" => ("icons/file_icons/graphql.svg", color(0xe10098)),

        "toml" => ("icons/file_icons/toml.svg", color(0x9c4221)),
        "yaml" | "yml" => ("icons/file_icons/yaml.svg", color(0xcb171e)),
        "json" | "jsonc" => ("icons/file_icons/code.svg", color(0xf1e05a)),
        "ini" | "conf" | "cfg" => ("icons/file_icons/code.svg", color(0x9da3a8)),
        "kdl" => ("icons/file_icons/kdl.svg", color(0x6c8ebf)),

        "md" | "markdown" => ("icons/file_icons/code.svg", color(0x519aba)),
        "txt" | "rst" | "org" | "tex" | "log" => ("icons/file.svg", color(0xb0b8c0)),
        "pdf" | "epub" | "mobi" => ("icons/file_doc.svg", color(0xe85a4f)),

        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "tiff" => {
            ("icons/file_icons/image.svg", color(0xa074c4))
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "opus" | "aac" => {
            ("icons/file_icons/audio.svg", color(0xee5396))
        }
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4v" => {
            ("icons/file_icons/video.svg", color(0xff8a65))
        }

        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "tgz" | "zst" => {
            ("icons/file_icons/archive.svg", color(0xa1887f))
        }

        "lock" => ("icons/file_icons/lock.svg", color(0x9da3a8)),
        "diff" | "patch" => ("icons/file_icons/diff.svg", color(0x4eaa25)),
        "ttf" | "otf" | "woff" | "woff2" => ("icons/file_icons/font.svg", color(0xc0c0d0)),
        "db" | "sqlite" | "sqlite3" => ("icons/file_icons/database.svg", color(0x6c8ebf)),
        "ai" => ("icons/file_icons/ai.svg", color(0xff7c00)),

        _ => ("icons/file_icons/file.svg", color(0x9da3a8)),
    }
}

fn color(rgb_hex: u32) -> Hsla {
    rgb(rgb_hex).into()
}

fn split_name_parent(path: &Path, root: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let parent = match path.parent() {
        Some(p) if p == Path::new("") => String::new(),
        Some(p) => match p.strip_prefix(root) {
            Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => p.display().to_string(),
        },
        None => String::new(),
    };
    (name, parent)
}

/// Walk the current root in a background thread. `my_walk_id` matches the
/// `Inner.walk_id` we were spawned with — if the user navigates again, the
/// counter advances and we exit on the next iteration.
fn spawn_walker(inner: Arc<Mutex<Inner>>, pulse: async_channel::Sender<()>, my_walk_id: u64) {
    let root = inner.lock().unwrap().root.clone();
    std::thread::spawn(move || {
        let mut local: Vec<(PathBuf, String)> = Vec::with_capacity(PUSH_BATCH);
        let mut count_since_pulse = 0usize;
        let mut total = 0usize;
        let walker = WalkDir::new(&root)
            .max_depth(MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_skip(e));

        for entry in walker {
            let path = match entry {
                Ok(e) => e.into_path(),
                Err(err) => {
                    tracing::debug!("skip walk entry: {err}");
                    continue;
                }
            };
            let key = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_lowercase();
            local.push((path, key));
            total += 1;
            count_since_pulse += 1;

            if local.len() >= PUSH_BATCH && !flush_batch(&inner, &mut local, my_walk_id) {
                return;
            }
            if count_since_pulse >= PULSE_EVERY {
                count_since_pulse = 0;
                let _ = pulse.try_send(());
                if pulse.is_closed() {
                    return;
                }
            }
            if total >= MAX_ENTRIES {
                tracing::warn!("file walk hit cap of {MAX_ENTRIES}; truncating");
                break;
            }
        }
        flush_batch(&inner, &mut local, my_walk_id);
        let _ = pulse.try_send(());
        tracing::debug!("walk {my_walk_id} done in {root:?}, {total} entries");
    });
}

/// Take the lock once and push the whole batch. Returns false if the walker
/// has been superseded by a navigate (caller should exit).
fn flush_batch(
    inner: &Arc<Mutex<Inner>>,
    batch: &mut Vec<(PathBuf, String)>,
    my_walk_id: u64,
) -> bool {
    let mut g = inner.lock().unwrap();
    if g.walk_id != my_walk_id {
        return false;
    }
    for (p, k) in batch.drain(..) {
        g.entries.push(p);
        g.keys.push(k);
    }
    true
}

fn is_skip(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') && entry.depth() > 0 {
        return true;
    }
    if entry.file_type().is_dir() && SKIP_DIRS.iter().any(|s| name == *s) {
        return true;
    }
    false
}

fn image_format_from_path(path: &Path) -> Option<ImageFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        _ => None,
    }
}

fn text_preview(path: &Path) -> String {
    let metadata = std::fs::metadata(path).ok();
    let header = match &metadata {
        Some(m) => format!("{}\n{} bytes\n\n", path.display(), m.len()),
        None => format!("{}\n\n", path.display()),
    };

    match read_text_capped(path, PREVIEW_TEXT_BYTES) {
        Ok(text) => format!("{header}{text}"),
        Err(_) => format!("{header}(binary or unreadable)"),
    }
}

fn read_text_capped(path: &Path, cap: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    if buf.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "binary content (NUL byte)",
        ));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn directory_listing(path: &Path) -> String {
    let mut lines = vec![format!("{}/", path.display()), String::new()];
    let read = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(e) => return format!("{}: {e}", path.display()),
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries.iter().take(200) {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        lines.push(if is_dir { format!("{name}/") } else { name });
    }
    if entries.len() > 200 {
        lines.push(format!("… ({} more)", entries.len() - 200));
    }
    lines.join("\n")
}
