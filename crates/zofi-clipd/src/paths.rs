use std::path::PathBuf;

use anyhow::{Context, Result};

/// `$XDG_DATA_HOME/zofi/clipboard.db` (defaults to `~/.local/share/zofi/`).
pub fn db_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(base.join("zofi").join("clipboard.db"))
}

/// `$XDG_RUNTIME_DIR/zofi-clipd.pid`. Falls back to `/tmp` so the file is at
/// least addressable, but flock semantics across users degrade — XDG path is
/// strongly preferred.
pub fn pid_path() -> PathBuf {
    runtime_dir().join("zofi-clipd.pid")
}

/// `$XDG_RUNTIME_DIR/zofi-clipd.sock`. Used for zofi → daemon IPC.
pub fn sock_path() -> PathBuf {
    runtime_dir().join("zofi-clipd.sock")
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
