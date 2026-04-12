use gpui::{div, px, rgb, Div, Hsla, InteractiveElement, ParentElement, Pixels, Styled};

pub const BAR_HEIGHT: Pixels = px(34.0);
pub const FONT_SIZE: Pixels = px(12.5);
pub const PADDING_X: Pixels = px(10.0);
pub const MODULE_GAP: Pixels = px(4.0);
pub const PILL_PX: Pixels = px(8.0);
pub const PILL_PY: Pixels = px(3.0);
const PCT_WIDTH: Pixels = px(32.0);

fn with_alpha(hex: u32, a: f32) -> Hsla {
    let mut c: Hsla = rgb(hex).into();
    c.a = a;
    c
}

pub fn bg() -> Hsla {
    with_alpha(0x1e1e2e, 0.7)
}
pub fn fg() -> Hsla {
    rgb(0xcdd6f4).into()
}
pub fn fg_dim() -> Hsla {
    rgb(0xa6adc8).into()
}
pub fn accent() -> Hsla {
    rgb(0x89b4fa).into()
}
pub fn accent_dim() -> Hsla {
    with_alpha(0x89b4fa, 0.15)
}
pub fn accent_dim_hover() -> Hsla {
    with_alpha(0x89b4fa, 0.25)
}
pub fn surface() -> Hsla {
    with_alpha(0x313244, 0.5)
}
pub fn surface_hover() -> Hsla {
    with_alpha(0x45475a, 0.6)
}
pub fn urgent() -> Hsla {
    rgb(0xf38ba8).into()
}
pub fn warning() -> Hsla {
    rgb(0xfab387).into()
}
pub fn green() -> Hsla {
    rgb(0xa6e3a1).into()
}
pub fn border() -> Hsla {
    with_alpha(0x6c7086, 0.2)
}
pub fn separator() -> Hsla {
    with_alpha(0x6c7086, 0.15)
}

pub fn pill() -> Div {
    div()
        .px(PILL_PX)
        .py(PILL_PY)
        .rounded_md()
        .bg(surface())
        .hover(|s| s.bg(surface_hover()))
}

pub fn pct_label(value: impl std::fmt::Display, color: Hsla) -> Div {
    div()
        .min_w(PCT_WIDTH)
        .text_color(color)
        .child(format!("{value:>3}%"))
}

pub fn threshold_color(pct: f32, warn_at: f32, urgent_at: f32) -> Hsla {
    if pct >= urgent_at {
        urgent()
    } else if pct >= warn_at {
        warning()
    } else {
        fg_dim()
    }
}
