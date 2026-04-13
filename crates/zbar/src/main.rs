mod bar;

use gpui::{
    layer_shell::*, point, px, App, AppContext, Bounds, Size, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions,
};
use gpui_platform::application;
use zbar::theme::BAR_HEIGHT;

use crate::bar::Bar;

fn main() {
    // Must be set before any threads spawn.
    std::env::set_var("XKB_COMPOSE_DISABLE", "1");

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let backend = zbar::backend::detect::detect_backend();

    // Query sway for output widths (rect.width = compositor logical pixels).
    // GPUI's display.bounds() may use a different scale, so we prefer sway's values.
    let sway_widths = zbar::backend::sway::query_output_widths();

    application().run(move |cx: &mut App| {
        cx.bind_keys(zbar::modules::network_popup::key_bindings());
        let backend = backend.clone();
        let sway_widths = sway_widths.clone();
        // Wayland output events arrive asynchronously after bind; wait briefly so
        // cx.displays() can return every monitor instead of racing the roundtrip.
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            cx.update(|cx| {
                let displays = cx.displays();
                if displays.is_empty() {
                    tracing::warn!(
                        "no displays reported; opening a single bar without output targeting"
                    );
                    open_bar(cx, backend.clone(), None, px(1920.));
                } else {
                    tracing::info!("opening zbar on {} output(s)", displays.len());
                    for (i, display) in displays.iter().enumerate() {
                        let id = display.id();
                        // Use sway rect width (compositor logical px) when available,
                        // fall back to GPUI display bounds otherwise.
                        let width = sway_widths
                            .get(i)
                            .map(|(_, w)| px(*w))
                            .unwrap_or(display.bounds().size.width);
                        tracing::info!("  -> display id={id:?} width={width:?}");
                        open_bar(cx, backend.clone(), Some(id), width);
                    }
                }
            });
        })
        .detach();
    });
}

fn open_bar(
    cx: &mut App,
    backend: Option<std::sync::Arc<dyn zbar::backend::WorkspaceBackend>>,
    display_id: Option<gpui::DisplayId>,
    width: gpui::Pixels,
) {
    // Use the display's reported width so wgpu gets a valid initial surface.
    // Anchor::LEFT|RIGHT will still let the compositor adjust if needed.
    let result = cx.open_window(
        WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(width, BAR_HEIGHT),
            })),
            display_id,
            app_id: Some("zbar".to_string()),
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::LayerShell(LayerShellOptions {
                namespace: "zbar".to_string(),
                layer: Layer::Top,
                anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                exclusive_zone: Some(BAR_HEIGHT),
                keyboard_interactivity: KeyboardInteractivity::None,
                ..Default::default()
            }),
            ..Default::default()
        },
        |_, cx| cx.new(|cx| Bar::new(backend, display_id, cx)),
    );
    if let Err(e) = result {
        tracing::warn!("failed to open zbar window on display {display_id:?}: {e:#}");
    }
}
