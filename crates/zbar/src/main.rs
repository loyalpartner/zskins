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

    application().run(move |cx: &mut App| {
        cx.bind_keys(zbar::modules::network_popup::key_bindings());
        let backend = backend.clone();
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
                    open_bar(cx, backend.clone(), None);
                } else {
                    tracing::info!("opening zbar on {} output(s)", displays.len());
                    for display in displays {
                        let id = display.id();
                        tracing::info!("  -> display id={id:?}");
                        open_bar(cx, backend.clone(), Some(id));
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
) {
    // width=0 lets the compositor size the surface via Anchor::LEFT|RIGHT.
    // Passing a real width breaks rotated outputs because GPUI's display.bounds()
    // reports pre-transform (landscape) dimensions.
    let result = cx.open_window(
        WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(0.), BAR_HEIGHT),
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
