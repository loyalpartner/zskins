use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use serde::Deserialize;
use anyhow::Result;
use crate::backend::{Workspace, WorkspaceId, WorkspaceState};

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
