//! Open-window launcher source backed by `zwindows` (wlr-foreign-toplevel).
//!
//! Lists the compositor's live toplevel windows with app-id + title, lets the
//! user search across them, and activates (focuses) the chosen one. Designed
//! to be composed into a [`UnionSource`][crate::sources::union::UnionSource]
//! alongside apps/clipboard/files so "find what to launch or switch to" is
//! one search box.
//!
//! # Data flow
//!
//! * `new(client)` starts a background thread that drains `client.events` and
//!   mutates the shared `items` vector. Each event flushes a pulse so the
//!   launcher re-renders.
//! * `activate(ix)` invokes a boxed callback; in production this looks up the
//!   toplevel handle and calls `activate()` on it. Tests inject a log-sender.
//!
//! # Why the activation callback is pluggable
//!
//! `ToplevelHandle` is Wayland-owned and not trivially fakeable. Injecting the
//! activation closure keeps `WindowsSource` unit-testable without standing up
//! a compositor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use gpui::{div, prelude::*, AnyElement, FontWeight, Image, ImageFormat, ObjectFit};

use crate::source::{ActivateOutcome, Layout, Preview, PreviewChrome, PreviewPill, Source};
use crate::sources::icon as shared_icon;
use crate::theme;
use crate::usage::UsageTracker;

/// Shared with `UsageTracker` rows — same reasoning as
/// [`crate::sources::apps::SOURCE_NAME`].
pub const SOURCE_NAME: &str = "windows";

/// Resolved icon for a window row. `Arc<Image>` so multiple rows referencing
/// the same app-id can share bytes.
type WindowIcon = Arc<Image>;

/// Pluggable `app_id → icon` lookup. Boxed rather than generic so
/// `WindowsSource` stays a concrete type that can live inside the launcher's
/// trait-object registry. The callback runs on the Wayland-events thread, so
/// `Send + Sync` is mandatory.
pub type IconResolver = Arc<dyn Fn(&str) -> Option<WindowIcon> + Send + Sync>;

/// One open window, as surfaced to the launcher.
#[derive(Clone)]
pub struct WindowRow {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub activated: bool,
    /// Preloaded icon bytes. `None` when app-id has no matching icon-theme
    /// entry or icon loading failed — we still show the row, just without art.
    pub icon_data: Option<WindowIcon>,
    /// Pre-lowercased `app_id` / `title` so scoring doesn't allocate on every
    /// keystroke.
    pub app_id_lc: String,
    pub title_lc: String,
    /// Lowercase `app_id + " " + title`. Pre-computed so `filter` is a cheap
    /// substring scan instead of repeatedly lowercasing on every keystroke.
    pub search_key: String,
    /// Rendered label (`"app_id — title"`). Pre-computed so `render_item`
    /// doesn't allocate per frame.
    pub display_label: String,
}

impl WindowRow {
    fn build(id: u64, app_id: String, title: String, activated: bool) -> Self {
        let app_id_lc = app_id.to_lowercase();
        let title_lc = title.to_lowercase();
        let search_key = format!("{app_id_lc} {title_lc}");
        // Icon already conveys the app; prefer the title (the user-distinguishing
        // bit). Fall back to app_id only when the window has no title yet.
        let display_label = if title.is_empty() {
            app_id.clone()
        } else {
            title.clone()
        };
        Self {
            id,
            app_id,
            title,
            activated,
            icon_data: None,
            app_id_lc,
            title_lc,
            search_key,
            display_label,
        }
    }
}

/// Boxed activation callback. `Fn(id)` rather than `FnOnce` because the
/// launcher may activate multiple times before quitting (Refresh outcomes).
type ActivateFn = Box<dyn Fn(u64) + Send + Sync>;

/// Memoised `app_id → Option<icon>` results. We cache `None` too: icon-theme
/// lookups touch the filesystem, and a missing icon would otherwise be looked
/// up again for every new window of the same app.
type IconMemo = Arc<Mutex<HashMap<String, Option<WindowIcon>>>>;

/// `(app_id, title) -> PNG-encoded thumbnail` shared by the capture worker
/// and the launcher's preview callback. Empty on non-sway / non-wlroots
/// compositors and during the brief window before the worker finishes; both
/// cases degrade to the textual fallback.
pub type ThumbnailMap = Arc<RwLock<HashMap<(String, String), Arc<Image>>>>;

pub struct WindowsSource {
    /// Shared with the event-consumer thread: reader-heavy (one writer thread,
    /// many reads per keystroke). `RwLock` gives lock-free reads via read()
    /// when there's no pending update.
    items: Arc<RwLock<Vec<WindowRow>>>,
    activate_cb: ActivateFn,
    /// Taken by the launcher once via [`Source::take_pulse`].
    pulse_rx: Option<async_channel::Receiver<()>>,
    /// Optional: when Some, `weight()` adds the per-app frecency bonus.
    /// Optional rather than required so existing tests (`from_snapshot`) can
    /// construct a source without wiring a DB.
    tracker: Option<Arc<UsageTracker>>,
    /// Per-window thumbnails captured at construction time. Lookup is by
    /// `(app_id, title)` — sway exposes those uniquely enough that two windows
    /// of the same app rarely share both.
    thumbs: ThumbnailMap,
    /// Full-resolution PNG-encoded captures, same keys as `thumbs`. Drives
    /// peek mode (1:1 overlay) and Ctrl+C image copy. Separate from `thumbs`
    /// so the preview pane keeps its aliasing-free pre-shrunk render.
    full_thumbs: ThumbnailMap,
    /// Id of the most recently-activated window. Updated on every event
    /// that reports `activated=true`, never cleared. Zofi itself steals
    /// focus on launch so `row.activated` becomes false everywhere;
    /// this sticky pointer lets the preview pane still mark the window
    /// the user would return to by pressing esc. Sentinel `0` = none.
    last_active: Arc<AtomicU64>,
    /// Pre-zofi focused window's `(app_id, title)` from sway IPC. Captured
    /// at construction time (before zofi grabs focus) because the
    /// wlr-foreign-toplevel `State` event for the previously-focused window
    /// often arrives only as `activated=false` — by the time we bind to
    /// the protocol, focus has already moved to zofi. Used as a fallback
    /// when `last_active` never gets a true reading.
    pre_focus: Option<(String, String)>,
}

impl WindowsSource {
    /// Production entry: wire to a live `zwindows::Client`. Spawns a thread
    /// that consumes events and coalesces them into a single pulse per batch.
    ///
    /// Kept for test/bootstrap parity even though `main.rs` prefers
    /// [`Self::with_resolver`] so it can seed a composite resolver that reuses
    /// `AppsSource`'s in-memory icons.
    #[allow(dead_code)]
    pub fn new(client: zwindows::Client) -> Self {
        // Build the icon-theme cache once per source instance — it scans the
        // filesystem and should not be repeated per window event.
        let cache = Arc::new(icon_theme::IconCache::new(&["apps"]));
        let resolver: IconResolver = {
            let cache = cache.clone();
            Arc::new(move |name: &str| shared_icon::resolve_by_name(&cache, name))
        };
        Self::with_resolver(client, resolver)
    }

    /// Same as [`new`] but with an injected icon resolver. Public for the
    /// launcher wiring (e.g. UnionSource sharing one `IconCache` across
    /// sources) and reused by tests.
    pub fn with_resolver(client: zwindows::Client, resolver: IconResolver) -> Self {
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        let (pulse_tx, pulse_rx) = async_channel::bounded::<()>(1);
        let memo: IconMemo = Arc::new(Mutex::new(HashMap::new()));

        let last_active: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let items_for_thread = items.clone();
        let events = client.events.clone();
        let resolver_for_thread = resolver.clone();
        let memo_for_thread = memo.clone();
        let last_active_for_thread = last_active.clone();
        std::thread::Builder::new()
            .name("zofi-windows".into())
            .spawn(move || {
                // `recv_blocking` returns Err only when the sender is dropped
                // (zwindows thread exited) — exiting this thread is correct.
                while let Ok(ev) = events.recv_blocking() {
                    apply_event(
                        &items_for_thread,
                        ev,
                        resolver_for_thread.as_ref(),
                        &memo_for_thread,
                        &last_active_for_thread,
                    );
                    // `try_send` coalesces: if the receiver hasn't drained
                    // yet, drop the extra tick. One pulse per frame is enough.
                    let _ = pulse_tx.try_send(());
                }
            })
            .expect("spawn zofi-windows thread");

        // Capture handles by cloning the Arc: activation runs on the launcher
        // thread, not the Wayland thread, so we can't touch `&client` here.
        let handles = client.handles.clone();
        let activate_cb: ActivateFn = Box::new(move |id| {
            if let Some(handle) = handles.read().unwrap().get(&id) {
                handle.activate();
            } else {
                tracing::warn!("activate: no handle for window id {id}");
            }
        });

        // Capture thumbnails on a background thread so launcher startup is
        // not blocked. The ext per-toplevel capture path asks the compositor
        // to render each window individually, so it does not screenshot the
        // visible desktop and cannot "capture itself".
        let thumbs: ThumbnailMap = Arc::new(RwLock::new(HashMap::new()));
        let full_thumbs: ThumbnailMap = Arc::new(RwLock::new(HashMap::new()));
        let thumbs_for_thread = thumbs.clone();
        let full_for_thread = full_thumbs.clone();
        std::thread::Builder::new()
            .name("zofi-windows-capture".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let raw = zwindows::Client::capture_windows(Duration::from_secs(5));
                let raw_count = raw.len();
                let mut encoded: HashMap<(String, String), Arc<Image>> = HashMap::new();
                let mut full_encoded: HashMap<(String, String), Arc<Image>> = HashMap::new();
                for (key, buf) in raw {
                    // Full-res first: peek / copy want the original pixels.
                    // PNG-encoded (~100–500 KB/window) so memory stays bounded.
                    if let Some(png) = buf.to_png() {
                        full_encoded.insert(
                            key.clone(),
                            Arc::new(Image {
                                format: ImageFormat::Png,
                                bytes: png,
                                id: shared_icon::next_image_id(),
                            }),
                        );
                    }
                    // Pre-shrink to the preview pane's inner box before PNG
                    // encoding. GPUI's wgpu sampler is plain bilinear with no
                    // mipmaps; letting the shader downscale a 2560-wide
                    // source to a 680-wide pane aliases text heavily. Pre-
                    // rendering at exactly the display dims (accounting for
                    // aspect via area averaging) makes the shader resample at
                    // effectively 1:1 — crisp glyphs instead of mush.
                    let shrunk = buf.downscale_to_box(
                        u32::from(theme::PREVIEW_IMG_MAX_W),
                        u32::from(theme::PREVIEW_IMG_MAX_H),
                    );
                    if let Some(png) = shrunk.to_png() {
                        encoded.insert(
                            key,
                            Arc::new(Image {
                                format: ImageFormat::Png,
                                bytes: png,
                                id: shared_icon::next_image_id(),
                            }),
                        );
                    }
                }
                let count = encoded.len();
                let keys: Vec<String> = encoded
                    .keys()
                    .map(|(a, t)| format!("({a:?}, {t:?})"))
                    .collect();
                *thumbs_for_thread.write().unwrap() = encoded;
                *full_for_thread.write().unwrap() = full_encoded;
                tracing::info!(
                    "windows: captured {count}/{raw_count} thumbnails in {:?}; keys={keys:?}",
                    t0.elapsed()
                );
            })
            .expect("spawn zofi-windows-capture thread");

        // Best-effort: snapshot the focused window via the compositor's IPC
        // BEFORE the launcher takes input focus. The trait dispatches to
        // sway / Hyprland / noop based on env detection; failures are
        // silent — unsupported compositors just lose the "active" pill.
        let ipc = zwindows::compositor::detect();
        let pre_focus = ipc.focused_window();
        if let Some((ref app, ref title)) = pre_focus {
            tracing::info!("pre-zofi focus: app_id={app:?} title={title:?}");
        }

        Self {
            items,
            activate_cb,
            pulse_rx: Some(pulse_rx),
            tracker: None,
            thumbs,
            full_thumbs,
            last_active,
            pre_focus,
        }
    }

    /// Attach a usage tracker so `weight` includes a per-app frecency bonus
    /// and `item_key` opts into MRU tracking. Consumed builder-style so
    /// production wiring stays a one-liner in main.rs.
    pub fn with_tracker(mut self, tracker: Arc<UsageTracker>) -> Self {
        self.tracker = Some(tracker);
        self
    }

    /// Test entry. Bypasses Wayland; logs every activation id to a channel so
    /// tests can assert routing.
    #[cfg(test)]
    pub(crate) fn from_snapshot(
        rows: Vec<WindowRow>,
        activate_log: async_channel::Sender<u64>,
    ) -> Self {
        let (_pulse_tx, pulse_rx) = async_channel::bounded::<()>(1);
        Self {
            items: Arc::new(RwLock::new(rows)),
            activate_cb: Box::new(move |id| {
                let _ = activate_log.try_send(id);
            }),
            pulse_rx: Some(pulse_rx),
            tracker: None,
            thumbs: Arc::new(RwLock::new(HashMap::new())),
            full_thumbs: Arc::new(RwLock::new(HashMap::new())),
            last_active: Arc::new(AtomicU64::new(0)),
            pre_focus: None,
        }
    }

    /// Test-only escape hatch to seed thumbnails without running screencopy.
    #[cfg(test)]
    pub(crate) fn with_thumbnails(self, thumbs: HashMap<(String, String), Arc<Image>>) -> Self {
        *self.thumbs.write().unwrap() = thumbs;
        self
    }

    /// Test-only escape hatch for full-resolution thumbnails (peek / copy).
    #[cfg(test)]
    pub(crate) fn with_full_thumbnails(self, full: HashMap<(String, String), Arc<Image>>) -> Self {
        *self.full_thumbs.write().unwrap() = full;
        self
    }
}

/// Merge one event into the shared items vec. Kept free-standing so the
/// thread closure stays tiny and the invariants (update-by-id, append-new,
/// remove-in-place, icon-memoisation) are testable in isolation.
///
/// `resolver` is invoked only on cache miss. Results (including `None`) are
/// memoised in `memo` so we never double-scan the filesystem for the same
/// app-id — this matters when a busy app repeatedly emits Updated events.
fn apply_event(
    items: &Arc<RwLock<Vec<WindowRow>>>,
    ev: zwindows::ToplevelEvent,
    resolver: &(dyn Fn(&str) -> Option<WindowIcon> + Send + Sync),
    memo: &IconMemo,
    last_active: &Arc<AtomicU64>,
) {
    use zwindows::ToplevelEvent::*;
    let mut guard = items.write().unwrap();
    match ev {
        Added(tl) | Updated(tl) => {
            if tl.activated {
                last_active.store(tl.id, Ordering::Relaxed);
            }
            let app_id = tl.app_id.unwrap_or_default();
            let title = tl.title.unwrap_or_default();
            let icon = resolve_icon_memoised(&app_id, resolver, memo);
            let mut row = WindowRow::build(tl.id, app_id, title, tl.activated);
            row.icon_data = icon;
            if let Some(existing) = guard.iter_mut().find(|r| r.id == row.id) {
                row_update_in_place(existing, row);
            } else {
                guard.push(row);
            }
        }
        Removed(id) => {
            guard.retain(|r| r.id != id);
            // Clear sticky pointer if the activated window just closed.
            let _ = last_active.compare_exchange(
                id,
                0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }
}

/// Look up an icon for `app_id`, consulting the memoisation cache first. Empty
/// app-ids (zwindows delivers `None → ""`) are never looked up — avoids
/// polluting the cache with a "" entry and sparing the resolver a no-op call.
fn resolve_icon_memoised(
    app_id: &str,
    resolver: &(dyn Fn(&str) -> Option<WindowIcon> + Send + Sync),
    memo: &IconMemo,
) -> Option<WindowIcon> {
    if app_id.is_empty() {
        return None;
    }
    let mut cache = memo.lock().unwrap();
    if let Some(entry) = cache.get(app_id) {
        return entry.clone();
    }
    let resolved = resolver(app_id);
    cache.insert(app_id.to_string(), resolved.clone());
    resolved
}

fn row_update_in_place(dst: &mut WindowRow, src: WindowRow) {
    // Prefer the freshly-resolved icon (from the memo) but keep the previous
    // one if the new lookup somehow returned None — avoids flicker on Updated
    // events where the icon theme cache was empty at Added time.
    let fallback = dst.icon_data.take();
    *dst = WindowRow {
        icon_data: src.icon_data.or(fallback),
        ..src
    };
}

impl Source for WindowsSource {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    fn icon(&self) -> &'static str {
        "◱"
    }

    fn prefix(&self) -> Option<char> {
        Some('@')
    }

    fn placeholder(&self) -> &'static str {
        "Search windows..."
    }

    fn empty_text(&self) -> &'static str {
        "No windows"
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        let items = self.items.read().unwrap();
        if query.is_empty() {
            return (0..items.len()).collect();
        }
        let q = query.to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, r)| r.search_key.contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn filter_scored(&self, query: &str) -> Vec<(usize, i32)> {
        let items = self.items.read().unwrap();
        if query.is_empty() {
            return (0..items.len()).map(|i| (i, 0)).collect();
        }
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for (i, row) in items.iter().enumerate() {
            // Scoring priority: app-id prefix > title prefix > app-id
            // substring > title substring. Higher score = better match.
            let score = if row.app_id_lc.starts_with(&q) {
                30
            } else if row.title_lc.starts_with(&q) {
                20
            } else if row.app_id_lc.contains(&q) {
                15
            } else if row.title_lc.contains(&q) {
                10
            } else {
                continue;
            };
            out.push((i, score));
        }
        out
    }

    fn weight(&self, ix: usize) -> i32 {
        let items = self.items.read().unwrap();
        // Baseline 100 so windows rank alongside apps/files (which use 0 by
        // default). Activated bonus nudges the focused window to the top. On
        // top of that, per-app frecency records "this app is used a lot" —
        // multiple windows of the same app share that bonus because they
        // share app_id. Bonus is 0 when no tracker is attached (tests).
        items
            .get(ix)
            .map(|r| {
                let activated = if r.activated { 10 } else { 0 };
                let frecency = self
                    .tracker
                    .as_ref()
                    .map(|t| t.frecency_bonus(SOURCE_NAME, &r.app_id))
                    .unwrap_or(0);
                100 + activated + frecency
            })
            .unwrap_or(0)
    }

    fn item_key(&self, ix: usize) -> Option<String> {
        let items = self.items.read().unwrap();
        let row = items.get(ix)?;
        if row.app_id.is_empty() {
            return None;
        }
        Some(row.app_id.clone())
    }

    fn render_item(&self, ix: usize, selected: bool) -> AnyElement {
        let items = self.items.read().unwrap();
        let Some(row) = items.get(ix) else {
            return div().into_any_element();
        };
        let icon: AnyElement = match &row.icon_data {
            Some(data) => div()
                .size(theme::ICON_SIZE)
                .flex_shrink_0()
                .child(
                    gpui::img(data.clone())
                        .size(theme::ICON_SIZE)
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            // When icon-theme lookup misses, fall back to a macOS-ish initial
            // tile — rounded rect in a muted tone with the first letter of the
            // app_id. Looks intentional next to the real app icons rather than
            // an abandoned placeholder glyph.
            None => {
                let initial = row
                    .app_id
                    .chars()
                    .find(|c| !c.is_whitespace())
                    .map(|c| c.to_uppercase().next().unwrap_or(c).to_string())
                    .unwrap_or_else(|| "?".into());
                div()
                    .size(theme::ICON_SIZE)
                    .flex_shrink_0()
                    .rounded(gpui::px(5.0))
                    .bg(theme::hover_bg())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme::fg_dim())
                    .text_size(theme::FONT_SIZE_SM)
                    .font_weight(FontWeight::MEDIUM)
                    .child(initial)
                    .into_any_element()
            }
        };

        // Two-line label: title (or app_id fallback) on top, dim app_id
        // subtitle underneath so the row carries WM-class context without
        // the user needing to open the preview pane.
        let subtitle = row.app_id.clone();

        div()
            .h_full()
            .px(theme::PAD_X)
            .flex()
            .items_center()
            .gap(theme::GAP)
            .child(icon)
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
                            .text_color(if selected {
                                gpui::white()
                            } else {
                                theme::fg()
                            })
                            .child(row.display_label.clone()),
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
        let id = self.items.read().unwrap().get(ix).map(|r| r.id);
        if let Some(id) = id {
            (self.activate_cb)(id);
        }
        ActivateOutcome::Quit
    }

    fn take_pulse(&mut self) -> Option<async_channel::Receiver<()>> {
        self.pulse_rx.take()
    }

    fn layout(&self) -> Layout {
        Layout::ListAndPreview
    }

    fn peek_image(&self, ix: usize) -> Option<Arc<Image>> {
        let items = self.items.read().unwrap();
        let row = items.get(ix)?;
        let key = (row.app_id.clone(), row.title.clone());
        self.full_thumbs.read().unwrap().get(&key).cloned()
    }

    fn can_peek(&self) -> bool {
        true
    }

    fn copy_image_bytes(&self, ix: usize) -> Option<Arc<Vec<u8>>> {
        let items = self.items.read().unwrap();
        let row = items.get(ix)?;
        let key = (row.app_id.clone(), row.title.clone());
        // Reuse the already-encoded PNG bytes rather than re-encoding; the
        // capture thread pays the PNG cost once per window.
        let img = self.full_thumbs.read().unwrap().get(&key).cloned()?;
        Some(Arc::new(img.bytes.clone()))
    }

    fn can_copy_image(&self) -> bool {
        true
    }

    fn preview_chrome(&self, ix: usize) -> Option<PreviewChrome> {
        let items = self.items.read().unwrap();
        let row = items.get(ix)?;
        // "active" pill: prefer the wlr-foreign-toplevel sticky pointer
        // (works on any wlroots compositor), fall back to the sway IPC
        // pre-zofi snapshot (works whenever sway is the compositor —
        // covers the case where the toplevel state event for the
        // previously-focused window only ever reports activated=false).
        let is_active = self.last_active.load(Ordering::Relaxed) == row.id
            || self
                .pre_focus
                .as_ref()
                .is_some_and(|(app, title)| app == &row.app_id && title == &row.title);
        let mut pills = Vec::new();
        if is_active {
            pills.push(PreviewPill {
                text: "active".into(),
                active: true,
            });
        }
        // app_id is what wlr-foreign-toplevel reports as the Wayland app id
        // (for native clients) or the X11 WM_CLASS (for XWayland). Two-row
        // layout matches the mockup's information density without faking
        // a PID lookup we can't actually do per-row.
        let metadata = vec![
            ("App".into(), row.app_id.clone()),
            ("ID".into(), format!("0x{:x}", row.id)),
        ];
        Some(PreviewChrome {
            title: row.display_label.clone(),
            pills,
            metadata,
        })
    }

    fn preview(&self, ix: usize) -> Option<Preview> {
        let items = self.items.read().unwrap();
        let row = items.get(ix)?;
        // Prefer the live thumbnail if we managed to capture one. Lookup by
        // (app_id, title) — same key the capture worker uses. Falling back to
        // text means the launcher still surfaces *something* useful even when
        // screencopy isn't available (non-wlroots compositors, capture
        // timeouts, or windows that opened after the snapshot).
        let key = (row.app_id.clone(), row.title.clone());
        let hit = self.thumbs.read().unwrap().get(&key).cloned();
        if let Some(img) = hit {
            return Some(Preview::Image(img));
        }
        tracing::info!("preview: thumbnail miss for {key:?}");
        let mut out = String::new();
        if !row.title.is_empty() {
            out.push_str(&row.title);
            out.push_str("\n\n");
        }
        out.push_str("App ID\n");
        out.push_str(&row.app_id);
        out.push('\n');
        out.push_str("\nState\n");
        out.push_str(if row.activated {
            "active"
        } else {
            "background"
        });
        out.push('\n');
        out.push_str("\nWindow ID\n");
        out.push_str(&row.id.to_string());
        out.push('\n');
        Some(Preview::Text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, app: &str, title: &str, activated: bool) -> WindowRow {
        WindowRow::build(id, app.to_string(), title.to_string(), activated)
    }

    fn fixture() -> (WindowsSource, async_channel::Receiver<u64>) {
        let (tx, rx) = async_channel::bounded::<u64>(16);
        let rows = vec![
            row(1, "firefox", "Issues · zskins", false),
            row(2, "kitty", "~/src/zskins", true),
            row(3, "Code", "main.rs — zofi", false),
        ];
        (WindowsSource::from_snapshot(rows, tx), rx)
    }

    #[test]
    fn name_icon_placeholder_and_empty_text() {
        let (src, _) = fixture();
        assert_eq!(src.name(), "windows");
        assert_eq!(src.icon(), "◱");
        assert_eq!(src.placeholder(), "Search windows...");
        assert_eq!(src.empty_text(), "No windows");
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let (src, _) = fixture();
        assert_eq!(src.filter(""), vec![0, 1, 2]);
    }

    #[test]
    fn filter_substring_matches_app_id() {
        let (src, _) = fixture();
        assert_eq!(src.filter("fire"), vec![0]);
        // Case-insensitive: upper-case query matches lower-case app_id.
        assert_eq!(src.filter("FIRE"), vec![0]);
    }

    #[test]
    fn filter_substring_matches_title_across_rows() {
        let (src, _) = fixture();
        // "zskins" appears in row 0's title ("Issues · zskins") and row 1's
        // title ("~/src/zskins") and row 2's title ("main.rs — zofi") — no,
        // row 2 doesn't match. So: rows 0 and 1.
        assert_eq!(src.filter("zskins"), vec![0, 1]);
    }

    #[test]
    fn filter_case_insensitive_on_mixed_case_app_id() {
        // Row 3 has app_id "Code" (mixed case); a lowercase query must match.
        let (src, _) = fixture();
        assert_eq!(src.filter("code"), vec![2]);
    }

    #[test]
    fn filter_scored_prefix_beats_substring() {
        let (src, _) = fixture();
        // "fire" is a prefix of "firefox" (score 30); "ire" is only a
        // substring of "firefox" (score 15).
        let prefix = src.filter_scored("fire");
        let substring = src.filter_scored("ire");
        assert_eq!(prefix, vec![(0, 30)]);
        assert_eq!(substring, vec![(0, 15)]);
    }

    #[test]
    fn filter_scored_title_prefix_beats_title_substring() {
        // "Issues" is a title prefix on row 0 (score 20); "ues" is only a
        // substring of that title (score 10).
        let (src, _) = fixture();
        let prefix = src.filter_scored("Issues");
        assert_eq!(prefix, vec![(0, 20)]);
        let substr = src.filter_scored("ues");
        assert_eq!(substr, vec![(0, 10)]);
    }

    #[test]
    fn filter_scored_empty_query_gives_zero_scores() {
        let (src, _) = fixture();
        assert_eq!(src.filter_scored(""), vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn weight_base_is_100_for_inactive_window() {
        let (src, _) = fixture();
        // Row 0 (firefox) is not activated.
        assert_eq!(src.weight(0), 100);
        assert_eq!(src.weight(2), 100);
    }

    #[test]
    fn weight_includes_activated_bonus() {
        let (src, _) = fixture();
        // Row 1 (kitty) has activated=true in the fixture.
        assert_eq!(src.weight(1), 110);
    }

    #[test]
    fn weight_folds_tracker_frecency_bonus() {
        // Base weight (100) + frecency bonus for the app_id. Seed the tracker
        // with multiple activations for `firefox` and confirm it raises the
        // row's weight above the unseeded rows.
        let (src, _) = fixture();
        let base = src.weight(0);
        let tracker = Arc::new(UsageTracker::open_in_memory().unwrap());
        for _ in 0..10 {
            tracker.record(SOURCE_NAME, "firefox");
        }
        let src = src.with_tracker(tracker);
        assert!(
            src.weight(0) > base,
            "firefox weight with tracker should exceed baseline {base}"
        );
        // Unseeded rows keep the baseline.
        assert_eq!(src.weight(2), base);
    }

    #[test]
    fn item_key_returns_app_id() {
        let (src, _) = fixture();
        assert_eq!(src.item_key(0).as_deref(), Some("firefox"));
        assert_eq!(src.item_key(2).as_deref(), Some("Code"));
    }

    #[test]
    fn item_key_none_for_empty_app_id() {
        // Wayland sometimes delivers a blank app_id — such rows opt out of
        // MRU tracking.
        let (tx, _rx) = async_channel::bounded::<u64>(1);
        let rows = vec![row(1, "", "no app", false)];
        let src = WindowsSource::from_snapshot(rows, tx);
        assert_eq!(src.item_key(0), None);
    }

    #[test]
    fn activate_invokes_callback_with_correct_id() {
        let (src, rx) = fixture();
        // virtual index 1 corresponds to id 2 (kitty) in our snapshot.
        let outcome = src.activate(1);
        assert!(matches!(outcome, ActivateOutcome::Quit));
        let id = rx.try_recv().expect("expected activate log entry");
        assert_eq!(id, 2);
    }

    #[test]
    fn activate_out_of_range_does_not_panic_or_log() {
        let (src, rx) = fixture();
        let outcome = src.activate(99);
        // Still Quit — matches behavior of empty filter results.
        assert!(matches!(outcome, ActivateOutcome::Quit));
        assert!(rx.try_recv().is_err(), "no id should have been logged");
    }

    #[test]
    fn take_pulse_returns_some_first_then_none() {
        let (mut src, _) = fixture();
        assert!(src.take_pulse().is_some());
        assert!(src.take_pulse().is_none());
    }

    #[test]
    fn window_row_build_assembles_search_key_lowercased() {
        let r = row(42, "Firefox", "My Title", false);
        assert_eq!(r.search_key, "firefox my title");
        // The icon already conveys the app — label shows just the title so
        // multiple foot/firefox windows stay visually distinguishable.
        assert_eq!(r.display_label, "My Title");
    }

    #[test]
    fn window_row_build_handles_empty_title() {
        let r = row(1, "kitty", "", false);
        // When there's no title, just show the app-id — avoids "kitty — "
        // trailing an em-dash.
        assert_eq!(r.display_label, "kitty");
    }

    // Convenience helpers keep the noisy apply_event() signature out of tests.

    fn null_memo() -> IconMemo {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn null_last_active() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    fn null_resolver_fn() -> impl Fn(&str) -> Option<WindowIcon> + Send + Sync {
        |_: &str| None
    }

    fn toplevel(id: u64, app_id: &str, title: &str) -> zwindows::Toplevel {
        zwindows::Toplevel {
            id,
            app_id: Some(app_id.into()),
            title: Some(title.into()),
            activated: false,
            minimized: false,
        }
    }

    /// Build a pretend `Arc<Image>` for icon tests. We never render it; we
    /// only check identity via `Arc::ptr_eq` / `icon_data.is_some()`, so the
    /// byte payload and format are irrelevant.
    fn stub_icon() -> WindowIcon {
        Arc::new(gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes: Vec::new(),
            id: 0,
        })
    }

    #[test]
    fn apply_event_added_appends_new_row() {
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "firefox", "t")),
            &null_resolver_fn(),
            &null_memo(),
            &null_last_active(),
        );
        let guard = items.read().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].id, 1);
    }

    #[test]
    fn apply_event_updated_replaces_same_id_row() {
        let items: Arc<RwLock<Vec<WindowRow>>> =
            Arc::new(RwLock::new(vec![row(1, "firefox", "old", false)]));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Updated(zwindows::Toplevel {
                id: 1,
                app_id: Some("firefox".into()),
                title: Some("new".into()),
                activated: true,
                minimized: false,
            }),
            &null_resolver_fn(),
            &null_memo(),
            &null_last_active(),
        );
        let guard = items.read().unwrap();
        assert_eq!(guard.len(), 1, "update must not duplicate");
        assert_eq!(guard[0].title, "new");
        assert!(guard[0].activated);
    }

    #[test]
    fn apply_event_removed_drops_the_row() {
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(vec![
            row(1, "a", "", false),
            row(2, "b", "", false),
        ]));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Removed(1),
            &null_resolver_fn(),
            &null_memo(),
            &null_last_active(),
        );
        let guard = items.read().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].id, 2);
    }

    // ------------------------------------------------------------------
    // Icon resolver tests (TDD)
    // ------------------------------------------------------------------

    #[test]
    fn icon_resolver_called_with_app_id() {
        // The resolver must see the window's app-id — not the title, not a
        // lowercased variant — because icon-theme names are case-sensitive.
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let resolver = move |name: &str| -> Option<WindowIcon> {
            calls_clone.lock().unwrap().push(name.to_string());
            None
        };
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "Firefox", "t")),
            &resolver,
            &null_memo(),
            &null_last_active(),
        );
        assert_eq!(calls.lock().unwrap().as_slice(), &["Firefox".to_string()]);
    }

    #[test]
    fn icon_data_populated_from_resolver() {
        let icon = stub_icon();
        let icon_for_resolver = icon.clone();
        let resolver = move |_: &str| Some(icon_for_resolver.clone());
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "firefox", "t")),
            &resolver,
            &null_memo(),
            &null_last_active(),
        );
        let guard = items.read().unwrap();
        let got = guard[0]
            .icon_data
            .as_ref()
            .expect("icon_data should be Some");
        assert!(
            Arc::ptr_eq(got, &icon),
            "row must carry the Arc returned by the resolver"
        );
    }

    #[test]
    fn icon_data_none_when_resolver_returns_none() {
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "nonexistent-app", "t")),
            &null_resolver_fn(),
            &null_memo(),
            &null_last_active(),
        );
        assert!(items.read().unwrap()[0].icon_data.is_none());
    }

    #[test]
    fn icon_cache_hits_on_repeat_app_id() {
        // Two Added events for the same app-id must hit the resolver exactly
        // once — otherwise a busy app spamming events would thrash the FS.
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let count_clone = count.clone();
        let icon = stub_icon();
        let icon_for_resolver = icon.clone();
        let resolver = move |_: &str| {
            *count_clone.lock().unwrap() += 1;
            Some(icon_for_resolver.clone())
        };
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        let memo = null_memo();
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "firefox", "a")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(2, "firefox", "b")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        assert_eq!(*count.lock().unwrap(), 1);
    }

    #[test]
    fn icon_cache_misses_differ_by_app_id() {
        // Distinct app-ids must each get one resolver call — the cache key is
        // the app-id, not "any lookup".
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let resolver = move |name: &str| -> Option<WindowIcon> {
            seen_clone.lock().unwrap().push(name.to_string());
            None
        };
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        let memo = null_memo();
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "firefox", "a")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(2, "kitty", "b")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        let got = seen.lock().unwrap().clone();
        assert_eq!(got, vec!["firefox".to_string(), "kitty".to_string()]);
    }

    #[test]
    fn icon_cache_memoises_none_results() {
        // None results must also be cached — otherwise apps without an icon
        // theme entry would get looked up on every Update event.
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let count_clone = count.clone();
        let resolver = move |_: &str| -> Option<WindowIcon> {
            *count_clone.lock().unwrap() += 1;
            None
        };
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        let memo = null_memo();
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "ghost", "a")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        apply_event(
            &items,
            zwindows::ToplevelEvent::Updated(toplevel(1, "ghost", "b")),
            &resolver,
            &memo,
            &null_last_active(),
        );
        assert_eq!(*count.lock().unwrap(), 1);
    }

    // ------------------------------------------------------------------
    // Preview tests
    // ------------------------------------------------------------------

    #[test]
    fn preview_returns_text_when_no_thumbnail_present() {
        let (src, _) = fixture();
        match src.preview(0) {
            Some(Preview::Text(t)) => {
                assert!(t.contains("firefox"), "text preview should mention app_id");
            }
            _ => panic!("expected textual fallback"),
        }
    }

    #[test]
    fn preview_returns_image_when_thumbnail_seeded() {
        let (src, _) = fixture();
        let img = stub_icon();
        let mut thumbs = HashMap::new();
        // Row 0 has app_id "firefox", title "Issues · zskins" — must match
        // exactly so the lookup hits.
        thumbs.insert(
            ("firefox".to_string(), "Issues · zskins".to_string()),
            img.clone(),
        );
        let src = src.with_thumbnails(thumbs);
        match src.preview(0) {
            Some(Preview::Image(got)) => assert!(Arc::ptr_eq(&got, &img)),
            _ => panic!("expected image preview"),
        }
    }

    #[test]
    fn preview_falls_back_to_text_when_thumbnail_key_misses() {
        // A thumbnail map that exists but doesn't contain the row's key must
        // not break preview — we degrade to the text view.
        let (src, _) = fixture();
        let mut thumbs = HashMap::new();
        thumbs.insert(
            ("other-app".to_string(), "other-title".to_string()),
            stub_icon(),
        );
        let src = src.with_thumbnails(thumbs);
        assert!(matches!(src.preview(0), Some(Preview::Text(_))));
    }

    // ------------------------------------------------------------------
    // Peek image / clipboard tests
    // ------------------------------------------------------------------

    fn full_stub_image(bytes: Vec<u8>) -> Arc<Image> {
        Arc::new(gpui::Image {
            format: gpui::ImageFormat::Png,
            bytes,
            id: 42,
        })
    }

    #[test]
    fn can_peek_and_can_copy_image_are_true() {
        let (src, _) = fixture();
        assert!(src.can_peek());
        assert!(src.can_copy_image());
    }

    #[test]
    fn peek_image_none_when_full_thumbs_empty() {
        let (src, _) = fixture();
        assert!(src.peek_image(0).is_none());
    }

    #[test]
    fn peek_image_some_when_full_thumbs_seeded() {
        let (src, _) = fixture();
        let img = full_stub_image(vec![1, 2, 3]);
        let mut full = HashMap::new();
        full.insert(
            ("firefox".to_string(), "Issues · zskins".to_string()),
            img.clone(),
        );
        let src = src.with_full_thumbnails(full);
        let got = src.peek_image(0).expect("row 0 should hit full_thumbs");
        assert!(Arc::ptr_eq(&got, &img));
    }

    #[test]
    fn peek_image_none_for_out_of_range_row() {
        let (src, _) = fixture();
        // Seed to prove `None` comes from bounds, not empty map.
        let mut full = HashMap::new();
        full.insert(
            ("firefox".to_string(), "Issues · zskins".to_string()),
            full_stub_image(vec![1]),
        );
        let src = src.with_full_thumbnails(full);
        assert!(src.peek_image(99).is_none());
    }

    #[test]
    fn copy_image_bytes_returns_png_bytes_when_present() {
        let (src, _) = fixture();
        let png = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut full = HashMap::new();
        full.insert(
            ("firefox".to_string(), "Issues · zskins".to_string()),
            full_stub_image(png.clone()),
        );
        let src = src.with_full_thumbnails(full);
        let got = src
            .copy_image_bytes(0)
            .expect("row 0 should have copyable bytes");
        assert_eq!(&*got, &png);
    }

    #[test]
    fn copy_image_bytes_none_when_full_thumbs_miss() {
        let (src, _) = fixture();
        // Seed only a non-matching key; row 0's key won't match.
        let mut full = HashMap::new();
        full.insert(
            ("other".to_string(), "other".to_string()),
            full_stub_image(vec![0]),
        );
        let src = src.with_full_thumbnails(full);
        assert!(src.copy_image_bytes(0).is_none());
    }

    #[test]
    fn icon_resolver_skipped_for_empty_app_id() {
        // zwindows delivers a missing app_id as "" — resolving that would
        // only pollute the memo with a useless entry.
        let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let count_clone = count.clone();
        let resolver = move |_: &str| -> Option<WindowIcon> {
            *count_clone.lock().unwrap() += 1;
            None
        };
        let items: Arc<RwLock<Vec<WindowRow>>> = Arc::new(RwLock::new(Vec::new()));
        apply_event(
            &items,
            zwindows::ToplevelEvent::Added(toplevel(1, "", "unknown")),
            &resolver,
            &null_memo(),
            &null_last_active(),
        );
        assert_eq!(*count.lock().unwrap(), 0);
    }
}
