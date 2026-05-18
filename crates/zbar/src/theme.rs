//! Layout constants and small render helpers for the bar.
//!
//! Color tokens have moved to the workspace-shared [`ztheme`] crate. Read
//! them via `cx.global::<ztheme::Theme>()` at render time. The dimensions
//! and a pair of helpers (pill, pct_label, threshold_color) live here
//! because they're zbar-specific layout, not part of the cross-crate
//! palette contract.

use gpui::{div, px, App, Div, Hsla, InteractiveElement, ParentElement, Pixels, Styled};
use ztheme::Theme;

pub const BAR_HEIGHT: Pixels = px(34.0);
pub const FONT_SIZE: Pixels = px(12.5);
pub const PADDING_X: Pixels = px(10.0);
pub const MODULE_GAP: Pixels = px(4.0);
pub const PILL_PX: Pixels = px(8.0);
pub const PILL_PY: Pixels = px(3.0);
const PCT_WIDTH: Pixels = px(28.0);

/// Hover-bg for a workspace pill in the active state. The shared `Theme`
/// only carries one accent_soft slot; we want the workspace pill's hover
/// to bump the alpha a notch above the idle state. Derived rather than
/// hand-coded so the hue stays perfectly in sync with the active palette.
pub fn accent_dim_hover(t: &Theme) -> Hsla {
    let mut c = t.accent_soft;
    c.a = (c.a + 0.10).min(1.0);
    c
}

/// Standard pill chrome used by every status module (clock, volume,
/// brightness, battery, etc.). Pulls colors off the global theme so the
/// pill restyles itself when the user swaps palettes.
pub fn pill(cx: &App) -> Div {
    let t = cx.global::<Theme>();
    div()
        .px(PILL_PX)
        .py(PILL_PY)
        .rounded_md()
        .bg(t.surface)
        .hover(move |s| s.bg(t.surface_hover))
}

/// Fixed-width, right-aligned percentage label (`"5%"`, `"87%"`, `"100%"`).
///
/// The width is fixed (not `min_w`) so CPU and memory labels are exactly
/// equal width and the pill never jitters as the integer grows from 1 to 3
/// digits. `justify_end` right-aligns the glyphs, so the `%` lines up
/// regardless of the proportional font's space width — space-padding the
/// string would only align in a monospace font.
pub fn pct_label(value: impl std::fmt::Display, color: Hsla) -> Div {
    div()
        .w(PCT_WIDTH)
        .flex()
        .justify_end()
        .text_color(color)
        .child(format!("{value}%"))
}

/// Map a percentage to a semantic color via two thresholds: anything at
/// or above `urgent_at` paints urgent, between `warn_at..urgent_at` paints
/// warning, and below `warn_at` falls back to dim foreground.
pub fn threshold_color(cx: &App, pct: f32, warn_at: f32, urgent_at: f32) -> Hsla {
    let t = cx.global::<Theme>();
    if pct >= urgent_at {
        t.urgent
    } else if pct >= warn_at {
        t.warning
    } else {
        t.fg_dim
    }
}
