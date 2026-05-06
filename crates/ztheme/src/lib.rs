//! Shared theme system for the zskins workspace.
//!
//! Exposes a [`Theme`] struct with 16 semantic color slots, two built-in
//! palettes (Catppuccin Mocha + Latte), config IO helpers, and a `notify`-
//! backed file watcher so live edits to `~/.config/zskins/config.toml`
//! re-render running apps without a restart.
//!
//! `Theme` implements [`gpui::Global`], so consumers can stash it via
//! `cx.set_global::<Theme>(ztheme::load())` and read it back with
//! `cx.global::<Theme>()` in any render path.

use std::path::{Path, PathBuf};

use gpui::{rgb, Hsla};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod watcher;
pub use watcher::{watch, WatcherHandle};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(String),
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

impl From<toml::de::Error> for ThemeError {
    fn from(e: toml::de::Error) -> Self {
        ThemeError::Toml(e.to_string())
    }
}

impl From<toml::ser::Error> for ThemeError {
    fn from(e: toml::ser::Error) -> Self {
        ThemeError::Toml(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// 16-slot semantic color palette shared by zbar and zofi. All fields are
/// [`Hsla`] so consumers can paint directly via GPUI's color APIs.
///
/// The struct is `Copy` because every field is `Hsla` (a 4×f32). That
/// keeps `cx.global::<Theme>()` lookups effectively zero-cost — callers
/// can copy the slot they need into a local without lifetime gymnastics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_hover: Hsla,
    pub surface_alt: Hsla,
    pub fg: Hsla,
    pub fg_dim: Hsla,
    pub fg_accent: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub urgent: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub border: Hsla,
    pub separator: Hsla,
    pub selected_bg: Hsla,
    pub hover_bg: Hsla,
}

impl gpui::Global for Theme {}

fn alpha(hex: u32, a: f32) -> Hsla {
    let mut c: Hsla = rgb(hex).into();
    c.a = a;
    c
}

impl Theme {
    /// Catppuccin Mocha — the dark default. Semitransparent surfaces keep
    /// the bar's frosted-glass aesthetic; tweaking these alphas should be
    /// done with care because the GPU compositor blends underneath.
    pub fn catppuccin_mocha() -> Self {
        Self {
            bg: alpha(0x1e1e2e, 0.7),
            surface: alpha(0x313244, 0.5),
            surface_hover: alpha(0x45475a, 0.6),
            surface_alt: rgb(0x181825).into(),
            fg: rgb(0xcdd6f4).into(),
            fg_dim: rgb(0xa6adc8).into(),
            fg_accent: rgb(0xbac2de).into(),
            accent: rgb(0x89b4fa).into(),
            accent_soft: alpha(0x89b4fa, 0.22),
            urgent: rgb(0xf38ba8).into(),
            warning: rgb(0xfab387).into(),
            success: rgb(0xa6e3a1).into(),
            border: alpha(0x6c7086, 0.2),
            separator: alpha(0x6c7086, 0.15),
            selected_bg: alpha(0x45475a, 0.6),
            hover_bg: alpha(0x313244, 0.5),
        }
    }

    /// Catppuccin Latte — the light variant. `bg` uses a higher alpha than
    /// Mocha because light semi-transparent bars tend to read as washed-out
    /// on bright wallpapers.
    pub fn catppuccin_latte() -> Self {
        Self {
            bg: alpha(0xeff1f5, 0.85),
            // Surfaces sit lighter than mocha so pills float over the
            // bg with visible contrast on bright wallpapers.
            surface: alpha(0xccd0da, 0.4),
            surface_hover: alpha(0xbcc0cc, 0.55),
            surface_alt: rgb(0xe6e9ef).into(),
            fg: rgb(0x4c4f69).into(),
            fg_dim: rgb(0x6c6f85).into(),
            fg_accent: rgb(0x5c5f77).into(),
            accent: rgb(0x1e66f5).into(),
            accent_soft: alpha(0x1e66f5, 0.18),
            urgent: rgb(0xd20f39).into(),
            warning: rgb(0xfe640b).into(),
            success: rgb(0x40a02b).into(),
            border: alpha(0x9ca0b0, 0.35),
            separator: alpha(0x9ca0b0, 0.25),
            // selected_bg promoted to surface2 so it reads as the most
            // emphasized state; hover_bg sits between surface and selected.
            selected_bg: alpha(0xacb0be, 0.7),
            hover_bg: alpha(0xbcc0cc, 0.5),
        }
    }
}

// ---------------------------------------------------------------------------
// Brightness helpers
// ---------------------------------------------------------------------------

/// Whether the active theme reads as a light palette. Built-in mocha returns
/// `false`, latte returns `true`. Used by callers that need to flip a
/// product-specific palette without taking a hard dep on a specific theme
/// preset (e.g. zofi's `kind_*`/`kbd_*`/`category` helpers).
///
/// Lightness threshold is fixed at `bg.l > 0.5` — this stays true for any
/// reasonable user-supplied light theme even if its exact bg differs from
/// catppuccin latte.
pub fn is_light(theme: &Theme) -> bool {
    theme.bg.l > 0.5
}

/// Foreground hex string suitable for SVG symbolic-icon recoloring. Returns
/// the literal hex used by mocha (`#cdd6f4`) or latte (`#4c4f69`) so it can
/// be plugged directly into a `String::replace` pass without a HSL→RGB
/// conversion.
pub fn fg_hex(theme: &Theme) -> &'static str {
    if is_light(theme) {
        "#4c4f69"
    } else {
        "#cdd6f4"
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::catppuccin_mocha()
    }
}

// ---------------------------------------------------------------------------
// Config IO
// ---------------------------------------------------------------------------

const MOCHA_NAME: &str = "catppuccin-mocha";
const LATTE_NAME: &str = "catppuccin-latte";

/// Map a [`Theme`] back to the canonical name used in the config file.
/// Returns `None` if the theme isn't a known built-in (e.g. someone
/// constructed a custom `Theme` literal at runtime). UI layers use this
/// to mark which row of the picker is currently active.
pub fn name_for(theme: &Theme) -> Option<&'static str> {
    if *theme == Theme::catppuccin_mocha() {
        Some(MOCHA_NAME)
    } else if *theme == Theme::catppuccin_latte() {
        Some(LATTE_NAME)
    } else {
        None
    }
}

/// Resolve a theme name to a built-in palette. Unknown names fall back to
/// Mocha — the same fallback `load()` uses for malformed config — so callers
/// don't have to decide what "unknown" means at every call site.
pub fn theme_from_name(name: &str) -> Theme {
    match name {
        LATTE_NAME => Theme::catppuccin_latte(),
        // Mocha is the fallback for the canonical name + every unknown value.
        _ => Theme::catppuccin_mocha(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigFile {
    theme: ThemeSection,
}

#[derive(Debug, Serialize, Deserialize)]
struct ThemeSection {
    name: String,
}

/// Resolve the on-disk config path. Honors `$XDG_CONFIG_HOME`, falling
/// back to `$HOME/.config/zskins/config.toml`. The path is returned even
/// if the file doesn't exist — callers (load/save/watch) decide what to
/// do about it.
pub fn config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("zskins").join("config.toml");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("zskins")
            .join("config.toml");
    }
    // Last-resort fallback: dirs crate (handles cases like passwd lookups
    // when neither env var is set, e.g. inside some sandboxed runtimes).
    if let Some(cfg) = dirs::config_dir() {
        return cfg.join("zskins").join("config.toml");
    }
    PathBuf::from("zskins-config.toml")
}

/// Read the config file and return the theme it points at. Any failure
/// (missing file, IO error, malformed TOML, unknown theme name) falls
/// back to Mocha and emits a `tracing::warn!` so the issue surfaces in
/// logs without crashing the app.
pub fn load() -> Theme {
    load_from(&config_path())
}

fn load_from(path: &Path) -> Theme {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // Missing config is the common path on first run; log at debug
            // level. Anything else (permissions, IO) is genuinely unexpected.
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!(?path, "ztheme: no config file, using mocha default");
            } else {
                tracing::warn!(?path, error = %e, "ztheme: failed to read config, using mocha default");
            }
            return Theme::catppuccin_mocha();
        }
    };
    let cfg: ConfigFile = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?path, error = %e, "ztheme: malformed config, using mocha default");
            return Theme::catppuccin_mocha();
        }
    };
    let theme = theme_from_name(&cfg.theme.name);
    if name_for(&theme)
        .map(|n| n != cfg.theme.name)
        .unwrap_or(false)
    {
        tracing::warn!(
            requested = cfg.theme.name,
            "ztheme: unknown theme name, falling back to mocha"
        );
    }
    theme
}

/// Persist `name` to the config file. Writes through a `.tmp` sibling +
/// rename so a crashed write never leaves a half-written file behind.
/// Auto-creates the parent directory.
pub fn save(name: &str) -> Result<(), ThemeError> {
    save_to(&config_path(), name)
}

fn save_to(path: &Path, name: &str) -> Result<(), ThemeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = ConfigFile {
        theme: ThemeSection {
            name: name.to_string(),
        },
    };
    let text = toml::to_string_pretty(&cfg)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_cfg(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn mocha_and_latte_are_distinct() {
        assert_ne!(Theme::catppuccin_mocha(), Theme::catppuccin_latte());
    }

    #[test]
    fn name_for_round_trips() {
        assert_eq!(name_for(&Theme::catppuccin_mocha()), Some(MOCHA_NAME));
        assert_eq!(name_for(&Theme::catppuccin_latte()), Some(LATTE_NAME));
    }

    #[test]
    fn theme_from_name_handles_known_and_unknown() {
        assert_eq!(theme_from_name(MOCHA_NAME), Theme::catppuccin_mocha());
        assert_eq!(theme_from_name(LATTE_NAME), Theme::catppuccin_latte());
        // Unknown name falls back to mocha rather than panicking.
        assert_eq!(theme_from_name("does-not-exist"), Theme::catppuccin_mocha());
    }

    #[test]
    fn load_missing_file_falls_back_to_mocha() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.toml");
        assert_eq!(load_from(&path), Theme::catppuccin_mocha());
    }

    #[test]
    fn load_malformed_falls_back_to_mocha() {
        let dir = TempDir::new().unwrap();
        let path = write_cfg(dir.path(), "this is not toml = =");
        assert_eq!(load_from(&path), Theme::catppuccin_mocha());
    }

    #[test]
    fn load_unknown_name_falls_back_to_mocha() {
        let dir = TempDir::new().unwrap();
        let path = write_cfg(dir.path(), "[theme]\nname = \"solarized\"\n");
        assert_eq!(load_from(&path), Theme::catppuccin_mocha());
    }

    #[test]
    fn save_then_load_round_trips_latte() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        save_to(&path, LATTE_NAME).unwrap();
        assert_eq!(load_from(&path), Theme::catppuccin_latte());
    }

    #[test]
    fn save_then_load_round_trips_mocha() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        save_to(&path, MOCHA_NAME).unwrap();
        assert_eq!(load_from(&path), Theme::catppuccin_mocha());
    }

    #[test]
    fn is_light_distinguishes_presets() {
        assert!(!is_light(&Theme::catppuccin_mocha()));
        assert!(is_light(&Theme::catppuccin_latte()));
    }

    #[test]
    fn fg_hex_returns_preset_specific_literal() {
        assert_eq!(fg_hex(&Theme::catppuccin_mocha()), "#cdd6f4");
        assert_eq!(fg_hex(&Theme::catppuccin_latte()), "#4c4f69");
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        // Two levels deep — neither exists yet. Save must mkdir -p.
        let path = dir.path().join("a").join("b").join("config.toml");
        save_to(&path, LATTE_NAME).unwrap();
        assert!(path.exists());
        assert_eq!(load_from(&path), Theme::catppuccin_latte());
    }
}
