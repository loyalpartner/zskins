//! Minimal sway IPC client to enumerate window geometries by output.
//!
//! Pulled in deliberately rather than reused from `zbar`'s richer Sway backend
//! to keep `zwindows` a self-contained protocol crate. The wire format
//! (`i3-ipc` framing, GET_TREE = msg type 4) is small and stable; duplicating
//! ~80 lines is preferable to crate cross-dependencies for a one-shot use.
//!
//! Returns a flat `Vec<WindowGeom>` because consumers don't care about the
//! tree shape — they index by `(app_id, title)` or by output to look up a
//! per-window screen rect for cropping.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum SwayTreeError {
    #[error("SWAYSOCK env var missing")]
    NoSocket,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad magic in sway message header")]
    BadMagic,
    #[error("json parse: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid utf-8 in sway payload: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

/// One visible toplevel from the sway tree.
///
/// `output_name` is the wl_output `name` (e.g. `"DP-1"`) so callers can pair
/// the rect against the matching screencopy buffer. `rect` is in absolute
/// compositor-pixel coords, same coord space sway reports for outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowGeom {
    pub app_id: String,
    pub title: String,
    pub output_name: String,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

const MAGIC: &[u8; 6] = b"i3-ipc";
const MSG_GET_TREE: u32 = 4;

/// Connect, send GET_TREE, parse the response, and return all visible
/// toplevels. Callers in non-sway environments get [`SwayTreeError::NoSocket`]
/// and are expected to skip preview generation gracefully.
pub fn fetch_windows() -> Result<Vec<WindowGeom>, SwayTreeError> {
    let path = std::env::var("SWAYSOCK").map_err(|_| SwayTreeError::NoSocket)?;
    let mut stream = UnixStream::connect(path)?;
    write_message(&mut stream, MSG_GET_TREE, b"")?;
    let (_ty, payload) = read_message(&mut stream)?;
    let raw: RawNode = serde_json::from_slice(&payload)?;
    let mut out = Vec::new();
    walk(&raw, None, &mut out);
    Ok(out)
}

fn write_message(stream: &mut UnixStream, msg_type: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(14 + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&msg_type.to_le_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf)
}

fn read_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), SwayTreeError> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header)?;
    if &header[0..6] != MAGIC {
        return Err(SwayTreeError::BadMagic);
    }
    let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let msg_type = u32::from_le_bytes(header[10..14].try_into().unwrap());
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((msg_type, payload))
}

#[derive(Deserialize)]
struct RawRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    node_type: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    visible: Option<bool>,
    #[serde(default)]
    rect: Option<RawRect>,
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default)]
    floating_nodes: Vec<RawNode>,
    #[serde(default)]
    window_properties: Option<WindowProperties>,
}

#[derive(Deserialize)]
struct WindowProperties {
    #[serde(default)]
    class: Option<String>,
}

/// Recurse into the tree, picking up "con" or "floating_con" nodes that look
/// like real windows. We track the enclosing output name so we can stamp it on
/// each leaf — the tree itself only attaches it at the output level.
fn walk(node: &RawNode, mut output: Option<String>, out: &mut Vec<WindowGeom>) {
    if node.node_type.as_deref() == Some("output") {
        output = node.name.clone();
    }
    let is_window = matches!(
        node.node_type.as_deref(),
        Some("con") | Some("floating_con")
    ) && node.app_id.is_some()
        || node
            .window_properties
            .as_ref()
            .and_then(|w| w.class.as_ref())
            .is_some();
    // Filter "scratchpad" / hidden via `visible == false`. sway populates
    // `visible: true` for the focused workspace's windows; tabbed/stacked
    // siblings have `visible: false` and aren't on screen, so we'd never
    // capture them anyway.
    let visible = node.visible.unwrap_or(false);
    if is_window && visible {
        if let (Some(rect), Some(out_name)) = (&node.rect, output.as_ref()) {
            let app_id = node.app_id.clone().unwrap_or_else(|| {
                node.window_properties
                    .as_ref()
                    .and_then(|w| w.class.clone())
                    .unwrap_or_default()
            });
            out.push(WindowGeom {
                app_id,
                title: node.name.clone().unwrap_or_default(),
                output_name: out_name.clone(),
                rect: Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            });
        }
    }
    for child in node.nodes.iter().chain(node.floating_nodes.iter()) {
        walk(child, output.clone(), out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal tree shape mirroring real sway output: root → output → workspace
    /// → con. This exercises the recursion and the per-output name stamping.
    const SAMPLE_TREE: &str = r#"
    {
        "type": "root",
        "name": "root",
        "rect": {"x":0,"y":0,"width":3840,"height":1080},
        "nodes": [{
            "type": "output",
            "name": "DP-1",
            "rect": {"x":0,"y":0,"width":1920,"height":1080},
            "nodes": [{
                "type": "workspace",
                "name": "1",
                "rect": {"x":0,"y":0,"width":1920,"height":1080},
                "nodes": [{
                    "type": "con",
                    "name": "Issues — zskins",
                    "app_id": "firefox",
                    "visible": true,
                    "rect": {"x":10,"y":20,"width":1900,"height":1040}
                }]
            }],
            "floating_nodes": []
        }, {
            "type": "output",
            "name": "DP-2",
            "rect": {"x":1920,"y":0,"width":1920,"height":1080},
            "nodes": [{
                "type": "workspace",
                "name": "2",
                "rect": {"x":1920,"y":0,"width":1920,"height":1080},
                "nodes": [{
                    "type": "con",
                    "name": "main.rs",
                    "app_id": "kitty",
                    "visible": true,
                    "rect": {"x":1930,"y":40,"width":900,"height":600}
                }, {
                    "type": "con",
                    "name": "hidden",
                    "app_id": "alacritty",
                    "visible": false,
                    "rect": {"x":1930,"y":40,"width":900,"height":600}
                }]
            }]
        }]
    }
    "#;

    #[test]
    fn parse_sample_tree_extracts_visible_windows_only() {
        let raw: RawNode = serde_json::from_str(SAMPLE_TREE).unwrap();
        let mut out = Vec::new();
        walk(&raw, None, &mut out);
        assert_eq!(out.len(), 2, "should skip the visible=false con");
        assert_eq!(out[0].app_id, "firefox");
        assert_eq!(out[0].title, "Issues — zskins");
        assert_eq!(out[0].output_name, "DP-1");
        assert_eq!(out[0].rect.width, 1900);
        assert_eq!(out[1].app_id, "kitty");
        assert_eq!(out[1].output_name, "DP-2");
    }

    #[test]
    fn parse_falls_back_to_window_properties_class_for_xwayland() {
        let raw: RawNode = serde_json::from_str(
            r#"{
                "type":"output","name":"HDMI-A-1",
                "rect":{"x":0,"y":0,"width":1920,"height":1080},
                "nodes":[{"type":"workspace","name":"3",
                  "rect":{"x":0,"y":0,"width":1920,"height":1080},
                  "nodes":[{"type":"con","name":"GIMP",
                    "visible":true,
                    "rect":{"x":0,"y":0,"width":800,"height":600},
                    "window_properties":{"class":"Gimp"}}]}]
            }"#,
        )
        .unwrap();
        let mut out = Vec::new();
        walk(&raw, None, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].app_id, "Gimp");
    }

    #[test]
    fn parse_skips_workspace_and_root_nodes() {
        // Only leaf containers should appear in the output, not workspace or
        // output containers (they have rects too but aren't windows).
        let raw: RawNode = serde_json::from_str(SAMPLE_TREE).unwrap();
        let mut out = Vec::new();
        walk(&raw, None, &mut out);
        for w in &out {
            assert!(!w.app_id.is_empty(), "windows must have an app_id");
        }
    }
}
