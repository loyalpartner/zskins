//! Hyprland backend.
//!
//! Hyprland exposes a UNIX socket per-instance under
//! `$XDG_RUNTIME_DIR/hypr/$HIS/.socket.sock` (older builds use
//! `/tmp/hypr/$HIS/.socket.sock`). The protocol is line-oriented: write a
//! command (`j/activewindow` for JSON), read until EOF, parse.
//!
//! We only need the active window's class+title, so we ignore the rest of
//! the response shape (workspace, geometry, pid, ...).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::CompositorIpc;

pub struct HyprlandIpc;

impl CompositorIpc for HyprlandIpc {
    fn focused_window(&self) -> Option<(String, String)> {
        let his = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let stream = open_socket(&his).ok()?;
        query_active_window(stream).ok()?
    }
}

fn open_socket(his: &str) -> std::io::Result<UnixStream> {
    // Try the modern XDG path first; fall back to the legacy /tmp location
    // for older Hyprland builds. The first one that connects wins.
    let candidates = candidate_paths(his);
    let mut last_err = None;
    for path in candidates {
        match UnixStream::connect(&path) {
            Ok(s) => {
                // Don't let a wedged compositor hang the launcher.
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                return Ok(s);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no hyprland socket candidates")))
}

fn candidate_paths(his: &str) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        out.push(PathBuf::from(xdg).join("hypr").join(his).join(".socket.sock"));
    }
    out.push(PathBuf::from("/tmp/hypr").join(his).join(".socket.sock"));
    out
}

fn query_active_window(mut stream: UnixStream) -> std::io::Result<Option<(String, String)>> {
    stream.write_all(b"j/activewindow")?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    // Hyprland returns an empty object `{}` when no window is focused —
    // serde will populate `class` and `title` as empty strings, which we
    // filter to None below.
    let parsed: ActiveWindow = serde_json::from_str(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if parsed.class.is_empty() && parsed.title.is_empty() {
        return Ok(None);
    }
    Ok(Some((parsed.class, parsed.title)))
}

#[derive(Deserialize, Default)]
struct ActiveWindow {
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_active_window_response() {
        let payload = r#"{"address":"0x55b","class":"firefox","title":"Issues — zskins","workspace":{"id":1}}"#;
        let parsed: ActiveWindow = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.class, "firefox");
        assert_eq!(parsed.title, "Issues — zskins");
    }

    #[test]
    fn empty_object_yields_blank_fields() {
        let parsed: ActiveWindow = serde_json::from_str("{}").unwrap();
        assert!(parsed.class.is_empty());
        assert!(parsed.title.is_empty());
    }

    #[test]
    fn extra_fields_are_ignored() {
        let payload = r#"{"class":"alacritty","title":"~","pid":42,"floating":true}"#;
        let parsed: ActiveWindow = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.class, "alacritty");
    }

    #[test]
    fn xdg_runtime_dir_path_takes_precedence() {
        // Save and restore XDG_RUNTIME_DIR so the test stays hermetic.
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let paths = candidate_paths("HIS-FAKE");
        assert_eq!(paths[0], PathBuf::from("/run/user/1000/hypr/HIS-FAKE/.socket.sock"));
        assert_eq!(paths[1], PathBuf::from("/tmp/hypr/HIS-FAKE/.socket.sock"));
        match prev {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}
