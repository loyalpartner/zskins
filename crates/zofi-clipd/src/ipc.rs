//! Length-prefixed bincode frames over a Unix socket. One request per
//! connection, one response, then close. bincode handles the framing for
//! `Vec<u8>` payloads (e.g. image bytes inside `SetSelection`) so we don't
//! need a separate header/payload split.

use std::io::Write;
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("connect to zofi-clipd socket (is the daemon running?): {0}")]
    Connect(#[source] std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Touch the item's `last_used_at` and become the wayland selection
    /// holder serving its content. `mime` selects which representation to
    /// serve; `None` falls back to the item's `primary_mime`.
    Activate { uuid: String, mime: Option<String> },

    /// Record `bytes` as a new clipboard entry of `mime`, then immediately
    /// hold it as the active selection. Lets ephemeral callers (the
    /// launcher's image copy) hand off ownership to the long-lived daemon
    /// and exit without losing the clipboard.
    SetSelection { mime: String, bytes: Vec<u8> },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error { message: String },
}

pub fn send(req: &Request) -> Result<Response, IpcError> {
    let mut stream = UnixStream::connect(paths::sock_path()).map_err(IpcError::Connect)?;
    bincode::serialize_into(&mut stream, req)?;
    stream.flush()?;
    let resp: Response = bincode::deserialize_from(&mut stream)?;
    Ok(resp)
}

pub fn read_request<R: std::io::Read>(reader: R) -> Result<Request, IpcError> {
    Ok(bincode::deserialize_from(reader)?)
}

pub fn write_response(stream: &mut UnixStream, resp: &Response) -> Result<(), IpcError> {
    bincode::serialize_into(&mut *stream, resp)?;
    stream.flush()?;
    Ok(())
}
