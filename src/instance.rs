//! Single-instance guard. Two copies racing each other's mount operations
//! would be unsafe, so a second launch exits quietly.

use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;

use crate::error::{Context, Error, Result};

/// Holds the lock for as long as it is alive (the kernel drops it on exit).
pub struct InstanceLock {
    _file: File,
}

pub enum Acquire {
    Acquired(InstanceLock),
    AlreadyRunning,
}

pub fn acquire() -> Result<Acquire> {
    let dir = crate::paths::support_dir().context("Home directory not found")?;
    fs::create_dir_all(&dir).context(format!("Creating {}", dir.display()))?;
    let path = dir.join("instance.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .context(format!("Opening {}", path.display()))?;
    // SAFETY: flock on a valid, open file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Acquire::Acquired(InstanceLock { _file: file }));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(Acquire::AlreadyRunning)
    } else {
        Err(Error::new(format!("Locking {}: {err}", path.display())))
    }
}
