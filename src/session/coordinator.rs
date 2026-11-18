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

use serde::Serialize;

use crate::cli::Command;

use super::body::{Cancellation, NoopBody, SessionBody, SessionContext};
use super::control::{ReadingKind, SessionControl};
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

#[derive(Serialize)]
struct StatusReport {
    state: SessionState,
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

    /// Machine-readable report for `status --json`. The state is emitted with
    /// the same PascalCase names used by the control protocol, so the wire and
    /// CLI views of a lifecycle state cannot drift apart.
    pub fn report_json(&self) -> String {
        match self {
            Self::Status(state) => serde_json::to_string(&StatusReport { state: *state })
                .expect("status report is serializable"),
            other => serde_json::json!({ "result": other.report() }).to_string(),
        }
    }
}

/// Route `command`, running `body` only if this invocation becomes the owner.
///
/// This convenience form has a single body: a replacement command stops the
/// current reading and the owner exits rather than starting a second one.
/// Callers that support replacement use [`run_with_factory`].
pub fn run<B: SessionBody + 'static>(
    paths: &RuntimePaths,
    command: Command,
    body: B,
    retry: &ClientRetry,
) -> Result<CommandOutcome, SessionError> {
    let mut body = Some(body);
    let mut factory = move |_kind: ReadingKind| -> Box<dyn SessionBody> {
        match body.take() {
            Some(body) => Box::new(body),
            None => Box::new(NoopBody),
        }
    };
    run_with_factory(paths, command, &mut factory, retry)
}

/// Route `command`, creating a fresh body for every reading the owner runs.
///
/// The owner keeps the same lock, control socket and lifecycle state while it
/// replaces readings; each body gets a new cancellation token so a replacement
/// is never born cancelled (spec §4, §11).
pub fn run_with_factory<F, B>(
    paths: &RuntimePaths,
    command: Command,
    mut factory: F,
    retry: &ClientRetry,
) -> Result<CommandOutcome, SessionError>
where
    F: FnMut(ReadingKind) -> B,
    B: SessionBody,
{
    for attempt in 0..retry.lock_attempts {
        match InstanceLock::try_acquire(&paths.lock)? {
            Some(guard) => return handle_idle(paths, command, guard, &mut factory),
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
fn handle_idle<F, B>(
    paths: &RuntimePaths,
    command: Command,
    guard: InstanceLock,
    factory: &mut F,
) -> Result<CommandOutcome, SessionError>
where
    F: FnMut(ReadingKind) -> B,
    B: SessionBody,
{
    match command {
        // Starting a session needs the body. `guard` is kept alive for the
        // whole run by `become_owner`.
        Command::Toggle | Command::ReadSelection | Command::TestVoice => {
            let kind = match command {
                Command::TestVoice => ReadingKind::TestVoice,
                _ => ReadingKind::Selection,
            };
            become_owner(paths, guard, factory, kind)?;
            Ok(CommandOutcome::SessionRan)
        }
        // `guard` is released as this function returns: nothing was started.
        Command::Stop => Ok(CommandOutcome::Idle),
        Command::Status { .. } => Ok(CommandOutcome::Status(SessionState::Inactive)),
    }
}

/// Run a session while holding the lock and serving control commands.
///
/// A replacement stops the current body (reusing its ordered teardown), then
/// this loop starts the next reading with a fresh cancellation. `Stop` and
/// errors end the session; the lock and socket survive replacements.
fn become_owner<F, B>(
    paths: &RuntimePaths,
    guard: InstanceLock,
    factory: &mut F,
    initial_kind: ReadingKind,
) -> Result<(), SessionError>
where
    F: FnMut(ReadingKind) -> B,
    B: SessionBody,
{
    // Only the lock holder may remove/replace the socket. A stale socket from
    // a crashed owner is safe to remove at this point.
    remove_stale_socket(&paths.socket)?;

    let listener = UnixListener::bind(&paths.socket).map_err(|err| SessionError::Io {
        detail: format!(
            "cannot bind control socket {}: {err}",
            paths.socket.display()
        ),
    })?;

    let state = Arc::new(Mutex::new(SessionState::Starting));
    let control = SessionControl::new();
    let mut server =
        ControlServer::spawn_with_control(listener, Arc::clone(&state), control.clone());

    let mut kind = initial_kind;
    let mut first_error: Option<SessionError> = None;
    let mut last_body: Option<B> = None;
    loop {
        if control.stop_requested() {
            break;
        }

        // Consume a request that arrived before this reading started, so one
        // arriving *during* it is seen on the next iteration.
        control.clear_replace();
        let attempt = Cancellation::new();
        control.set_attempt(attempt.clone());
        if control.stop_requested() {
            attempt.cancel();
        }
        {
            let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
            *guard = SessionState::Starting;
        }

        let context = SessionContext {
            cancel: attempt,
            state: Arc::clone(&state),
        };
        let mut body = factory(kind);
        let result = body.run(&context);
        let failed = result.is_err();
        if first_error.is_none() {
            first_error = result.err();
        }
        last_body = Some(body);

        if failed || control.stop_requested() || !control.replace_requested() {
            break;
        }
        kind = control.replacement_kind();
    }

    // Teardown order (spec §10): the body has already stopped accepting audio,
    // aborted the HTTP request, terminated the selection subprocess and
    // released the audio device. Only now may the socket close and the lock go.
    control.stop();
    server.shutdown();
    remove_owned_socket(&paths.socket);
    if let Some(body) = &last_body {
        body.record_teardown(TeardownStep::ControlSocketClosed);
    }
    drop(guard); // release the lock only after all resources are gone
    if let Some(body) = &last_body {
        body.record_teardown(TeardownStep::InstanceLockReleased);
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
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
        Command::ReadSelection => WireCommand::ReplaceSelection,
        Command::TestVoice => WireCommand::ReplaceTestVoice,
        Command::Status { .. } => WireCommand::Status,
    }
}

fn outcome_from_ack(command: Command, response: Response) -> CommandOutcome {
    match command {
        Command::Status { .. } => match response {
            Response::Ok { state } => CommandOutcome::Status(state),
            other => CommandOutcome::Controlled { response: other },
        },
        _ => CommandOutcome::Controlled { response },
    }
}
