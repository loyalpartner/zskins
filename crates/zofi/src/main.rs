mod assets;
mod clipd;
mod highlight;
mod input;
mod launcher;
mod source;
mod sources;
mod theme;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gpui::{
    layer_shell::*, App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions,
};
use gpui_platform::application;

use crate::source::Source;

pub struct SourceEntry {
    pub name: &'static str,
    pub icon: &'static str,
    pub factory: fn() -> Box<dyn Source>,
}

pub const SOURCES: &[SourceEntry] = &[
    SourceEntry {
        name: "apps",
        icon: "⊞",
        factory: || Box::new(sources::apps::AppsSource::load()),
    },
    SourceEntry {
        name: "clipboard",
        icon: "▤",
        factory: || Box::new(sources::clipboard::ClipboardSource::load()),
    },
    SourceEntry {
        name: "files",
        icon: "▦",
        factory: || Box::new(sources::files::FilesSource::load()),
    },
];

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

    let initial_ix = match cli.source.as_deref() {
        None => 0,
        Some(name) => SOURCES
            .iter()
            .position(|s| s.name == name)
            .unwrap_or_else(|| {
                eprintln!(
                    "zofi: unknown source `{name}` (available: {})",
                    SOURCES
                        .iter()
                        .map(|s| s.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(2);
            }),
    };

    application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
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
                |window, cx| cx.new(|cx| launcher::Launcher::new(initial_ix, window, cx)),
            )
            .expect("failed to open zofi window: check compositor supports layer-shell");
        });
    Ok(())
}
