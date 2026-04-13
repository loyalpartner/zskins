use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};

/// Exclusive process lock on the daemon's pid file. Held for the lifetime of
/// `Self`; the lock is released when this is dropped or the process exits.
pub struct DaemonLock {
    _flock: Flock<File>,
}

impl DaemonLock {
    /// Take the lock and write our pid. Errors if another instance holds it.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open {path:?}"))?;

        let mut flock =
            Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| {
                anyhow::anyhow!("another zofi-clipd already holds {path:?}: {e}")
            })?;

        // Truncate + write our pid.
        let pid = std::process::id();
        (*flock).set_len(0).ok();
        (*flock).seek(SeekFrom::Start(0)).ok();
        write!(&mut *flock, "{pid}")?;
        flock.flush()?;

        Ok(Self { _flock: flock })
    }
}

/// Probe whether a daemon is running by reading the pid file and `kill(pid, 0)`.
pub fn probe(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return false;
    }
    let Ok(pid) = buf.trim().parse::<i32>() else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}
