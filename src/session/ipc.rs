//! Unix-socket control transport: the owner's control server and the control
//! client. Command/response *shapes* live in [`super::protocol`]; this module
//! only moves their newline-delimited frames between processes.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::SessionState;
use super::body::Cancellation;
use super::control::SessionControl;
use super::protocol::{self, Command, ProtocolError, Response, ResponseEnvelope};
use super::routing;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Client-side I/O budget while waiting for the owner's acknowledgement.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Absolute deadline for one server-side frame, measured from connection time.
/// A per-read inactivity timeout is not enough: a client that trickles a byte
/// every few milliseconds could otherwise hold a worker indefinitely.
const FRAME_DEADLINE: Duration = Duration::from_millis(750);
/// Frame budget for short-lived overflow workers used once every regular worker
/// is busy. A stalled client is dropped after this long, but on its *own* thread
/// — the accept loop never waits for a frame (review T2).
const PRIORITY_WAIT: Duration = Duration::from_millis(20);
/// Maximum bytes in one newline-delimited control frame. A client that sends
/// more (without a newline) is dropped rather than allowed to grow memory.
const MAX_FRAME_BYTES: usize = 4096;
/// Upper bound on long-lived workers serving frames with the full deadline.
const MAX_WORKERS: usize = 8;
/// Upper bound on short-lived overflow workers. Once both pools are full, new
/// connections are rejected immediately without reading a frame, so a flood of
/// slow clients cannot grow threads without bound or stall the accept loop.
const OVERFLOW_WORKERS: usize = 128;

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
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ControlServerHandle {
    /// Ask the server loop to stop and wait for it and its workers to exit.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Ok(mut workers) = self.workers.lock() {
            for worker in workers.drain(..) {
                let _ = worker.join();
            }
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
        let loop_shutdown = Arc::clone(&shutdown);
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let loop_workers = Arc::clone(&workers);
        let thread = thread::spawn(move || {
            serve(listener, state, control, loop_shutdown, loop_workers);
        });
        ControlServerHandle {
            shutdown,
            thread: Some(thread),
            workers,
        }
    }
}

fn serve(
    listener: UnixListener,
    state: Arc<Mutex<SessionState>>,
    control: SessionControl,
    shutdown: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        tracing::warn!(error = %err, "cannot make control listener non-blocking");
        return;
    }

    // Live worker counts. Framing (which can block on a slow client) always
    // happens on a worker thread; the accept loop only accepts, picks a bounded
    // pool, and spawns. A queue of stalled clients therefore cannot serialize
    // into a hotkey delay (review T2).
    let regular = Arc::new(AtomicUsize::new(0));
    let overflow = Arc::new(AtomicUsize::new(0));

    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (pool, budget) = if regular.load(Ordering::Acquire) < MAX_WORKERS {
                    (&regular, FRAME_DEADLINE)
                } else if overflow.load(Ordering::Acquire) < OVERFLOW_WORKERS {
                    // At capacity, frame this connection on a short-lived
                    // overflow worker rather than on the only accept thread.
                    (&overflow, PRIORITY_WAIT)
                } else {
                    // Bounded admission: reject without reading a frame at all.
                    tracing::debug!("control connection rejected: server saturated");
                    drop(stream);
                    continue;
                };
                let slot = Arc::clone(pool);
                slot.fetch_add(1, Ordering::AcqRel);
                let worker_state = Arc::clone(&state);
                let worker_control = control.clone();
                let handle = thread::spawn(move || {
                    handle_connection(stream, &worker_state, &worker_control, budget);
                    slot.fetch_sub(1, Ordering::AcqRel);
                });
                // The handle is kept only so `shutdown` can join it; the live
                // count above decides admission. Recover from a poisoned mutex
                // rather than dropping the handle on the floor.
                match workers.lock() {
                    Ok(mut active) => active.push(handle),
                    Err(err) => err.into_inner().push(handle),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(err) => {
                tracing::warn!(error = %err, "control listener stopped accepting");
                break;
            }
        }
    }
}

/// Read one frame and apply it, with a deadline measured from connection time.
///
/// `budget` is the full [`FRAME_DEADLINE`] for a regular worker and the much
/// shorter [`PRIORITY_WAIT`] for an overflow worker. Either way the framing runs
/// on a worker, never on the accept loop.
fn handle_connection(
    mut stream: UnixStream,
    state: &Arc<Mutex<SessionState>>,
    control: &SessionControl,
    budget: Duration,
) {
    // An absolute deadline, refreshed only as a remaining budget for each read,
    // bounds a client even when it keeps trickling bytes that reset a plain
    // inactivity timeout.
    let deadline = Instant::now() + budget;
    let mut line = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!("control frame deadline exceeded; dropping client");
            return;
        }
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Read at most one byte beyond the cap: a longer frame without
                // a newline is oversized and dropped instead of allocating.
                line.extend_from_slice(&buf[..n]);
                if line.len() > MAX_FRAME_BYTES {
                    tracing::warn!(bytes = line.len(), "oversized control frame dropped");
                    return;
                }
                if line.contains(&b'\n') {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot read control command");
                return;
            }
        }
    }

    write_response(&mut stream, dispatch_frame(&line, state, control));
}

/// Decode one complete frame and apply it to the shared session state. Shared
/// by regular and overflow workers so both behave identically.
fn dispatch_frame(
    line: &[u8],
    state: &Arc<Mutex<SessionState>>,
    control: &SessionControl,
) -> Response {
    match protocol::decode_command(line) {
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
    }
}

fn write_response(stream: &mut UnixStream, response: Response) {
    let frame = protocol::encode_response(response);
    let _ = stream.set_write_timeout(Some(FRAME_DEADLINE));
    if let Err(err) = stream
        .write_all(frame.as_bytes())
        .and_then(|_| stream.flush())
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
    (&mut reader)
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|err| ClientError::Io {
            detail: err.to_string(),
        })?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(ClientError::Protocol(ProtocolError::Malformed(
            "control response frame is too large".to_owned(),
        )));
    }

    protocol::decode_response(&line).map_err(ClientError::Protocol)
}
