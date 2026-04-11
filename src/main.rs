use gpui::{
    App, AppContext, Bounds, Context, IntoElement, Render, Size, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
    div, layer_shell::*, point, px, rgba,
};
use gpui_platform::application;

const BAR_HEIGHT: f32 = 32.0;

struct Bar;

impl Render for Bar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgba(0x1e1e2eff))
    }
}

fn main() {
    env_logger::init();

    application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: Size::new(px(1920.), px(BAR_HEIGHT)),
                })),
                app_id: Some("zbar".to_string()),
                window_background: WindowBackgroundAppearance::Opaque,
                kind: WindowKind::LayerShell(LayerShellOptions {
                    namespace: "zbar".to_string(),
                    layer: Layer::Top,
                    anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                    exclusive_zone: Some(px(BAR_HEIGHT)),
                    keyboard_interactivity: KeyboardInteractivity::None,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Bar),
        )
        .unwrap();
    });
}
