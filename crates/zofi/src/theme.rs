use gpui::{px, rgb, Hsla, Pixels};

// ── Dimensions ──────────────────────────────────────────────
pub const PANEL_W: Pixels = px(640.0);
pub const PANEL_H: Pixels = px(380.0);
pub const PANEL_RADIUS: Pixels = px(10.0);

pub const ICON_SIZE: Pixels = px(18.0);
pub const ITEM_HEIGHT: Pixels = px(30.0);
pub const ITEM_RADIUS: Pixels = px(6.0);
pub const INPUT_HEIGHT: Pixels = px(36.0);

pub const FONT_SIZE: Pixels = px(13.0);
pub const FONT_SIZE_SM: Pixels = px(11.5);

pub const PAD_X: Pixels = px(12.0);
pub const GAP: Pixels = px(8.0);

// ── Colors ──────────────────────────────────────────────────
fn rgb_alpha(hex: u32, alpha: f32) -> Hsla {
    let mut c: Hsla = rgb(hex).into();
    c.a = alpha;
    c
}

// Panel
pub fn panel_bg() -> Hsla {
    rgb(0x252530).into()
}
pub fn panel_border() -> Hsla {
    rgb_alpha(0x3e3e50, 0.6)
}

// Text
pub fn fg() -> Hsla {
    rgb(0xc8c8d0).into()
}
pub fn fg_dim() -> Hsla {
    rgb(0x6e6e80).into()
}
pub fn fg_accent() -> Hsla {
    rgb(0xe8e8f0).into()
}

// List
pub fn selected_bg() -> Hsla {
    rgb_alpha(0x444458, 0.6)
}
pub fn hover_bg() -> Hsla {
    rgb_alpha(0x363645, 0.5)
}

// Bottom bar
pub fn bar_border() -> Hsla {
    rgb_alpha(0x4e4e60, 0.25)
}
