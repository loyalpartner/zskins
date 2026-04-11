use gpui::{Hsla, Pixels, px, rgb};

pub const BAR_HEIGHT: Pixels = px(32.0);
pub const FONT_SIZE: Pixels = px(13.0);
pub const PADDING_X: Pixels = px(8.0);
pub const MODULE_GAP: Pixels = px(6.0);

pub fn bg() -> Hsla { rgb(0x1e1e2e).into() }
pub fn fg() -> Hsla { rgb(0xcdd6f4).into() }
#[allow(dead_code)]
pub fn accent() -> Hsla { rgb(0x89b4fa).into() }
#[allow(dead_code)]
pub fn muted() -> Hsla { rgb(0x45475a).into() }
