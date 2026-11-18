//! Session coordination: instance locking, control IPC and ownership routing.
//!
//! Exactly one invocation becomes the session owner; all others are control
//! clients. The owner holds the exclusive lock for the whole session and keeps
//! a temporary control socket responsive while a session body runs.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::audio::AudioError;
use crate::selection::SelectionError;
use crate::tts::TtsError;

pub mod body;
pub mod control;
pub mod coordinator;
pub mod ipc;
pub mod lock;
pub mod paths;
pub mod protocol;
pub mod routing;

pub use control::{ReadingKind, SessionControl};

/// Errors raised while coordinating a session or driving its control socket.
#[derive(Debug)]
pub enum SessionError {
    /// The XDG runtime directory could not be determined.
    Runtime { detail: String },
    /// The instance lock could not be acquired or prepared.
    Lock(lock::LockError),
    /// A socket or filesystem operation failed.
    Io { detail: String },
    /// A control-protocol frame was invalid.
    Protocol(protocol::ProtocolError),
    /// A control client could not reach the owner.
    Client(ipc::ClientError),
    /// The owner refused a control command (`Response::Error`).
    Control { detail: String },
    /// The PRIMARY selection could not be read or validated.
    Selection(SelectionError),
    /// The TTS request failed (network, HTTP status or stream error).
    Tts(TtsError),
    /// The audio pipeline failed while starting, feeding or finishing playback.
    Audio(AudioError),
    /// Bounded retries were exhausted while looking for a live owner.
    Unavailable { detail: String },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime { detail } => write!(f, "session runtime error: {detail}"),
            Self::Lock(err) => write!(f, "{err}"),
            Self::Io { detail } => write!(f, "session I/O error: {detail}"),
            Self::Protocol(err) => write!(f, "{err}"),
            Self::Client(err) => write!(f, "{err}"),
            Self::Control { detail } => write!(f, "control command failed: {detail}"),
            Self::Selection(err) => write!(f, "{err}"),
            Self::Tts(err) => write!(f, "{err}"),
            Self::Audio(err) => write!(f, "{err}"),
            Self::Unavailable { detail } => write!(f, "{detail}"),
        }
    }
}

impl Error for SessionError {}

impl From<lock::LockError> for SessionError {
    fn from(err: lock::LockError) -> Self {
        Self::Lock(err)
    }
}

impl From<protocol::ProtocolError> for SessionError {
    fn from(err: protocol::ProtocolError) -> Self {
        Self::Protocol(err)
    }
}

impl From<ipc::ClientError> for SessionError {
    fn from(err: ipc::ClientError) -> Self {
        Self::Client(err)
    }
}

impl From<SelectionError> for SessionError {
    fn from(err: SelectionError) -> Self {
        Self::Selection(err)
    }
}

impl From<TtsError> for SessionError {
    fn from(err: TtsError) -> Self {
        Self::Tts(err)
    }
}

impl From<AudioError> for SessionError {
    fn from(err: AudioError) -> Self {
        Self::Audio(err)
    }
}

/// Observable lifecycle state of a reading session (spec §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SessionState {
    Inactive,
    Starting,
    Connecting,
    Buffering,
    Playing,
    Stopping,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Inactive => "Inactive",
            Self::Starting => "Starting",
            Self::Connecting => "Connecting",
            Self::Buffering => "Buffering",
            Self::Playing => "Playing",
            Self::Stopping => "Stopping",
        };
        f.write_str(name)
    }
}

/// Ordered steps of a session teardown (spec §10). The coordinator records
/// the last two; the body records the rest. Exposed so tests can assert that
/// resources are released in the required order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownStep {
    StoppedAcceptingAudio,
    HttpRequestCancelled,
    SelectionProcessTerminated,
    AudioPipelineStopped,
    ControlSocketClosed,
    InstanceLockReleased,
}
