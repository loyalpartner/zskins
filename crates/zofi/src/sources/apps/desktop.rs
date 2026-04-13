use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::SharedString;
use rayon::prelude::*;

#[derive(Clone)]
pub struct DesktopEntry {
    pub name: SharedString,
    pub exec: String,
    pub icon_name: Option<String>,
    pub icon_path: Option<PathBuf>,
    pub icon_data: Option<Arc<gpui::Image>>,
    pub search_key: String,
}

/// Load all visible Application-type .desktop entries.
/// Uses rayon to parse files in parallel.
/// User entries (~/.local/share/applications/) override system entries by filename.
pub fn load_entries() -> Vec<DesktopEntry> {
    let dirs = [
        PathBuf::from("/usr/share/applications"),
        dirs_next_data_local().join("applications"),
    ];

    // Collect all .desktop paths first.
    let mut paths: Vec<PathBuf> = Vec::new();
    for dir in &dirs {
        if let Ok(rd) = fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop") {
                    paths.push(path);
                }
            }
        }
    }

    // Parse in parallel.
    let parsed: Vec<(String, DesktopEntry)> = paths
        .par_iter()
        .filter_map(|path| {
            let de = parse_desktop_file(path)?;
            let key = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            Some((key, de))
        })
        .collect();

    // Deduplicate (user overrides system by filename).
    let mut map: HashMap<String, DesktopEntry> = HashMap::with_capacity(parsed.len());
    for (key, de) in parsed {
        map.insert(key, de);
    }

    let mut entries: Vec<DesktopEntry> = map.into_values().collect();
    entries.sort_by(|a, b| a.search_key.cmp(&b.search_key));
    entries
}

fn dirs_next_data_local() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(dir)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share")
    } else {
        PathBuf::from("/tmp")
    }
}

fn parse_desktop_file(path: &Path) -> Option<DesktopEntry> {
    let content = fs::read_to_string(path).ok()?;

    let mut in_desktop_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut entry_type: Option<String> = None;
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if in_desktop_entry {
                // Reached another section, stop parsing.
                break;
            }
            if line == "[Desktop Entry]" {
                in_desktop_entry = true;
            }
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "Name" => {
                    if name.is_none() {
                        name = Some(value.to_string());
                    }
                }
                "Exec" => exec = Some(value.to_string()),
                "Icon" => icon = Some(value.to_string()),
                "Type" => entry_type = Some(value.to_string()),
                "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
                "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
    }

    let name = name?;
    let exec = exec?;
    let entry_type = entry_type?;

    if entry_type != "Application" || no_display || hidden {
        return None;
    }

    let search_key = name.to_lowercase();

    Some(DesktopEntry {
        name: name.into(),
        exec,
        icon_name: icon,
        icon_path: None,
        icon_data: None,
        search_key,
    })
}

/// Strip freedesktop field codes from an Exec string.
pub fn strip_field_codes(exec: &str) -> String {
    let mut result = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            if let Some(&next) = chars.peek() {
                match next {
                    'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {
                        chars.next();
                        continue;
                    }
                    '%' => {
                        chars.next();
                        result.push('%');
                        continue;
                    }
                    _ => {}
                }
            }
        }
        result.push(ch);
    }
    // Collapse multiple spaces.
    let mut prev_space = false;
    result
        .chars()
        .filter(|&c| {
            if c == ' ' {
                if prev_space {
                    return false;
                }
                prev_space = true;
            } else {
                prev_space = false;
            }
            true
        })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_codes() {
        assert_eq!(
            strip_field_codes("/usr/bin/chromium %U"),
            "/usr/bin/chromium"
        );
        assert_eq!(strip_field_codes("app %f --flag"), "app --flag");
        assert_eq!(strip_field_codes("app %%escaped"), "app %escaped");
    }
}
