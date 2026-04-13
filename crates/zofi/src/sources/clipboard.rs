use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, FontWeight, ImageFormat, SharedString};
use zofi_clipd::{
    db::Db,
    ipc::{self, Request, Response},
    model::Kind,
    paths, pidfile, Entry,
};

use crate::source::{ActivateOutcome, Layout, Preview, Source};
use crate::theme;

const LIST_LIMIT: usize = 500;

static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

pub struct ClipboardSource {
    entries: Vec<Entry>,
    /// Pre-computed lowercase preview strings for case-insensitive substring filter.
    search_keys: Vec<String>,
    /// Pre-decoded image objects, parallel to `entries`. None for text rows or
    /// images with unsupported mime. Pre-allocating a stable `Arc<Image>` (and
    /// a stable id) keeps GPUI's image cache hot across renders.
    images: Vec<Option<Arc<gpui::Image>>>,
    daemon_running: bool,
    daemon_error: Option<String>,
}

impl ClipboardSource {
    pub fn load() -> Self {
        let pid_path = paths::pid_path();
        let mut daemon_running = pidfile::probe(&pid_path);
        let mut daemon_error = None;
        if !daemon_running {
            match spawn_daemon() {
                Ok(()) => {
                    daemon_running = wait_for_daemon(&pid_path, Duration::from_millis(1500));
                    if !daemon_running {
                        daemon_error = Some("spawned but did not become ready in 1.5s".into());
                    }
                }
                Err(e) => {
                    let msg = format!("{e}");
                    tracing::warn!("failed to spawn zofi-clipd: {msg}");
                    daemon_error = Some(msg);
                }
            }
        }
        let entries = match Self::load_entries() {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("load clipboard entries: {e:#}");
                Vec::new()
            }
        };
        let search_keys = entries
            .iter()
            .map(|e| e.preview.clone().unwrap_or_default().to_lowercase())
            .collect();
        let images = entries.iter().map(decode_image).collect();
        tracing::info!(
            "clipboard source: {} entries, daemon {}",
            entries.len(),
            if daemon_running { "up" } else { "DOWN" }
        );
        Self {
            entries,
            search_keys,
            images,
            daemon_running,
            daemon_error,
        }
    }

    fn load_entries() -> anyhow::Result<Vec<Entry>> {
        let path = paths::db_path()?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let db = Db::open(&path)?;
        let entries = db.list(LIST_LIMIT)?;
        Ok(entries)
    }
}

impl Source for ClipboardSource {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn icon(&self) -> &'static str {
        "▤"
    }

    fn placeholder(&self) -> &'static str {
        "Search clipboard..."
    }

    fn empty_text(&self) -> &'static str {
        "No clipboard entries"
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let q = query.to_lowercase();
        self.search_keys
            .iter()
            .enumerate()
            .filter(|(_, k)| k.contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn render_item(&self, ix: usize, selected: bool) -> AnyElement {
        let entry = &self.entries[ix];
        let label: SharedString = match entry.kind {
            Kind::Text => entry
                .preview
                .clone()
                .unwrap_or_else(|| "(empty)".to_string())
                .into(),
            Kind::Image => {
                let bytes = entry.primary_content().map(|c| c.len()).unwrap_or(0);
                format!("image · {} · {bytes} bytes", entry.primary_mime).into()
            }
        };
        let (badge_glyph, badge_fg, badge_bg): (&str, _, _) = match entry.kind {
            Kind::Text => ("¶", theme::kind_text_fg(), theme::kind_text_bg()),
            Kind::Image => ("◫", theme::kind_image_fg(), theme::kind_image_bg()),
        };

        div()
            .h_full()
            .px(theme::PAD_X)
            .flex()
            .items_center()
            .gap(theme::GAP)
            .child(
                div()
                    .flex_shrink_0()
                    .w(gpui::px(24.0))
                    .h(gpui::px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(gpui::px(4.0))
                    .bg(badge_bg)
                    .text_size(theme::FONT_SIZE)
                    .text_color(badge_fg)
                    .child(badge_glyph),
            )
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
                    .child(label),
            )
            .into_any_element()
    }

    fn activate(&self, ix: usize) -> ActivateOutcome {
        self.activate_inner(ix, None);
        ActivateOutcome::Quit
    }

    fn mimes(&self, ix: usize) -> Vec<String> {
        self.entries
            .get(ix)
            .map(|e| e.mimes.iter().map(|m| m.mime.clone()).collect())
            .unwrap_or_default()
    }

    fn primary_mime(&self, ix: usize) -> Option<String> {
        self.entries.get(ix).map(|e| e.primary_mime.clone())
    }

    fn preview_for_mime(&self, ix: usize, mime: &str) -> Option<Preview> {
        let entry = self.entries.get(ix)?;
        match entry.kind {
            Kind::Text => entry
                .content_for(mime)
                .map(|c| Preview::Text(String::from_utf8_lossy(c).into_owned())),
            Kind::Image => {
                // Cached primary-mime decode keeps GPUI's image atlas hot for
                // the common case; non-primary mimes mint a new Image (warmer
                // re-decode if the user navigates back).
                if mime == entry.primary_mime {
                    if let Some(Some(img)) = self.images.get(ix) {
                        return Some(Preview::Image(img.clone()));
                    }
                }
                match decode_image_for_mime(entry, mime) {
                    Some(img) => Some(Preview::Image(img)),
                    None => Some(Preview::Text(format!("[unsupported image mime: {mime}]"))),
                }
            }
        }
    }

    fn activate_with_mime(&self, ix: usize, mime: &str) -> ActivateOutcome {
        self.activate_inner(ix, Some(mime.to_string()));
        ActivateOutcome::Quit
    }

    fn layout(&self) -> Layout {
        Layout::ListAndPreview
    }

    fn banner(&self) -> Option<AnyElement> {
        if self.daemon_running {
            return None;
        }
        let detail = self
            .daemon_error
            .as_deref()
            .unwrap_or("daemon did not start");
        Some(daemon_warning(detail).into_any_element())
    }

    fn preview(&self, ix: usize) -> Option<Preview> {
        let entry = self.entries.get(ix)?;
        self.preview_for_mime(ix, &entry.primary_mime)
    }
}

impl ClipboardSource {
    fn activate_inner(&self, ix: usize, mime: Option<String>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        match ipc::send(&Request::Activate {
            uuid: entry.uuid.clone(),
            mime,
        }) {
            Ok(Response::Ok) => {}
            Ok(Response::Error { message }) => {
                tracing::error!("daemon refused activate: {message}");
            }
            Err(e) => tracing::error!("ipc activate: {e:#}"),
        }
    }
}

fn image_format_from_mime(mime: &str) -> Option<ImageFormat> {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        _ => None,
    }
}

fn decode_image(entry: &Entry) -> Option<Arc<gpui::Image>> {
    if entry.kind != Kind::Image {
        return None;
    }
    decode_image_for_mime(entry, &entry.primary_mime)
}

fn decode_image_for_mime(entry: &Entry, mime: &str) -> Option<Arc<gpui::Image>> {
    let format = image_format_from_mime(mime)?;
    let content = entry.content_for(mime)?.to_vec();
    Some(Arc::new(gpui::Image {
        format,
        bytes: content,
        id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed),
    }))
}

fn spawn_daemon() -> std::io::Result<()> {
    let bin = std::env::current_exe()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("clipd")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // setsid in the child so the daemon survives zofi exiting.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd.spawn()?;
    tracing::info!("spawned `{} clipd` pid={}", bin.display(), child.id());
    Ok(())
}

fn wait_for_daemon(pid_path: &std::path::Path, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if pidfile::probe(pid_path) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn daemon_warning(detail: &str) -> gpui::Div {
    div()
        .w_full()
        .px(theme::PAD_X)
        .py(px(6.0))
        .bg(gpui::rgb(0x3a_1a_1a))
        .text_size(theme::FONT_SIZE_SM)
        .text_color(gpui::rgb(0xff_8a_8a))
        .child(format!("⚠ zofi-clipd not running: {detail}"))
}
