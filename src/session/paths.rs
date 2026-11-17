//! Runtime file locations for the single-instance lock and control socket.
//!
//! Both live under `$XDG_RUNTIME_DIR/murmduck/`, which is user-private on the
//! target platform. Paths are injected into the coordinator in tests so no
//! test ever touches the real runtime directory.

use std::path::PathBuf;

use super::SessionError;

/// The runtime directory and the two files the owner manages inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub dir: PathBuf,
    pub lock: PathBuf,
    pub socket: PathBuf,
}

impl RuntimePaths {
    /// Build paths rooted at `dir` (used by tests and for explicit overrides).
    pub fn new(dir: PathBuf) -> Self {
        let lock = dir.join("instance.lock");
        let socket = dir.join("control.sock");
        Self { dir, lock, socket }
    }

    /// Resolve paths from `XDG_RUNTIME_DIR`.
    pub fn from_env() -> Result<Self, SessionError> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty());
        match runtime {
            Some(runtime) => Ok(Self::new(PathBuf::from(runtime).join("murmduck"))),
            None => Err(SessionError::Runtime {
                detail: "XDG_RUNTIME_DIR is not set or is empty".to_owned(),
            }),
        }
    }
}
