//! Ticket #7 acceptance tests: cancellation in every session stage.
//!
//! Everything runs through public seams with fakes only: a fake TTS server
//! that can stall a request, a fake selection source, and `AudioSink::Fake`
//! so no Wayland display or audible device is needed.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::audio::{AudioSink, StreamConfig};
use murmduck::cancellation::Cancellation;
use murmduck::cli::Command;
use murmduck::config::{SelectionConfig, TtsConfig};
use murmduck::selection::{self, SelectionError, SelectionSource, WaylandPrimary};
use murmduck::session::TeardownStep;
use murmduck::session::body::{ReadAloudSession, SessionReport};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::ipc::ClientOptions;
use murmduck::session::lock::InstanceLock;
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{SessionError, SessionState};
use murmduck::tts::{TtsClient, TtsError};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.mp3");

fn fixture() -> Vec<u8> {
    std::fs::read(FIXTURE).expect("read MP3 fixture")
}

fn wait_until(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    condition()
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeSource {
    text: Result<String, SelectionError>,
}

impl FakeSource {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: Ok(text.into()),
        }
    }
}

impl SelectionSource for FakeSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        self.text.clone()
    }
}

/// A TTS server that accepts a connection and then stalls: either before
/// sending any response (Connecting) or after the first audio chunk
/// (Buffering/Playing). It records when the client disconnects so tests can
/// prove the HTTP request was actually aborted rather than merely abandoned.
struct StallServer {
    addr: SocketAddr,
    connected: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StallServer {
    fn spawn(send_first_chunk: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
        let addr = listener.local_addr().expect("local addr");
        let connected = Arc::new(AtomicBool::new(false));
        let disconnected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (c, d) = (connected.clone(), disconnected.clone());
        let stop = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking stall listener");
            let mut stream = loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("read timeout");
            // Drain the request headers.
            let mut scratch = [0u8; 4096];
            let mut buf = Vec::new();
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut scratch) {
                    Ok(0) | Err(_) => {
                        d.store(true, Ordering::Release);
                        return;
                    }
                    Ok(n) => buf.extend_from_slice(&scratch[..n]),
                }
            }
            c.store(true, Ordering::Release);

            if send_first_chunk {
                let bytes = fixture();
                let midpoint = bytes.len() / 2;
                let first = &bytes[..midpoint];
                let head = b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\n\r\n";
                if stream.write_all(head).is_err()
                    || stream
                        .write_all(format!("{:X}\r\n", first.len()).as_bytes())
                        .is_err()
                    || stream.write_all(first).is_err()
                    || stream.write_all(b"\r\n").is_err()
                    || stream.flush().is_err()
                {
                    d.store(true, Ordering::Release);
                    return;
                }
            }

            // Stall until the client closes the connection.
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match stream.read(&mut scratch) {
                    Ok(0) => {
                        d.store(true, Ordering::Release);
                        return;
                    }
                    Ok(_) => continue,
                    Err(ref err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => {
                        d.store(true, Ordering::Release);
                        return;
                    }
                }
            }
        });
        Self {
            addr,
            connected,
            disconnected,
            shutdown,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn join(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for StallServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn tts_config(server: &StallServer) -> TtsConfig {
    TtsConfig {
        base_url: server.base_url(),
        ..TtsConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Slice 1: the HTTP request is aborted promptly while connecting / buffering.
// ---------------------------------------------------------------------------

#[test]
fn cancelling_while_connecting_aborts_the_http_request_promptly() {
    let server = StallServer::spawn(false);
    let client = TtsClient::new(&tts_config(&server), None).expect("build TTS client");
    let cancel = Cancellation::new();

    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        let mut sink = Vec::new();
        client.speak_to_cancellable("some selected text", &mut sink, &worker_cancel)
    });

    assert!(
        wait_until(
            || server.connected.load(Ordering::Acquire),
            Duration::from_secs(5)
        ),
        "server never saw the connection"
    );
    cancel.cancel();

    let result = worker.join().expect("request thread joins");
    assert!(
        matches!(result, Err(TtsError::Cancelled)),
        "connecting request must report cancellation: {result:?}"
    );
    assert!(
        wait_until(
            || server.disconnected.load(Ordering::Acquire),
            Duration::from_secs(2)
        ),
        "the HTTP connection must be closed on cancellation"
    );
    server.join();
}

#[test]
fn cancelling_during_selection_terminates_the_subprocess_promptly() {
    let cancel = Cancellation::new();
    // A single long-running process so killing it leaves no orphan children.
    let source = WaylandPrimary::with_command("sleep", &["30"], Duration::from_secs(30));
    let config = SelectionConfig {
        timeout_ms: 30_000,
        ..SelectionConfig::default()
    };
    let timeout_ms = config.timeout_ms;

    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        selection::read_selection_cancellable(&source, &config, &worker_cancel)
    });

    // Let the subprocess actually start before asking for cancellation.
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    cancel.cancel();

    let result = worker.join().expect("selection reader joins");
    assert!(
        matches!(result, Err(SelectionError::Cancelled)),
        "selection read must report cancellation: {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the selection subprocess must be terminated promptly, not after its {timeout_ms} ms timeout"
    );
}

#[test]
fn cancelling_while_buffering_aborts_the_http_request_promptly() {
    let server = StallServer::spawn(true);
    let client = TtsClient::new(&tts_config(&server), None).expect("build TTS client");
    let cancel = Cancellation::new();

    let worker_cancel = cancel.clone();
    let worker = thread::spawn(move || {
        let mut sink = Vec::new();
        client.speak_to_cancellable("some selected text", &mut sink, &worker_cancel)
    });

    assert!(
        wait_until(
            || server.connected.load(Ordering::Acquire),
            Duration::from_secs(5)
        ),
        "server never saw the connection"
    );
    cancel.cancel();

    let result = worker.join().expect("request thread joins");
    assert!(
        matches!(result, Err(TtsError::Cancelled)),
        "buffering request must report cancellation: {result:?}"
    );
    assert!(
        wait_until(
            || server.disconnected.load(Ordering::Acquire),
            Duration::from_secs(2)
        ),
        "the HTTP connection must be closed on cancellation"
    );
    server.join();
}

// ---------------------------------------------------------------------------
// A server that streams the whole fixture slowly, keeping the pipeline in the
// Playing stage long enough for a control command to arrive.
// ---------------------------------------------------------------------------

struct StreamingServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamingServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind streaming server");
        let addr = listener.local_addr().expect("local addr");
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking streaming listener");
            while !stop.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return,
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .expect("read timeout");
                let mut scratch = [0u8; 4096];
                let mut buf = Vec::new();
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut scratch) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&scratch[..n]),
                    }
                }
                let bytes = fixture();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\n\r\n",
                );
                for chunk in bytes.chunks(1024) {
                    if stop.load(Ordering::Acquire)
                        || stream
                            .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                            .is_err()
                        || stream.write_all(chunk).is_err()
                        || stream.write_all(b"\r\n").is_err()
                        || stream.flush().is_err()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
            }
        });
        Self {
            addr,
            shutdown,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn join(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for StreamingServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Minimal stand-in body for control commands.
struct NoopBodyForTest;

impl murmduck::session::body::SessionBody for NoopBodyForTest {
    fn run(&mut self, _ctx: &murmduck::session::body::SessionContext) -> Result<(), SessionError> {
        Ok(())
    }
}

fn body_for(
    base_url: &str,
    sink: AudioSink,
    report: Arc<Mutex<SessionReport>>,
) -> ReadAloudSession<FakeSource> {
    ReadAloudSession::new(
        FakeSource::new("selected text for cancellation tests"),
        SelectionConfig::default(),
        TtsConfig {
            base_url: base_url.to_owned(),
            ..TtsConfig::default()
        },
        None,
        sink,
    )
    .with_stream_config(StreamConfig {
        max_bytes: 2048,
        ..StreamConfig::default()
    })
    .with_report(report)
}

fn retry() -> ClientRetry {
    ClientRetry {
        lock_attempts: 5,
        lock_backoff: Duration::from_millis(5),
        connect: ClientOptions {
            attempts: 20,
            backoff: Duration::from_millis(5),
        },
    }
}

fn generous_retry() -> ClientRetry {
    ClientRetry {
        lock_attempts: 40,
        lock_backoff: Duration::from_millis(5),
        connect: ClientOptions {
            attempts: 60,
            backoff: Duration::from_millis(5),
        },
    }
}

fn paths(dir: &tempfile::TempDir) -> RuntimePaths {
    RuntimePaths::new(dir.path().join("murmduck"))
}

fn start_owner(
    runtime: RuntimePaths,
    body: ReadAloudSession<FakeSource>,
    policy: ClientRetry,
) -> thread::JoinHandle<Result<CommandOutcome, SessionError>> {
    thread::spawn(move || coordinator::run(&runtime, Command::Toggle, body, &policy))
}

fn expected_teardown() -> Vec<TeardownStep> {
    vec![
        TeardownStep::StoppedAcceptingAudio,
        TeardownStep::HttpRequestCancelled,
        TeardownStep::AudioPipelineStopped,
        TeardownStep::ControlSocketClosed,
        TeardownStep::InstanceLockReleased,
    ]
}

// ---------------------------------------------------------------------------
// Slice 3/4: full-session cancellation through the coordinator.
// ---------------------------------------------------------------------------

#[test]
fn toggle_while_connecting_cancels_and_releases_resources_in_order() {
    let server = StallServer::spawn(false);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = start_owner(
        runtime.clone(),
        body_for(&server.base_url(), AudioSink::Fake, Arc::clone(&report)),
        retry(),
    );

    let reached_connecting = wait_until(
        || {
            server.connected.load(Ordering::Acquire)
                && report
                    .lock()
                    .expect("report lock")
                    .states
                    .contains(&SessionState::Connecting)
        },
        Duration::from_secs(5),
    );
    assert!(
        reached_connecting,
        "the TTS request never reached Connecting"
    );

    let client = coordinator::run(&runtime, Command::Toggle, NoopBodyForTest, &retry())
        .expect("second toggle delivers a stop");
    assert!(
        matches!(client, CommandOutcome::Controlled { .. }),
        "{client:?}"
    );

    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(
        snapshot.teardown,
        expected_teardown(),
        "teardown must happen in the spec's order"
    );
    assert_eq!(
        snapshot.states.last(),
        Some(&SessionState::Stopping),
        "session ends in Stopping: {:?}",
        snapshot.states
    );
    assert!(
        server.disconnected.load(Ordering::Acquire),
        "the stalled HTTP request must be aborted"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after cancellation"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after cancellation"
    );
    server.join();
}

#[test]
fn toggle_while_buffering_cancels_streaming_and_stops_the_pipeline() {
    let server = StallServer::spawn(true);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = start_owner(
        runtime.clone(),
        body_for(&server.base_url(), AudioSink::Fake, Arc::clone(&report)),
        retry(),
    );

    let reached = wait_until(
        || {
            let states = &report.lock().expect("report lock").states;
            states.contains(&SessionState::Buffering) || states.contains(&SessionState::Playing)
        },
        Duration::from_secs(5),
    );
    assert!(reached, "session never began buffering audio");

    let _ = coordinator::run(&runtime, Command::Toggle, NoopBodyForTest, &retry())
        .expect("second toggle delivers a stop");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.teardown, expected_teardown());
    assert!(
        snapshot.audio_torn_down,
        "pipeline and sink must be Null after cancellation"
    );
    assert_eq!(snapshot.active_players, 0, "no pipeline left running");
    assert!(
        server.disconnected.load(Ordering::Acquire),
        "the stalled HTTP request must be aborted"
    );
    server.join();
}

#[test]
fn stop_while_playing_stops_audio_immediately_and_the_device_is_released() {
    let server = StreamingServer::spawn();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = start_owner(
        runtime.clone(),
        body_for(
            &server.base_url(),
            AudioSink::SyncedFake,
            Arc::clone(&report),
        ),
        generous_retry(),
    );

    let reached_playing = wait_until(
        || {
            report
                .lock()
                .expect("report lock")
                .states
                .contains(&SessionState::Playing)
        },
        Duration::from_secs(5),
    );
    assert!(reached_playing, "session never reached Playing");

    let started = Instant::now();
    let ack = coordinator::run(&runtime, Command::Stop, NoopBodyForTest, &generous_retry())
        .expect("stop reaches the owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "playback must stop promptly, took {:?}",
        started.elapsed()
    );

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.teardown, expected_teardown());
    assert!(
        snapshot.audio_torn_down,
        "the audio device must be released on stop"
    );

    // A fresh session starts immediately: nothing is holding the device.
    let next_report = Arc::new(Mutex::new(SessionReport::default()));
    let next = coordinator::run(
        &runtime,
        Command::Toggle,
        body_for(
            &server.base_url(),
            AudioSink::Fake,
            Arc::clone(&next_report),
        ),
        &generous_retry(),
    )
    .expect("a new session can start after cancellation");
    assert!(matches!(next, CommandOutcome::SessionRan), "{next:?}");
    assert!(
        next_report.lock().expect("report lock").decoded_bytes > 0,
        "the new session produced audio"
    );
    server.join();
}

#[test]
fn rapid_repeated_toggles_never_overlap_audio() {
    let server = StreamingServer::spawn();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let body = body_for(
                &server.base_url(),
                AudioSink::SyncedFake,
                Arc::clone(&report),
            );
            start_owner(runtime.clone(), body, generous_retry())
        })
        .collect();

    for handle in handles {
        handle
            .join()
            .expect("invocation does not panic")
            .expect("invocation succeeds");
    }

    let snapshot = report.lock().expect("report lock").clone();
    assert!(
        snapshot.peak_active_players <= 1,
        "audio pipelines overlapped: peak={}",
        snapshot.peak_active_players
    );
    assert_eq!(snapshot.active_players, 0, "no pipeline left running");
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after the last session"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after the last session"
    );
    server.join();
}

#[test]
fn cancellation_during_audio_pipeline_initialisation_is_safe() {
    let server = StallServer::spawn(false);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));

    // Cancel deterministically between pipeline construction and `start()`.
    // The body must tear down without panicking or leaking a half-built
    // pipeline, and must never send a TTS request after cancellation.
    let hook_ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&hook_ran);
    let owner = start_owner(
        runtime.clone(),
        body_for(&server.base_url(), AudioSink::Fake, Arc::clone(&report)).with_init_hook(
            move |ctx: &murmduck::session::body::SessionContext| {
                ctx.cancel.cancel();
                flag.store(true, Ordering::Release);
            },
        ),
        retry(),
    );

    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(
        hook_ran.load(Ordering::Acquire),
        "the init hook must have fired"
    );

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(
        snapshot.states.last(),
        Some(&SessionState::Stopping),
        "cancelled init ends in Stopping: {:?}",
        snapshot.states
    );
    assert_eq!(snapshot.active_players, 0, "no half-built pipeline left");
    assert!(
        snapshot.audio_torn_down,
        "the pipeline must be Null after cancelled init"
    );
    assert_eq!(
        snapshot.teardown,
        expected_teardown(),
        "cancelled init follows the same ordered teardown"
    );
    assert!(
        !server.connected.load(Ordering::Acquire),
        "no TTS request is sent after cancellation during init"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after cancelled init"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after cancelled init"
    );
    server.join();
}
