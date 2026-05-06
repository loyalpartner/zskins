//! Layout constants and source-specific palette helpers for zofi.
//!
//! Generic semantic colors live in the workspace-shared [`ztheme`] crate
//! and are read via `cx.global::<ztheme::Theme>()` at render time. This
//! module retains:
//! - dimensional constants (panel/preview sizes, font sizes, paddings)
//! - product-specific palettes that don't belong in the cross-crate
//!   contract: clipboard `kind_*` chips, keyboard hint pills, the
//!   per-source category tint, and the bar border tween. These are
//!   intentionally *not* themed yet — they're zofi-specific UX hooks
//!   the user hasn't asked to recolor.

use gpui::{px, rgb, Hsla, Pixels};
use ztheme::Theme;

// ── Dimensions ──────────────────────────────────────────────
pub const PANEL_W: Pixels = px(640.0);
pub const PANEL_H: Pixels = px(380.0);
/// In split layout: narrower list column (preview is the primary surface).
pub const SPLIT_LIST_W: Pixels = px(360.0);
/// In split layout: preview pane width.
pub const SPLIT_PREVIEW_W: Pixels = px(720.0);
/// In split layout: taller panel so images and long text breathe.
pub const SPLIT_PANEL_H: Pixels = px(540.0);
/// Preview pane inner image area after pane padding (px(20) horizontal,
/// px(16) vertical — see `render_preview_pane`). Used by window thumbnail
/// pre-shrinking so the GPU bilinear sampler does near-identity resampling.
pub const PREVIEW_IMG_MAX_W: Pixels = px(680.0);
pub const PREVIEW_IMG_MAX_H: Pixels = px(508.0);
pub const PANEL_RADIUS: Pixels = px(10.0);

pub const ICON_SIZE: Pixels = px(24.0);
pub const ITEM_HEIGHT: Pixels = px(44.0);
pub const ITEM_RADIUS: Pixels = px(6.0);
pub const INPUT_HEIGHT: Pixels = px(36.0);

pub const FONT_SIZE: Pixels = px(13.0);
pub const FONT_SIZE_SM: Pixels = px(11.5);
pub const PREVIEW_FONT_SIZE: Pixels = px(14.0);

pub const PAD_X: Pixels = px(12.0);
pub const GAP: Pixels = px(8.0);

// ── Colors ──────────────────────────────────────────────────
fn rgb_alpha(hex: u32, alpha: f32) -> Hsla {
    let mut c: Hsla = rgb(hex).into();
    c.a = alpha;
    c
}

// ── Product-specific palette (not part of the shared Theme) ────────────
//
// Clipboard `kind_*` colors, the kbd-hint pills, and the source category
// tints are zofi-specific affordances. They do not yet vary with the
// global theme — keeping them constant means the visual semantics
// ("text variants are blue, image variants are orange", "primary action
// is the brighter pill") stay legible across both palettes.

// ── Clipboard kind palette ──────────────────────────────────
pub fn kind_text_fg() -> Hsla {
    rgb(0x7fb8ff).into()
}
pub fn kind_text_bg() -> Hsla {
    rgb_alpha(0x3a5a85, 0.35)
}
pub fn kind_image_fg() -> Hsla {
    rgb(0xffb56e).into()
}
pub fn kind_image_bg() -> Hsla {
    rgb_alpha(0x7a4a26, 0.35)
}

// Bottom bar
pub fn bar_border() -> Hsla {
    rgb_alpha(0x4e4e60, 0.25)
}

// Keyboard-hint pills (bottom bar)
pub fn kbd_bg() -> Hsla {
    rgb(0x30353f).into()
}
pub fn kbd_fg() -> Hsla {
    rgb(0xaab1bc).into()
}
pub fn kbd_border() -> Hsla {
    rgb_alpha(0x3d4350, 1.0)
}
pub fn kbd_accent_bg() -> Hsla {
    rgb_alpha(0x4f8cff, 0.25)
}
pub fn kbd_accent_fg() -> Hsla {
    rgb(0xe5edff).into()
}
pub fn kbd_accent_border() -> Hsla {
    rgb_alpha(0x4f8cff, 0.45)
}

// Preview-header "active" pill: green text on a translucent green bg.
pub fn pill_active_fg() -> Hsla {
    rgb(0x5ecf8a).into()
}
pub fn pill_active_bg() -> Hsla {
    rgb_alpha(0x5ecf8a, 0.15)
}

/// Per-source tint for icons in the source bar and union gutter. Unknown
/// names fall back to the shared theme's `accent` so new sources are never
/// visually broken — just un-tinted until they get a palette entry.
///
/// Takes a [`Theme`] for the fallback path so the unknown-source case
/// stays in sync with whatever palette the user picked.
pub fn category(name: &str, t: &Theme) -> Hsla {
    match name {
        "windows" => rgb(0x4aa8ff).into(),
        "apps" => rgb(0xff9933).into(),
        "files" => rgb(0x33cc66).into(),
        "clipboard" => rgb(0xc466ff).into(),
        _ => t.accent,
    }
}
