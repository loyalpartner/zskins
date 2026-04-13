//! Newline-delimited JSON over a Unix socket. One request per connection,
//! one response, then close.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("connect to zofi-clipd socket (is the daemon running?): {0}")]
    Connect(#[source] std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Touch the item's `last_used_at` and become the wayland selection
    /// holder serving its content. `mime` selects which representation to
    /// serve; `None` falls back to the item's `primary_mime`.
    Activate { uuid: String, mime: Option<String> },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
}

pub fn send(req: &Request) -> Result<Response, IpcError> {
    let mut stream = UnixStream::connect(paths::sock_path()).map_err(IpcError::Connect)?;
    let line = serde_json::to_string(req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let resp: Response = serde_json::from_str(buf.trim())?;
    Ok(resp)
}

pub fn parse_request(line: &str) -> Result<Request, IpcError> {
    Ok(serde_json::from_str(line.trim())?)
}

pub fn write_response(stream: &mut UnixStream, resp: &Response) -> Result<(), IpcError> {
    let line = serde_json::to_string(resp)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}
