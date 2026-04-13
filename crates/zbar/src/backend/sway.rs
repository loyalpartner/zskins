use crate::backend::{
    EventSink, Workspace, WorkspaceBackend, WorkspaceEvent, WorkspaceId, WorkspaceState,
};
use anyhow::Result;
use gpui::{AsyncApp, Task};
use serde::Deserialize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Deserialize)]
struct RawWorkspace {
    name: String,
    focused: bool,
    urgent: bool,
}

pub fn parse_get_workspaces(raw: &str) -> Result<WorkspaceState> {
    let raws: Vec<RawWorkspace> = serde_json::from_str(raw)?;
    let mut active = None;
    let workspaces: Vec<Workspace> = raws
        .into_iter()
        .map(|r| {
            let id = WorkspaceId(r.name.clone());
            if r.focused {
                active = Some(id.clone());
            }
            Workspace {
                id,
                name: r.name,
                active: r.focused,
                urgent: r.urgent,
            }
        })
        .collect();
    Ok(WorkspaceState { workspaces, active })
}

#[derive(Deserialize)]
struct RawEvent {
    change: String,
    current: Option<RawWorkspace>,
}

enum EventAction {
    Focus(WorkspaceId),
    Refetch,
    Ignore,
}

fn classify_event(raw: &str) -> Result<EventAction> {
    let ev: RawEvent = serde_json::from_str(raw)?;
    match ev.change.as_str() {
        "focus" => Ok(match ev.current {
            Some(w) => EventAction::Focus(WorkspaceId(w.name)),
            None => EventAction::Ignore,
        }),
        "init" | "empty" | "move" | "rename" | "reload" => Ok(EventAction::Refetch),
        "urgent" => Ok(EventAction::Refetch),
        _ => Ok(EventAction::Ignore),
    }
}

const MAGIC: &[u8; 6] = b"i3-ipc";

pub fn encode_message(msg_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&msg_type.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub struct SwayConn {
    stream: UnixStream,
}

impl SwayConn {
    pub fn connect() -> anyhow::Result<Self> {
        let path = std::env::var("SWAYSOCK")?;
        let stream = UnixStream::connect(path)?;
        Ok(SwayConn { stream })
    }

    pub fn send(&mut self, msg_type: u32, payload: &[u8]) -> anyhow::Result<()> {
        self.stream.write_all(&encode_message(msg_type, payload))?;
        Ok(())
    }

    pub fn read_message(&mut self) -> anyhow::Result<(u32, Vec<u8>)> {
        let mut header = [0u8; 14];
        self.stream.read_exact(&mut header)?;
        anyhow::ensure!(&header[0..6] == MAGIC, "bad magic");
        let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        let msg_type = u32::from_le_bytes(header[10..14].try_into().unwrap());
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload)?;
        Ok((msg_type, payload))
    }
}

const MSG_RUN_COMMAND: u32 = 0;
const MSG_GET_WORKSPACES: u32 = 1;
const MSG_SUBSCRIBE: u32 = 2;
const MSG_GET_OUTPUTS: u32 = 3;
const EVENT_WORKSPACE: u32 = 0x80000000;

#[derive(Deserialize)]
struct RawOutputRect {
    width: f64,
}

#[derive(Deserialize)]
struct RawOutput {
    name: String,
    rect: RawOutputRect,
}

/// Query sway for output rects, returning a list of (name, logical_width).
pub fn query_output_widths() -> Vec<(String, f32)> {
    let Ok(mut conn) = SwayConn::connect() else {
        return Vec::new();
    };
    if conn.send(MSG_GET_OUTPUTS, b"").is_err() {
        return Vec::new();
    }
    let Ok((_ty, payload)) = conn.read_message() else {
        return Vec::new();
    };
    let Ok(outputs) = serde_json::from_slice::<Vec<RawOutput>>(&payload) else {
        return Vec::new();
    };
    outputs
        .into_iter()
        .map(|o| (o.name, o.rect.width as f32))
        .collect()
}

#[derive(Default)]
pub struct SwayBackend;

impl WorkspaceBackend for SwayBackend {
    fn run(&self, sink: EventSink, cx: &mut AsyncApp) -> Task<()> {
        cx.background_executor().spawn(async move {
            let mut delay_ms: u64 = 1000;
            loop {
                match run_session(&sink) {
                    Ok(()) => tracing::info!("sway session ended cleanly"),
                    Err(e) => {
                        tracing::warn!("sway session error: {e:#}; reconnecting in {delay_ms}ms")
                    }
                }
                let _ = sink.send_blocking(WorkspaceEvent::Disconnected);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(30_000);
            }
        })
    }

    fn activate(&self, id: &WorkspaceId) {
        let cmd = format!("workspace {}", id.0);
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut conn = SwayConn::connect()?;
                conn.send(MSG_RUN_COMMAND, cmd.as_bytes())?;
                let (_msg_type, payload) = conn.read_message()?;
                tracing::debug!(cmd = %cmd, payload = ?payload, "activate response");
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!("activate failed: {e:#}");
            }
        });
    }
}

const EVENT_WINDOW: u32 = 0x80000003;

#[derive(Deserialize)]
struct RawWindowEvent {
    change: String,
    container: Option<RawContainer>,
}

#[derive(Deserialize)]
struct RawContainer {
    name: Option<String>,
    focused: bool,
}

pub fn parse_window_event(raw: &str) -> anyhow::Result<Option<String>> {
    let ev: RawWindowEvent = serde_json::from_str(raw)?;
    if ev.change == "focus" || ev.change == "title" {
        Ok(ev
            .container
            .and_then(|c| if c.focused { c.name } else { None }))
    } else {
        Ok(None)
    }
}

pub fn run_window_title_session(sink: async_channel::Sender<Option<String>>) -> anyhow::Result<()> {
    let mut conn = SwayConn::connect()?;
    conn.send(MSG_SUBSCRIBE, br#"["window"]"#)?;
    let _ = conn.read_message()?;
    loop {
        let (msg_type, payload) = conn.read_message()?;
        if msg_type == EVENT_WINDOW {
            if let Some(title) = parse_window_event(std::str::from_utf8(&payload)?)? {
                sink.send_blocking(Some(title))?;
            }
        }
    }
}

fn fetch_workspaces(cmd: &mut SwayConn) -> anyhow::Result<WorkspaceState> {
    cmd.send(MSG_GET_WORKSPACES, b"")?;
    let (_t, payload) = cmd.read_message()?;
    parse_get_workspaces(std::str::from_utf8(&payload)?)
}

fn run_session(sink: &EventSink) -> anyhow::Result<()> {
    let mut sub = SwayConn::connect()?;
    let mut cmd = SwayConn::connect()?;

    sub.send(MSG_SUBSCRIBE, br#"["workspace"]"#)?;
    let _ = sub.read_message()?;

    let state = fetch_workspaces(&mut cmd)?;
    sink.send_blocking(WorkspaceEvent::Snapshot(state))?;

    loop {
        let (msg_type, payload) = sub.read_message()?;
        if msg_type == EVENT_WORKSPACE {
            let raw = std::str::from_utf8(&payload)?;
            match classify_event(raw)? {
                EventAction::Focus(id) => {
                    sink.send_blocking(WorkspaceEvent::Focus(id))?;
                }
                EventAction::Refetch => {
                    let state = fetch_workspaces(&mut cmd)?;
                    sink.send_blocking(WorkspaceEvent::Snapshot(state))?;
                }
                EventAction::Ignore => {}
            }
        }
    }
}
