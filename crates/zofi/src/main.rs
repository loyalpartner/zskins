mod input;
mod launcher;
mod source;
mod sources;
mod theme;

use gpui::{
    layer_shell::*, App, AppContext, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions,
};
use gpui_platform::application;

use crate::source::Source;

const DEFAULT_SOURCE: &str = "apps";
const KNOWN_SOURCES: &[&str] = &["apps"];

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let source_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
    if matches!(source_name.as_str(), "-h" | "--help" | "help") {
        print_help();
        return;
    }

    let source = match build_source(&source_name) {
        Some(s) => s,
        None => {
            eprintln!("zofi: unknown source `{source_name}`");
            print_help();
            std::process::exit(2);
        }
    };

    application().run(move |cx: &mut App| {
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
            |window, cx| cx.new(|cx| launcher::Launcher::new(source, window, cx)),
        )
        .expect("failed to open zofi window: check compositor supports layer-shell");
    });
}

fn build_source(name: &str) -> Option<Box<dyn Source>> {
    match name {
        "apps" => Some(Box::new(sources::apps::AppsSource::load())),
        _ => None,
    }
}

fn print_help() {
    println!("usage: zofi [SOURCE]");
    println!();
    println!("Sources:");
    for s in KNOWN_SOURCES {
        let marker = if *s == DEFAULT_SOURCE { " (default)" } else { "" };
        println!("  {s}{marker}");
    }
}
