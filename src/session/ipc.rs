//! Unix-socket control transport: the owner's control server and the control
//! client. Command/response *shapes* live in [`super::protocol`]; this module
//! only moves their newline-delimited frames between processes.

use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::SessionState;
use super::body::Cancellation;
use super::control::SessionControl;
use super::protocol::{self, Command, ProtocolError, Response, ResponseEnvelope};
use super::routing;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Retry policy for a control client.
///
/// A client may run before the owner has finished creating its socket, so it
/// retries connecting briefly. Defaults are intentionally small.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Total number of connection/send attempts.
    pub attempts: usize,
    /// Pause between attempts.
    pub backoff: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            attempts: 30,
            backoff: Duration::from_millis(10),
        }
    }
}

/// Problems a control client can hit.
#[derive(Debug)]
pub enum ClientError {
    /// No listener was reachable within the retry budget.
    Unavailable,
    /// Reading/writing the control stream failed.
    Io { detail: String },
    /// The peer sent an invalid frame.
    Protocol(ProtocolError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(f, "no active session owner is reachable"),
            Self::Io { detail } => write!(f, "control socket I/O error: {detail}"),
            Self::Protocol(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Handle to the owner's control-serving thread.
///
/// Dropping it asks the thread to stop and joins it, so cleanup is deterministic.
pub struct ControlServerHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ControlServerHandle {
    /// Ask the server loop to stop and wait for it to exit.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ControlServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Owner-side control server.
pub struct ControlServer;

impl ControlServer {
    /// Serve control commands from `listener` until the returned handle is
    /// shut down, using `cancel` as the whole session's cancellation. Kept as
    /// the simple-caller seam (used by the IPC tests); the owner, which supports
    /// replacement, uses [`Self::spawn_with_control`]. Stop and replacement both
    /// signal the supplied cancellation.
    pub fn spawn(
        listener: UnixListener,
        state: Arc<Mutex<SessionState>>,
        cancel: Cancellation,
    ) -> ControlServerHandle {
        Self::spawn_with_control(listener, state, SessionControl::from_cancel(cancel))
    }

    /// Serve control commands against a session-level [`SessionControl`].
    /// `state` is shared with the running body for `Status` replies.
    pub fn spawn_with_control(
        listener: UnixListener,
        state: Arc<Mutex<SessionState>>,
        control: SessionControl,
    ) -> ControlServerHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let loop_shutdown = shutdown.clone();
        let thread = thread::spawn(move || serve(listener, state, control, loop_shutdown));
        ControlServerHandle {
            shutdown,
            thread: Some(thread),
        }
    }
}

fn serve(
    listener: UnixListener,
    state: Arc<Mutex<SessionState>>,
    control: SessionControl,
    shutdown: Arc<AtomicBool>,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        tracing::warn!(error = %err, "cannot make control listener non-blocking");
        return;
    }

    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &state, &control),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(err) => {
                tracing::warn!(error = %err, "control listener stopped accepting");
                break;
            }
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    state: &Arc<Mutex<SessionState>>,
    control: &SessionControl,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut reader = match stream.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(err) => {
            tracing::warn!(error = %err, "cannot clone control stream");
            return;
        }
    };
    let mut writer = stream;

    let mut line = Vec::new();
    if let Err(err) = reader.read_until(b'\n', &mut line) {
        tracing::warn!(error = %err, "cannot read control command");
        return;
    }

    let response = match protocol::decode_command(&line) {
        Ok(envelope) => match routing::replacement_kind(envelope.command) {
            Some(kind) => {
                // Interrupt the current reading immediately; the owner loop
                // then starts a fresh one before this socket is closed. If it
                // has already begun shutdown, decline explicitly (§6) instead
                // of acknowledging a replacement that would be dropped.
                if control.request_replace(kind) {
                    let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
                    *guard = SessionState::Stopping;
                    Response::Ok {
                        state: SessionState::Stopping,
                    }
                } else {
                    Response::Error {
                        message: "session is shutting down; replacement declined".to_owned(),
                    }
                }
            }
            None => match envelope.command {
                Command::Stop => {
                    control.stop();
                    let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
                    *guard = SessionState::Stopping;
                    Response::Ok {
                        state: SessionState::Stopping,
                    }
                }
                Command::Status => Response::Ok {
                    state: *state.lock().unwrap_or_else(|err| err.into_inner()),
                },
                // `replacement_kind` covers both replacement commands.
                Command::ReplaceSelection | Command::ReplaceTestVoice => unreachable!(),
            },
        },
        Err(err) => Response::Error {
            message: err.to_string(),
        },
    };

    let frame = protocol::encode_response(response);
    if let Err(err) = writer
        .write_all(frame.as_bytes())
        .and_then(|_| writer.flush())
    {
        tracing::warn!(error = %err, "cannot write control response");
    }
}

/// Send one command to the owner listening at `socket_path`, retrying briefly
/// while it initializes, and return its acknowledgement.
pub fn send_command(
    socket_path: &Path,
    command: Command,
    options: &ClientOptions,
) -> Result<ResponseEnvelope, ClientError> {
    let frame = protocol::encode_command(command);
    let mut last_error = ClientError::Unavailable;

    for attempt in 0..options.attempts {
        match send_once(socket_path, frame.as_bytes()) {
            Ok(ack) => return Ok(ack),
            Err(err) => {
                tracing::debug!(
                    attempt,
                    path = %socket_path.display(),
                    error = %err,
                    "control command attempt failed"
                );
                last_error = err;
                if attempt + 1 < options.attempts {
                    thread::sleep(options.backoff);
                }
            }
        }
    }

    Err(last_error)
}

fn send_once(socket_path: &Path, frame: &[u8]) -> Result<ResponseEnvelope, ClientError> {
    // A missing/stale socket is reported as Unavailable so the caller can
    // retry lock acquisition rather than treating it as a fatal I/O error.
    let mut stream = UnixStream::connect(socket_path).map_err(|_| ClientError::Unavailable)?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    stream
        .write_all(frame)
        .and_then(|_| stream.flush())
        .map_err(|err| ClientError::Io {
            detail: err.to_string(),
        })?;

    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|err| ClientError::Io {
            detail: err.to_string(),
        })?;

    protocol::decode_response(&line).map_err(ClientError::Protocol)
}
