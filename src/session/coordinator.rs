//! Routes each CLI invocation to exactly one of two roles:
//!
//! * **Owner** — acquired the instance lock; creates the control socket, runs a
//!   session body and keeps the socket responsive until cleanup completes.
//! * **Client** — the lock was held; reaches the owner over the control socket
//!   and delivers a command (Stop for `toggle`/`stop`, Status for `status`).
//!
//! Retries cover the §6 races: a client before the socket is ready, during
//! shutdown, or after an owner crashed mid-initialisation.

use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::cli::Command;

use super::body::{Cancellation, SessionBody, SessionContext};
use super::ipc::{self, ClientError, ClientOptions, ControlServer};
use super::lock::InstanceLock;
use super::paths::RuntimePaths;
use super::protocol::{Command as WireCommand, Response};
use super::{SessionError, SessionState, TeardownStep};

/// Retry policy used while looking for a live owner.
#[derive(Debug, Clone)]
pub struct ClientRetry {
    /// How many times to (re)try lock acquisition.
    pub lock_attempts: usize,
    /// Pause before re-trying lock acquisition after the owner vanished.
    pub lock_backoff: Duration,
    /// Retry policy for connecting to the control socket.
    pub connect: ClientOptions,
}

impl Default for ClientRetry {
    fn default() -> Self {
        Self {
            lock_attempts: 5,
            lock_backoff: Duration::from_millis(20),
            connect: ClientOptions::bounded(),
        }
    }
}

/// Observable result of routing a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// This invocation owned and ran a session to completion.
    SessionRan,
    /// A control command was delivered to an existing owner.
    Controlled { response: Response },
    /// Nothing was active; `stop` (or a takeover) had nothing to do.
    Idle,
    /// A status report.
    Status(SessionState),
}

impl CommandOutcome {
    /// Human-readable line printed by the CLI.
    pub fn report(&self) -> String {
        match self {
            Self::SessionRan => "Session completed.".to_owned(),
            Self::Controlled { response } => match response {
                Response::Ok { state } => format!("Session is now {state}."),
                Response::Error { message } => format!("Control command failed: {message}"),
            },
            Self::Idle => "No active session.".to_owned(),
            Self::Status(state) => format!("Session status: {state}"),
        }
    }
}

/// Route `command`, running `body` only if this invocation becomes the owner.
pub fn run<B: SessionBody>(
    paths: &RuntimePaths,
    command: Command,
    body: B,
    retry: &ClientRetry,
) -> Result<CommandOutcome, SessionError> {
    for attempt in 0..retry.lock_attempts {
        match InstanceLock::try_acquire(&paths.lock)? {
            Some(guard) => return handle_idle(paths, command, guard, body),
            None => match ipc::send_command(&paths.socket, client_command(command), &retry.connect)
            {
                Ok(ack) => return Ok(outcome_from_ack(command, ack.response)),
                Err(ClientError::Unavailable) => {
                    // The owner may have exited or not created its socket yet.
                    // Re-trying the lock lets us take over within a bounded time.
                    tracing::debug!(attempt, "owner unreachable; retrying lock acquisition");
                    thread::sleep(retry.lock_backoff);
                }
                Err(err) => return Err(SessionError::Client(err)),
            },
        }
    }

    Err(SessionError::Unavailable {
        detail: format!(
            "no session owner became reachable after {} attempts",
            retry.lock_attempts
        ),
    })
}

/// An invocation that acquired the lock: it is the owner of an idle session.
fn handle_idle<B: SessionBody>(
    paths: &RuntimePaths,
    command: Command,
    guard: InstanceLock,
    body: B,
) -> Result<CommandOutcome, SessionError> {
    match command {
        // Starting a session needs the body. `guard` is kept alive for the
        // whole run by `become_owner`.
        Command::Toggle | Command::ReadSelection | Command::TestVoice => {
            become_owner(paths, guard, body)?;
            Ok(CommandOutcome::SessionRan)
        }
        // `guard` is released as this function returns: nothing was started.
        Command::Stop => Ok(CommandOutcome::Idle),
        Command::Status => Ok(CommandOutcome::Status(SessionState::Inactive)),
    }
}

/// Run a session while holding the lock and serving control commands.
fn become_owner<B: SessionBody>(
    paths: &RuntimePaths,
    guard: InstanceLock,
    mut body: B,
) -> Result<(), SessionError> {
    // Only the lock holder may remove/replace the socket. A stale socket from
    // a crashed owner is safe to remove at this point.
    remove_stale_socket(&paths.socket)?;

    let listener = UnixListener::bind(&paths.socket).map_err(|err| SessionError::Io {
        detail: format!(
            "cannot bind control socket {}: {err}",
            paths.socket.display()
        ),
    })?;

    let cancel = Cancellation::new();
    let state = Arc::new(Mutex::new(SessionState::Starting));
    let context = SessionContext {
        cancel: cancel.clone(),
        state: state.clone(),
    };
    let mut server = ControlServer::spawn(listener, state, cancel.clone());

    // The body may fail (e.g. an empty selection). Cleanup must still run
    // while the lock is held, so defer propagating the result until after it.
    let result = body.run(&context);

    // Teardown order (spec §10): the body has already stopped accepting audio,
    // aborted the HTTP request, terminated the selection subprocess and
    // released the audio device. Only now may the socket close and the lock go.
    cancel.cancel();
    server.shutdown();
    remove_owned_socket(&paths.socket);
    body.record_teardown(TeardownStep::ControlSocketClosed);
    drop(guard); // release the lock only after all resources are gone
    body.record_teardown(TeardownStep::InstanceLockReleased);

    result
}

/// Remove a stale socket file left by a crashed owner. Refuse to touch any
/// non-socket path so we never delete an unrelated file.
fn remove_stale_socket(path: &Path) -> Result<(), SessionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(path).map_err(|err| SessionError::Io {
                detail: format!("cannot remove stale socket {}: {err}", path.display()),
            })
        }
        Ok(_) => Err(SessionError::Io {
            detail: format!("control path {} exists and is not a socket", path.display()),
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SessionError::Io {
            detail: err.to_string(),
        }),
    }
}

/// Best-effort removal of the socket this owner created. We still hold the
/// lock, so no other process could have replaced it.
fn remove_owned_socket(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_socket()
    {
        let _ = fs::remove_file(path);
    }
}

/// The wire command a control client sends to an existing owner.
fn client_command(command: Command) -> WireCommand {
    match command {
        // A second `toggle` cancels the current session (spec §4).
        Command::Toggle | Command::Stop => WireCommand::Stop,
        Command::ReadSelection | Command::TestVoice => WireCommand::ReplaceSelection,
        Command::Status => WireCommand::Status,
    }
}

fn outcome_from_ack(command: Command, response: Response) -> CommandOutcome {
    match command {
        Command::Status => match response {
            Response::Ok { state } => CommandOutcome::Status(state),
            other => CommandOutcome::Controlled { response: other },
        },
        _ => CommandOutcome::Controlled { response },
    }
}
