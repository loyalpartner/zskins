mod assets;
mod clipd;
mod fuzzy;
mod highlight;
mod input;
mod launcher;
mod registry;
mod source;
mod sources;
mod theme;
mod usage;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gpui::{
    layer_shell::*, App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions,
};
use gpui_platform::application;

use crate::registry::{SourceEntry, SourceRegistry};
use crate::source::Source;
use crate::sources::{
    apps::AppsSource, clipboard::ClipboardSource, union::UnionSource, windows::WindowsSource,
};
use crate::usage::UsageTracker;

#[derive(Parser)]
#[command(name = "zofi", about = "rofi-style multi-source launcher", version)]
struct Cli {
    /// Source to open when no subcommand is given.
    source: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the clipboard daemon.
    Clipd,
    /// Bulk-import a clipman.json into the clipboard db.
    Import { path: PathBuf },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Clipd) => return clipd::run(),
        Some(Cmd::Import { path }) => return clipd::import(&path),
        None => {}
    }

    // Build the registry up-front so anything that needs a one-shot resource
    // (Wayland connection, daemon socket, etc.) is acquired before GPUI takes
    // over the main thread.
    let registry = build_registry();

    let initial_ix = match cli.source.as_deref() {
        None => 0,
        Some(name) => registry.position(name).unwrap_or_else(|| {
            eprintln!(
                "zofi: unknown source `{name}` (available: {})",
                registry.names_joined()
            );
            std::process::exit(2);
        }),
    };

    application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Theme global must be installed before any module renders,
            // otherwise `cx.global::<Theme>()` panics. zofi has a short
            // lifetime so we don't bother with a watcher — the launcher
            // exits on activation/dismiss; live edits to the config only
            // take effect on the next invocation.
            cx.set_global::<ztheme::Theme>(ztheme::load());
            cx.bind_keys(launcher::key_bindings());

            cx.open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(Bounds::maximized(None, cx))),
                    app_id: Some("zofi".to_string()),
                    window_background: WindowBackgroundAppearance::Transparent,
                    kind: WindowKind::LayerShell(LayerShellOptions {
                        namespace: "zofi".to_string(),
                        layer: Layer::Overlay,
                        anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                        exclusive_zone: None,
                        keyboard_interactivity: KeyboardInteractivity::Exclusive,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| launcher::Launcher::new(registry, initial_ix, window, cx)),
            )
            .expect("failed to open zofi window: check compositor supports layer-shell");
        });
    Ok(())
}

/// Compose the source list. When the compositor exposes
/// `wlr-foreign-toplevel-management-v1` the default tab merges windows +
/// apps — that's what "launch or jump" means in practice, and the two
/// naturally share ranking. Clipboard stays as its own tab because its
/// queries are structurally different (history recall) and mixing it
/// dilutes the launch view.
fn build_registry() -> SourceRegistry {
    let t0 = std::time::Instant::now();
    // Tracker is shared across sources (apps + windows write & read from the
    // same DB). If the on-disk state dir is unreachable we fall back to an
    // in-memory tracker so the launcher still works — MRU is simply disabled
    // for the session.
    let tracker = Arc::new(match UsageTracker::open() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("usage tracker unavailable, MRU disabled: {e}");
            UsageTracker::open_in_memory().expect("in-memory SQLite open should never fail")
        }
    });
    tracing::info!("startup: tracker ready ({:?})", t0.elapsed());

    let t_spawn = std::time::Instant::now();
    let client = zwindows::spawn();
    tracing::info!(
        "startup: zwindows::spawn = {:?} (connected: {})",
        t_spawn.elapsed(),
        client.is_some()
    );

    match client {
        Some(client) => {
            tracing::info!(
                "zwindows client available — composing windows+apps UnionSource as default tab"
            );
            // Load apps first so WindowsSource can borrow their preloaded
            // `.desktop` icons — covers Feishu-style apps whose `Icon=` is an
            // absolute path that `icon-theme` can't find.
            let t_apps = std::time::Instant::now();
            let apps = AppsSource::load(tracker.clone());
            tracing::info!("startup: AppsSource::load = {:?}", t_apps.elapsed());
            let t_res = std::time::Instant::now();
            let resolver = sources::icon::build_window_icon_resolver(apps.entries());
            tracing::info!(
                "startup: build_window_icon_resolver = {:?}",
                t_res.elapsed()
            );
            let windows =
                WindowsSource::with_resolver(client, resolver).with_tracker(tracker.clone());
            let union: Box<dyn Source> = Box::new(
                UnionSource::new(vec![Box::new(windows), Box::new(apps)])
                    .with_name("launch")
                    .with_icon("◉")
                    .with_placeholder("Search windows and apps...")
                    .with_empty_text("No matches"),
            );
            let t_clip = std::time::Instant::now();
            let clip = ClipboardSource::load();
            tracing::info!("startup: ClipboardSource::load = {:?}", t_clip.elapsed());
            tracing::info!("startup: TOTAL = {:?}", t0.elapsed());
            SourceRegistry::new(vec![
                SourceEntry::from_source(union),
                SourceEntry::from_source(Box::new(clip)),
            ])
            .with_tracker(tracker)
        }
        None => {
            tracing::info!(
                "wlr-foreign-toplevel-management unavailable — using per-source switcher"
            );
            SourceRegistry::new(vec![
                SourceEntry::from_source(Box::new(AppsSource::load(tracker.clone()))),
                SourceEntry::from_source(Box::new(ClipboardSource::load())),
            ])
            .with_tracker(tracker)
        }
    }
}
