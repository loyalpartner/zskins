//! Syntax highlighting via syntect. Loads the bundled grammars + themes once
//! and exposes a single `highlight(text, lang)` that returns colored runs.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Mutex;

use gpui::{rgb, Hsla};
use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// `two-face` ships the same syntax/theme set as `bat` — ~290 languages
/// including TypeScript, JSX/TSX, Vue, Astro, Svelte, Zig, Kotlin, Dart, etc.
static SYNTAXES: Lazy<SyntaxSet> = Lazy::new(two_face::syntax::extra_newlines);
static THEMES: Lazy<two_face::theme::EmbeddedLazyThemeSet> = Lazy::new(two_face::theme::extra);
static THEME: Lazy<&'static Theme> =
    Lazy::new(|| THEMES.get(two_face::theme::EmbeddedThemeName::Base16EightiesDark));

/// Look up a syntax for the given language hint (file extension or token).
/// Returns None if the lang is unknown — caller should render plain text.
pub fn syntax_for(lang: &str) -> Option<&'static SyntaxReference> {
    if lang.is_empty() {
        return None;
    }
    SYNTAXES.find_syntax_by_extension(lang).or_else(|| {
        SYNTAXES
            .find_syntax_by_token(lang)
            .or_else(|| SYNTAXES.find_syntax_by_name(lang))
    })
}

/// Single-slot cache for the most recently highlighted preview. The
/// launcher re-renders the preview pane on every keystroke, scroll, and
/// pulse — without this, syntect re-tokenizes a 100 KB file on each frame.
struct CacheEntry {
    text_hash: u64,
    lang: String,
    runs: Vec<(Range<usize>, Hsla)>,
}

static CACHE: Lazy<Mutex<Option<CacheEntry>>> = Lazy::new(|| Mutex::new(None));

/// Tokenize `text` and return contiguous (range, color) runs covering it.
/// Plain text (no syntax match) → single run with `default_color`.
pub fn highlight(text: &str, lang: &str, default_color: Hsla) -> Vec<(Range<usize>, Hsla)> {
    let h = hash_text(text);
    {
        let cache = CACHE.lock().unwrap();
        if let Some(e) = cache.as_ref() {
            if e.text_hash == h && e.lang == lang {
                return e.runs.clone();
            }
        }
    }
    let runs = compute(text, lang, default_color);
    *CACHE.lock().unwrap() = Some(CacheEntry {
        text_hash: h,
        lang: lang.to_string(),
        runs: runs.clone(),
    });
    runs
}

fn hash_text(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn compute(text: &str, lang: &str, default_color: Hsla) -> Vec<(Range<usize>, Hsla)> {
    let Some(syntax) = syntax_for(lang) else {
        return vec![(0..text.len(), default_color)];
    };
    let mut h = HighlightLines::new(syntax, &THEME);
    let mut runs: Vec<(Range<usize>, Hsla)> = Vec::new();
    let mut offset = 0usize;

    for line in LinesWithEndings::from(text) {
        let line_len = line.len();
        let regions = match h.highlight_line(line, &SYNTAXES) {
            Ok(r) => r,
            Err(_) => {
                runs.push((offset..offset + line_len, default_color));
                offset += line_len;
                continue;
            }
        };
        for (style, slice) in regions {
            let len = slice.len();
            if len == 0 {
                continue;
            }
            runs.push((offset..offset + len, syntect_color(style)));
            offset += len;
        }
    }
    runs
}

fn syntect_color(style: Style) -> Hsla {
    let c = style.foreground;
    rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32).into()
}
