//! Exclusive, non-blocking instance lock backed by `flock`.
//!
//! The lock file is an anchor for the kernel lock, not a PID file: it is
//! created if missing and is never deleted. The lock is released automatically
//! when the guard is dropped (including on process exit), so a crashed owner
//! cannot leave a permanently stuck session.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// Problems acquiring the instance lock.
#[derive(Debug)]
pub enum LockError {
    /// The lock file or its parent directory could not be prepared/opened.
    Open { detail: String },
    /// `flock` failed for a reason other than "already held".
    Flock { detail: String },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { detail } => write!(f, "cannot open instance lock: {detail}"),
            Self::Flock { detail } => write!(f, "cannot lock instance: {detail}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Guard holding the exclusive instance lock.
///
/// The lock is held for as long as this value lives. Dropping it releases the
/// lock without touching the lock file.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Try to acquire the exclusive, non-blocking lock anchored at `path`.
    ///
    /// Returns `Ok(None)` when another process (or open file description) holds
    /// the lock — the caller should then act as a control client.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>, LockError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| LockError::Open {
                detail: err.to_string(),
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|err| LockError::Open {
                detail: err.to_string(),
            })?;

        // SAFETY: `file` is a valid, open file descriptor for the whole call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            // Another invocation owns the session.
            Ok(None)
        } else {
            Err(LockError::Flock {
                detail: err.to_string(),
            })
        }
    }
}
