use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use serde::Deserialize;
use anyhow::Result;
use gpui::{AsyncApp, Task};
use crate::backend::{EventSink, Workspace, WorkspaceBackend, WorkspaceEvent, WorkspaceId, WorkspaceState};

#[derive(Deserialize)]
struct RawWorkspace {
    name: String,
    focused: bool,
    urgent: bool,
}

pub fn parse_get_workspaces(raw: &str) -> Result<WorkspaceState> {
    let raws: Vec<RawWorkspace> = serde_json::from_str(raw)?;
    let mut active = None;
    let workspaces: Vec<Workspace> = raws.into_iter().map(|r| {
        let id = WorkspaceId(r.name.clone());
        if r.focused { active = Some(id.clone()); }
        Workspace {
            id,
            name: r.name,
            active: r.focused,
            urgent: r.urgent,
        }
    }).collect();
    Ok(WorkspaceState { workspaces, active })
}

pub struct WorkspaceChange {
    pub new_active: Option<WorkspaceId>,
}

#[derive(Deserialize)]
struct RawEvent {
    change: String,
    current: Option<RawWorkspace>,
}

pub fn parse_workspace_event(raw: &str) -> Result<WorkspaceChange> {
    let ev: RawEvent = serde_json::from_str(raw)?;
    let new_active = if ev.change == "focus" {
        ev.current.map(|w| WorkspaceId(w.name))
    } else {
        None
    };
    Ok(WorkspaceChange { new_active })
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

    /// Clones the underlying stream so a second handle can issue commands while
    /// the main handle is blocked on read.
    pub fn try_clone(&self) -> anyhow::Result<SwayConn> {
        Ok(SwayConn { stream: self.stream.try_clone()? })
    }
}

const MSG_RUN_COMMAND: u32 = 0;
const MSG_GET_WORKSPACES: u32 = 1;
const MSG_SUBSCRIBE: u32 = 2;
const EVENT_WORKSPACE: u32 = 0x80000000;

pub struct SwayBackend {
    /// Cloned write-side stream used by `activate` from any thread.
    cmd_conn: Arc<Mutex<Option<SwayConn>>>,
}

impl SwayBackend {
    pub fn new() -> Self {
        SwayBackend { cmd_conn: Arc::new(Mutex::new(None)) }
    }
}

impl WorkspaceBackend for SwayBackend {
    fn run(self: Box<Self>, sink: EventSink, cx: &mut AsyncApp) -> Task<()> {
        let cmd_conn = self.cmd_conn.clone();
        cx.background_executor().spawn(async move {
            loop {
                match run_session(&cmd_conn, &sink) {
                    Ok(()) => log::info!("sway session ended cleanly"),
                    Err(e) => log::warn!("sway session error: {e:#}"),
                }
                let _ = sink.send(WorkspaceEvent::Disconnected);
                // Reconnection backoff handled in Task 11. For now, exit on error.
                break;
            }
        })
    }

    fn activate(&self, id: &WorkspaceId) {
        let mut guard = self.cmd_conn.lock().unwrap();
        let Some(conn) = guard.as_mut() else {
            log::warn!("activate: no sway connection");
            return;
        };
        let cmd = format!("workspace {}", id.0);
        if let Err(e) = conn.send(MSG_RUN_COMMAND, cmd.as_bytes()) {
            log::warn!("activate: send failed: {e:#}");
        }
        // Drain the reply so we don't desync the stream.
        if let Err(e) = conn.read_message() {
            log::warn!("activate: read reply failed: {e:#}");
        }
    }
}

fn run_session(
    cmd_conn: &Arc<Mutex<Option<SwayConn>>>,
    sink: &EventSink,
) -> anyhow::Result<()> {
    let mut conn = SwayConn::connect()?;
    let cmd = conn.try_clone()?;
    *cmd_conn.lock().unwrap() = Some(cmd);

    // 1. Subscribe
    conn.send(MSG_SUBSCRIBE, br#"["workspace"]"#)?;
    let (_t, _payload) = conn.read_message()?; // subscription ack

    // 2. Initial snapshot
    conn.send(MSG_GET_WORKSPACES, b"")?;
    let (_t, payload) = conn.read_message()?;
    let state = parse_get_workspaces(std::str::from_utf8(&payload)?)?;
    sink.send(WorkspaceEvent::Snapshot(state))?;

    // 3. Event loop — refetch snapshot on every workspace event for simplicity
    loop {
        let (msg_type, _payload) = conn.read_message()?;
        if msg_type == EVENT_WORKSPACE {
            // Re-fetch full state instead of applying deltas. Simpler, fewer bugs.
            // Use a fresh command connection to avoid mid-event recursion.
            let mut snap_conn = SwayConn::connect()?;
            snap_conn.send(MSG_GET_WORKSPACES, b"")?;
            let (_t, payload) = snap_conn.read_message()?;
            let state = parse_get_workspaces(std::str::from_utf8(&payload)?)?;
            sink.send(WorkspaceEvent::Snapshot(state))?;
        }
    }
}
