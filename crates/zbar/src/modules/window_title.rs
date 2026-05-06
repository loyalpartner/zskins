use crate::backend::sway::run_window_title_session;
use gpui::{div, px, Context, IntoElement, ParentElement, Render, Styled, Window};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

pub struct WindowTitleModule {
    title: Option<String>,
}

enum TitleSource {
    Sway,
    Niri,
    None,
}

fn detect_title_source() -> TitleSource {
    if let Ok(path) = std::env::var("SWAYSOCK") {
        // SWAYSOCK can linger from a previous sway session — confirm the
        // socket actually accepts a connection before committing to it.
        if UnixStream::connect(&path).is_ok() {
            return TitleSource::Sway;
        }
    }
    if Command::new("niri")
        .arg("msg")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return TitleSource::Niri;
    }
    TitleSource::None
}

impl WindowTitleModule {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded::<Option<String>>(16);

        match detect_title_source() {
            TitleSource::None => return WindowTitleModule { title: None },
            TitleSource::Sway => {
                let tx = tx.clone();
                cx.background_executor()
                    .spawn(async move {
                        let mut delay_ms: u64 = 1000;
                        loop {
                            match run_window_title_session(tx.clone()) {
                                Ok(()) => {}
                                Err(e) => tracing::warn!(
                                    "window-title (sway) error: {e:#}; reconnecting in {delay_ms}ms"
                                ),
                            }
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                            delay_ms = (delay_ms * 2).min(30_000);
                        }
                    })
                    .detach();
            }
            TitleSource::Niri => {
                let tx = tx.clone();
                cx.background_executor()
                    .spawn(async move {
                        let mut delay_ms: u64 = 1000;
                        loop {
                            match run_niri_title_session(&tx) {
                                Ok(()) => {}
                                Err(e) => tracing::warn!(
                                    "window-title (niri) error: {e:#}; reconnecting in {delay_ms}ms"
                                ),
                            }
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                            delay_ms = (delay_ms * 2).min(30_000);
                        }
                    })
                    .detach();
            }
        }

        cx.spawn(async move |this, cx| {
            while let Ok(title) = rx.recv().await {
                if this
                    .update(cx, |m, cx| {
                        m.title = title;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        WindowTitleModule { title: None }
    }
}

#[derive(Debug, thiserror::Error)]
enum NiriTitleError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("niri msg exited: {0}")]
    Exit(std::process::ExitStatus),
}

/// Tracks niri window state so we can compute the focused window's title
/// from the event stream. Pure state machine — easy to unit test.
#[derive(Default)]
struct NiriTitleTracker {
    titles: HashMap<u64, String>,
    focused: Option<u64>,
}

impl NiriTitleTracker {
    /// Apply one event-stream message. Returns the focused title *after*
    /// applying, so the caller can dedupe against a previous value.
    fn apply(&mut self, event: &serde_json::Value) -> Option<String> {
        let Some(obj) = event.as_object() else {
            return self.current();
        };
        // niri wraps each event as a single-key object; loop is defensive.
        for (kind, payload) in obj {
            match kind.as_str() {
                "WindowsChanged" => {
                    self.titles.clear();
                    self.focused = None;
                    if let Some(arr) = payload.get("windows").and_then(|v| v.as_array()) {
                        for w in arr {
                            self.upsert_window(w);
                        }
                    }
                }
                "WindowOpenedOrChanged" => {
                    if let Some(w) = payload.get("window") {
                        self.upsert_window(w);
                    }
                }
                "WindowClosed" => {
                    if let Some(id) = payload.get("id").and_then(|v| v.as_u64()) {
                        self.titles.remove(&id);
                        if self.focused == Some(id) {
                            self.focused = None;
                        }
                    }
                }
                "WindowFocusChanged" => {
                    self.focused = payload.get("id").and_then(|v| v.as_u64());
                }
                _ => {}
            }
        }
        self.current()
    }

    fn upsert_window(&mut self, w: &serde_json::Value) {
        let Some(id) = w.get("id").and_then(|v| v.as_u64()) else {
            return;
        };
        if let Some(title) = w.get("title").and_then(|v| v.as_str()) {
            self.titles.insert(id, title.to_string());
        }
        if w.get("is_focused").and_then(|v| v.as_bool()) == Some(true) {
            self.focused = Some(id);
        }
    }

    fn current(&self) -> Option<String> {
        self.focused.and_then(|id| self.titles.get(&id).cloned())
    }
}

/// RAII wrapper that kills the child on drop so reconnect loops don't leak
/// `niri msg` subprocesses if the reader exits unexpectedly.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Subscribe to `niri msg --json event-stream` and emit the focused
/// window's title whenever it changes.
fn run_niri_title_session(
    tx: &async_channel::Sender<Option<String>>,
) -> Result<(), NiriTitleError> {
    let mut child = KillOnDrop(
        Command::new("niri")
            .arg("msg")
            .arg("--json")
            .arg("event-stream")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let stdout =
        child.0.stdout.take().ok_or_else(|| {
            NiriTitleError::Io(std::io::Error::other("niri msg: stdout not piped"))
        })?;
    let reader = BufReader::new(stdout);

    let mut tracker = NiriTitleTracker::default();
    let mut last_emitted: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let current = tracker.apply(&event);
        if current != last_emitted {
            last_emitted = current.clone();
            if tx.send_blocking(current).is_err() {
                break;
            }
        }
    }

    let status = child.0.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(NiriTitleError::Exit(status))
    }
}

#[cfg(test)]
mod tests {
    use super::NiriTitleTracker;
    use serde_json::json;

    #[test]
    fn windows_changed_seeds_state_and_picks_focused() {
        let mut t = NiriTitleTracker::default();
        let title = t.apply(&json!({
            "WindowsChanged": {
                "windows": [
                    {"id": 1, "title": "A", "is_focused": false},
                    {"id": 2, "title": "B", "is_focused": true},
                    {"id": 3, "title": "C", "is_focused": false},
                ]
            }
        }));
        assert_eq!(title.as_deref(), Some("B"));
    }

    #[test]
    fn focus_change_picks_new_window() {
        let mut t = NiriTitleTracker::default();
        t.apply(&json!({
            "WindowsChanged": {
                "windows": [
                    {"id": 1, "title": "A", "is_focused": true},
                    {"id": 2, "title": "B", "is_focused": false},
                ]
            }
        }));
        let title = t.apply(&json!({ "WindowFocusChanged": {"id": 2} }));
        assert_eq!(title.as_deref(), Some("B"));
    }

    #[test]
    fn title_update_replaces_current() {
        let mut t = NiriTitleTracker::default();
        t.apply(&json!({
            "WindowsChanged": {
                "windows": [ {"id": 1, "title": "old", "is_focused": true} ]
            }
        }));
        let title = t.apply(&json!({
            "WindowOpenedOrChanged": {
                "window": {"id": 1, "title": "new", "is_focused": true}
            }
        }));
        assert_eq!(title.as_deref(), Some("new"));
    }

    #[test]
    fn closing_focused_window_clears_title() {
        let mut t = NiriTitleTracker::default();
        t.apply(&json!({
            "WindowsChanged": {
                "windows": [ {"id": 1, "title": "A", "is_focused": true} ]
            }
        }));
        let title = t.apply(&json!({ "WindowClosed": {"id": 1} }));
        assert_eq!(title, None);
    }

    #[test]
    fn windows_changed_resets_focus() {
        // If a WindowsChanged snapshot reports nobody focused, the tracker
        // should drop its previous focused id instead of keeping stale state.
        let mut t = NiriTitleTracker::default();
        t.apply(&json!({
            "WindowsChanged": {
                "windows": [ {"id": 1, "title": "A", "is_focused": true} ]
            }
        }));
        let title = t.apply(&json!({
            "WindowsChanged": {
                "windows": [ {"id": 1, "title": "A", "is_focused": false} ]
            }
        }));
        assert_eq!(title, None);
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let mut t = NiriTitleTracker::default();
        t.apply(&json!({
            "WindowsChanged": {
                "windows": [ {"id": 1, "title": "A", "is_focused": true} ]
            }
        }));
        let title = t.apply(&json!({ "WorkspacesChanged": {"workspaces": []} }));
        assert_eq!(title.as_deref(), Some("A"));
    }
}

impl Render for WindowTitleModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.title.clone().unwrap_or_default();
        let t = cx.global::<ztheme::Theme>();
        div()
            .max_w(px(500.))
            .overflow_x_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_color(t.fg_dim)
            .child(text)
    }
}
