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

    // Prefer sway IPC (logical width, matches compositor scaling) when available,
    // otherwise fall back to a generic Wayland wl_output probe so we still get
    // names + widths on niri/hyprland/cosmic.
    let sway_widths = zbar::backend::sway::query_output_widths();
    let sway_widths = if sway_widths.is_empty() {
        zbar::backend::ext_workspace::query_wayland_outputs()
    } else {
        sway_widths
    };

    application().run(move |cx: &mut App| {
        // Theme global must be installed before any module renders, otherwise
        // the first `cx.global::<Theme>()` call panics with "no global of type
        // Theme". Loading is fire-and-forget — failure paths inside `load`
        // fall back to mocha and log via tracing.
        cx.set_global::<ztheme::Theme>(ztheme::load());
        spawn_theme_watcher(cx);

        cx.bind_keys(zbar::modules::network_popup::key_bindings());
        cx.bind_keys(zbar::modules::tray_menu::key_bindings());
        cx.bind_keys(zbar::modules::settings::key_bindings());
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
                    let tray = cx.new(|cx| bar::TrayModule::new(None, cx));
                    let window_title = cx.new(bar::WindowTitleModule::new);
                    open_bar(
                        cx,
                        backend.clone(),
                        None,
                        None,
                        px(1920.),
                        tray,
                        window_title,
                    );
                } else {
                    tracing::info!("opening zbar on {} output(s)", displays.len());
                    // Create shared state that should NOT be duplicated per
                    // bar. TrayModule owns a DBus SNI host — one per process.
                    // The primary display is passed so popups have a home
                    // until a click-site-aware implementation lands.
                    let primary_display = displays.first().map(|d| d.id());
                    let tray = cx.new(|cx| bar::TrayModule::new(primary_display, cx));
                    let window_title = cx.new(bar::WindowTitleModule::new);
                    // Cross-connection match: GPUI's `display.uuid()` is
                    // `Uuid::v5(NAMESPACE_DNS, name)`, so we invert by hashing
                    // each known output name and building a UUID->name map.
                    let uuid_to_name: std::collections::HashMap<uuid::Uuid, String> = sway_widths
                        .iter()
                        .map(|(n, _)| (zbar::backend::output_name_uuid(n), n.clone()))
                        .collect();
                    let name_to_width: std::collections::HashMap<String, f32> =
                        sway_widths.iter().cloned().collect();
                    for display in displays.iter() {
                        let id = display.id();
                        let output_name = display
                            .uuid()
                            .ok()
                            .and_then(|u| uuid_to_name.get(&u).cloned());
                        let width = output_name
                            .as_deref()
                            .and_then(|n| name_to_width.get(n).copied())
                            .map(px)
                            .unwrap_or(display.bounds().size.width);
                        tracing::info!(
                            "  -> display id={id:?} width={width:?} output={output_name:?}"
                        );
                        open_bar(
                            cx,
                            backend.clone(),
                            Some(id),
                            output_name,
                            width,
                            tray.clone(),
                            window_title.clone(),
                        );
                    }
                }
            });
        })
        .detach();
    });
}

/// Wire `ztheme::watch` into GPUI's main loop. The watcher fires from a
/// background thread; we route every new theme through an async channel
/// and consume it in a `cx.spawn` task that owns an [`AsyncApp`], so the
/// final `cx.set_global` + `cx.refresh_windows` runs on GPUI's thread.
///
/// The [`ztheme::WatcherHandle`] is leaked deliberately — the bar runs
/// for the entire process lifetime and tearing the watcher down at exit
/// adds nothing that the OS doesn't already do for us.
fn spawn_theme_watcher(cx: &mut App) {
    let (tx, rx) = async_channel::bounded::<ztheme::Theme>(8);
    let handle = match ztheme::watch(move |theme| {
        // try_send drops the new theme if the channel is full — that's
        // fine, the next event will carry the latest state via load().
        let _ = tx.try_send(theme);
    }) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "ztheme: watcher disabled");
            return;
        }
    };
    // Stash the handle inside the GPUI App so its lifetime matches the app.
    // Using a leaked Box keeps the handle alive without a struct field.
    Box::leak(Box::new(handle));

    cx.spawn(async move |cx| {
        while let Ok(theme) = rx.recv().await {
            // `cx.update` panics only if the app has been dropped — at that
            // point the foreground executor has stopped scheduling our task
            // anyway, so we never observe the panic in practice. Catching it
            // would require AnyhowAsyncContext glue that this single bounce
            // doesn't justify.
            cx.update(|cx| {
                cx.set_global::<ztheme::Theme>(theme);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn open_bar(
    cx: &mut App,
    backend: Option<std::sync::Arc<dyn zbar::backend::WorkspaceBackend>>,
    display_id: Option<gpui::DisplayId>,
    output_name: Option<String>,
    width: gpui::Pixels,
    tray: gpui::Entity<bar::TrayModule>,
    window_title: gpui::Entity<bar::WindowTitleModule>,
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
        |_, cx| cx.new(|cx| Bar::new(backend, display_id, output_name, tray, window_title, cx)),
    );
    if let Err(e) = result {
        tracing::warn!("failed to open zbar window on display {display_id:?}: {e:#}");
    }
}
