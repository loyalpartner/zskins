//! Filesystem watcher for the theme config.
//!
//! We watch the *parent directory* rather than the file itself. Many editors
//! (vim, emacs, helix, vscode) save by writing a sibling and renaming over
//! the target — when notify watches the file inode directly it usually loses
//! track after the rename. Watching the directory and filtering by basename
//! is the standard workaround.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use notify::event::{EventKind, ModifyKind};
use notify::{recommended_watcher, Event, RecursiveMode, Watcher};

use crate::{config_path, load, Theme, ThemeError};

/// Lifecycle handle returned by [`watch`]. Hold it for as long as you want
/// the watcher to run; dropping it stops the watcher and joins the
/// notification thread.
///
/// Internally it owns the platform-specific [`notify::RecommendedWatcher`]
/// (which stops on drop) and a join handle for our debounce thread, which
/// is told to exit by closing its inbound channel.
pub struct WatcherHandle {
    _watcher: notify::RecommendedWatcher,
    // Optional so `Drop` can `take()` and join without moving out of `&mut self`.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        // Watcher's own Drop fires first via field-order above. Joining the
        // dispatcher thread after that gives it a chance to drain any final
        // event the platform queued before the watcher was torn down.
        if let Some(t) = self.thread.take() {
            // Best-effort — if the thread panicked we still want the
            // handle's drop to complete.
            let _ = t.join();
        }
    }
}

/// Spawn a watcher over the theme config file. `on_change` is called
/// from a dedicated dispatcher thread whenever a write affects the file
/// (create/modify/rename-into-place). Callers are responsible for routing
/// the new theme back onto the GUI thread; this crate does not assume an
/// async runtime.
pub fn watch<F>(on_change: F) -> Result<WatcherHandle, ThemeError>
where
    F: Fn(Theme) + Send + 'static,
{
    let path = config_path();
    let parent = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    // Auto-create the directory so notify::watch doesn't error on a fresh
    // install. Failure here is fine — the watcher will simply never fire
    // until the directory exists.
    let _ = std::fs::create_dir_all(&parent);

    let basename = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("config.toml"));

    let (tx, rx) = channel::<notify::Result<Event>>();
    let mut watcher = recommended_watcher(move |res| {
        // try_send equivalent: if the receiver is gone, drop silently.
        let _ = tx.send(res);
    })?;
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    let thread = std::thread::Builder::new()
        .name("ztheme-watcher".into())
        .spawn(move || {
            // Coalesce bursts of events (atomic save = remove + create) to a
            // single reload. 100ms is short enough to feel instant on the
            // panel but long enough to swallow editor save sequences.
            const DEBOUNCE: Duration = Duration::from_millis(100);
            loop {
                let event = match rx.recv() {
                    Ok(Ok(e)) => e,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "ztheme watcher: notify error");
                        continue;
                    }
                    // Channel closed: the WatcherHandle dropped the watcher
                    // and we exit so the join in WatcherHandle::drop unblocks.
                    Err(_) => return,
                };
                if !event_touches(&event, &basename) {
                    continue;
                }
                if !is_write_kind(&event.kind) {
                    continue;
                }
                // Drain any other events that arrived during the debounce
                // window so we only reload once per save.
                let deadline = std::time::Instant::now() + DEBOUNCE;
                while let Some(remaining) =
                    deadline.checked_duration_since(std::time::Instant::now())
                {
                    match rx.recv_timeout(remaining) {
                        Ok(_) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        // Channel closed mid-debounce: still fire the final
                        // reload so the consumer sees the latest state.
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            on_change(load());
                            return;
                        }
                    }
                }
                on_change(load());
            }
        })
        .map_err(ThemeError::Io)?;

    Ok(WatcherHandle {
        _watcher: watcher,
        thread: Some(thread),
    })
}

fn event_touches(event: &Event, basename: &std::ffi::OsStr) -> bool {
    event.paths.iter().any(|p| p.file_name() == Some(basename))
}

fn is_write_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Other)
            | EventKind::Remove(_)
    )
}
