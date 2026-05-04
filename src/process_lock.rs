//! Advisory file lock held by any indexer process for its lifetime.
//!
//! Lets us answer "is something already indexing?" without process-
//! manager coupling -- works the same for the launchd agent, a
//! foreground `mailwise index` in another terminal, and any future
//! entry point. flock is released by the kernel on process exit
//! (including crashes), so stale state is impossible.
//!
//! `config` consults [`is_held`] before destructive ops; the indexer
//! holds [`Lock`] for the lifetime of `cmd_index`.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

fn lock_path() -> Result<PathBuf> {
    Ok(crate::settings::mailwise_dir()?.join("indexer.lock"))
}

/// RAII guard. Drop releases the flock; the lockfile itself stays on
/// disk (zero-byte, harmless). Racing `unlink` with concurrent
/// acquirers would let two processes both think they own the lock.
pub struct Lock {
    _file: File,
}

/// Non-blocking acquire. `Ok(None)` means another process already holds
/// the lock and the caller should bail with a helpful message rather
/// than wait.
pub fn try_acquire() -> Result<Option<Lock>> {
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(Lock { _file: file }))
    } else {
        let err = io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            Ok(None)
        } else {
            Err(anyhow::anyhow!("flock {}: {err}", path.display()))
        }
    }
}

/// Check whether some other process is currently indexing. Equivalent to
/// `try_acquire().is_none()`, but releases the lock immediately so the
/// caller doesn't accidentally hold it.
pub fn is_held() -> Result<bool> {
    Ok(try_acquire()?.is_none())
}
