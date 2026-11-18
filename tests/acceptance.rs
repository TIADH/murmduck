//! Ticket #9 acceptance verification.
//!
//! Every criterion that can be checked on this headless host is exercised here
//! through public seams with fakes: an injectable selection source, a local
//! fake TTS server, `AudioSink::Fake`/`SyncedFake`, injected runtime paths and
//! a captured tracing writer. No Wayland session, `wl-paste` or audible device
//! is needed. Criteria that need the Fedora KDE Wayland target
//! are recorded in `docs/acceptance-report.md` instead of being faked here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::audio::{AudioSink, StreamConfig};
use murmduck::cli::Command as CliCommand;
use murmduck::config::{Config, SelectionConfig, TtsConfig};
use murmduck::logging;
use murmduck::selection::{self, StaticSource};
use murmduck::session::body::{NoopBody, ReadAloudSession, SessionReport};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::ipc::ClientOptions;
use murmduck::session::lock::InstanceLock;
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{SessionError, SessionState, TeardownStep};
use murmduck::tts::TtsError;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.mp3");
/// Exit code the crashed-owner helper process uses. The OS still releases its
/// flock and leaves its socket file behind, which is the scenario we verify.
const CRASH_EXIT_CODE: i32 = 70;
const CRASH_ENV: &str = "MURMDUCK_ACCEPTANCE_CRASH_OWNER_DIR";

fn fixture() -> Vec<u8> {
    std::fs::read(FIXTURE).expect("read MP3 fixture")
}

fn chunks(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
    bytes.chunks(size).map(<[u8]>::to_vec).collect()
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

fn paths(dir: &tempfile::TempDir) -> RuntimePaths {
    RuntimePaths::new(dir.path().join("murmduck"))
}

fn retry() -> ClientRetry {
    ClientRetry {
        lock_attempts: 40,
        lock_backoff: Duration::from_millis(5),
        connect: ClientOptions {
            attempts: 60,
            backoff: Duration::from_millis(5),
        },
    }
}

fn expected_cancel_teardown() -> Vec<TeardownStep> {
    vec![
        TeardownStep::StoppedAcceptingAudio,
        TeardownStep::HttpRequestCancelled,
        TeardownStep::AudioPipelineStopped,
        TeardownStep::ControlSocketClosed,
        TeardownStep::InstanceLockReleased,
    ]
}

fn assert_clean(runtime: &RuntimePaths) {
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "no lock may remain held"
    );
    assert!(!runtime.socket.exists(), "no control socket may remain");
}

// ---------------------------------------------------------------------------
// Fake TTS server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ServerConfig {
    chunks: Vec<Vec<u8>>,
    /// After sending `chunks`, hold the connection open instead of ending the
    /// HTTP stream. Models a slow/unavailable server and keeps the session
    /// alive long enough for a control command to arrive.
    stall: bool,
    chunk_delay: Duration,
}

impl ServerConfig {
    fn ok(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks,
            stall: false,
            chunk_delay: Duration::ZERO,
        }
    }

    fn stalled(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks,
            stall: true,
            chunk_delay: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone)]
struct ReceivedRequest {
    path: String,
    authorization: Option<String>,
    body: serde_json::Value,
}

struct FakeTtsServer {
    base_url: String,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    connected: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeTtsServer {
    fn spawn(config: ServerConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake TTS server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let disconnected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (r, c, d, s) = (
            requests.clone(),
            connected.clone(),
            disconnected.clone(),
            shutdown.clone(),
        );
        let thread = thread::spawn(move || serve(listener, config, r, c, d, s));
        Self {
            base_url,
            requests,
            connected,
            disconnected,
            shutdown,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("{}/v1", self.base_url)
    }

    fn requests(&self) -> Vec<ReceivedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    fn join(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeTtsServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: TcpListener,
    config: ServerConfig,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    connected: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let mut stream = loop {
        if shutdown.load(Ordering::Acquire) {
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

    let mut scratch = [0u8; 4096];
    let mut buf = Vec::new();
    let header_end = loop {
        match stream.read(&mut scratch) {
            Ok(0) => {
                disconnected.store(true, Ordering::Release);
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&scratch[..n]);
                if let Some(pos) = find(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(ref err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
        }
    }
    let body_end = (header_end + content_length).min(buf.len());
    let body =
        serde_json::from_slice(&buf[header_end..body_end]).unwrap_or(serde_json::Value::Null);
    let path = header
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_owned();
    let authorization = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    requests
        .lock()
        .expect("requests lock")
        .push(ReceivedRequest {
            path,
            authorization,
            body,
        });
    connected.store(true, Ordering::Release);

    let head = b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if stream.write_all(head).is_err() {
        return;
    }
    for chunk in &config.chunks {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let wrote = stream
            .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
            .and_then(|_| stream.write_all(chunk))
            .and_then(|_| stream.write_all(b"\r\n"))
            .and_then(|_| stream.flush());
        if wrote.is_err() {
            disconnected.store(true, Ordering::Release);
            return;
        }
        if !config.chunk_delay.is_zero() {
            thread::sleep(config.chunk_delay);
        }
    }
    if config.stall {
        // Hold the response open until the client disconnects or we shut down.
        // This is the "slow server" state the acceptance criteria care about.
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match stream.read(&mut scratch) {
                Ok(0) => {
                    disconnected.store(true, Ordering::Release);
                    return;
                }
                Ok(_) => {}
                Err(ref err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    disconnected.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn body_for(
    server: &FakeTtsServer,
    text: &str,
    selection: SelectionConfig,
    api_key: Option<String>,
    sink: AudioSink,
    report: Arc<Mutex<SessionReport>>,
) -> ReadAloudSession<StaticSource> {
    ReadAloudSession::new(
        StaticSource::new(text),
        selection,
        TtsConfig {
            base_url: server.base_url(),
            ..TtsConfig::default()
        },
        api_key,
        sink,
    )
    .with_stream_config(StreamConfig {
        max_bytes: 2048,
        ..StreamConfig::default()
    })
    .with_report(report)
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

#[test]
fn vietnamese_and_unicode_selection_is_transmitted_verbatim_and_read_aloud() {
    let text = "Tiếng Việt: Grüße, 日本語, 🎧 — đọc to thành tiếng.";
    let server = FakeTtsServer::spawn(ServerConfig::ok(chunks(&fixture(), 2048)));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let selection_config = SelectionConfig {
        max_chars: 200,
        ..SelectionConfig::default()
    };
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let body = body_for(
        &server,
        text,
        selection_config.clone(),
        None,
        AudioSink::Fake,
        Arc::clone(&report),
    );

    let outcome = coordinator::run(&runtime, CliCommand::Toggle, body, &retry())
        .expect("a Unicode selection reads to completion");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one TTS request");
    assert_eq!(requests[0].path, "/v1/audio/speech");
    assert_eq!(
        requests[0].body["input"], text,
        "UTF-8 text changed in transit"
    );
    let selected = selection::read_selection(&StaticSource::new(text), &selection_config)
        .expect("selection within limit");
    assert_eq!(selected.char_count(), text.chars().count());
    assert_eq!(selected.text(), text);

    let snapshot = report.lock().expect("report lock").clone();
    assert!(snapshot.decoded_bytes > 0, "audio reached the sink");
    assert_eq!(snapshot.states.last(), Some(&SessionState::Stopping));
    assert_clean(&runtime);
    server.join();
}

#[test]
fn the_character_limit_applies_to_unicode_text_before_any_tts_request() {
    // 11 characters, many more bytes: the limit must count chars, not bytes.
    let text = "Tiếng Việt!";
    assert_eq!(text.chars().count(), 11);
    assert!(text.len() > 11);
    let server = FakeTtsServer::spawn(ServerConfig::ok(chunks(&fixture(), 4096)));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let body = body_for(
        &server,
        text,
        SelectionConfig {
            max_chars: 10,
            ..SelectionConfig::default()
        },
        None,
        AudioSink::Fake,
        Arc::clone(&report),
    );

    let err = coordinator::run(&runtime, CliCommand::Toggle, body, &retry())
        .expect_err("an over-limit selection aborts the session");
    assert!(
        matches!(
            err,
            SessionError::Selection(selection::SelectionError::TooLong {
                max: 10,
                actual: 11
            })
        ),
        "{err:?}"
    );
    assert!(
        server.requests().is_empty(),
        "no TTS request may be sent for oversized text"
    );
    assert!(
        !err.to_string().contains("Tiếng"),
        "the error must not echo selected text: {err}"
    );
    assert_clean(&runtime);
    server.join();
}

#[test]
fn rapid_repeated_toggles_never_overlap_audio_or_leave_resources() {
    let server = FakeTtsServer::spawn(ServerConfig {
        chunks: chunks(&fixture(), 512),
        stall: false,
        chunk_delay: Duration::from_millis(15),
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let body = body_for(
                &server,
                "rapid repeated shortcut activation",
                SelectionConfig::default(),
                None,
                AudioSink::SyncedFake,
                Arc::clone(&report),
            );
            let runtime = runtime.clone();
            thread::spawn(move || coordinator::run(&runtime, CliCommand::Toggle, body, &retry()))
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
    assert_eq!(snapshot.active_players, 0, "a pipeline was left running");
    assert_clean(&runtime);
    server.join();
}

#[test]
fn an_unavailable_tts_server_surfaces_an_error_without_an_orphan() {
    let addr = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe addr")
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let body = ReadAloudSession::new(
        StaticSource::new("nobody is listening"),
        SelectionConfig::default(),
        TtsConfig {
            base_url: format!("http://{addr}/v1"),
            ..TtsConfig::default()
        },
        None,
        AudioSink::Fake,
    );

    let started = Instant::now();
    let err = coordinator::run(&runtime, CliCommand::Toggle, body, &retry())
        .expect_err("a refused connection aborts the session");
    assert!(
        matches!(err, SessionError::Tts(TtsError::Request { .. })),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "unavailable server must fail fast, took {:?}",
        started.elapsed()
    );
    assert_clean(&runtime);
}

#[test]
fn a_slow_server_stays_controllable_and_stop_tears_everything_down() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let server = FakeTtsServer::spawn(ServerConfig::stalled(vec![bytes[..midpoint].to_vec()]));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = {
        let body = body_for(
            &server,
            "slow server",
            SelectionConfig::default(),
            None,
            AudioSink::Fake,
            Arc::clone(&report),
        );
        let runtime = runtime.clone();
        thread::spawn(move || coordinator::run(&runtime, CliCommand::Toggle, body, &retry()))
    };

    assert!(
        wait_until(
            || server.connected()
                && report
                    .lock()
                    .expect("report lock")
                    .states
                    .iter()
                    .any(|state| matches!(
                        state,
                        SessionState::Connecting | SessionState::Buffering | SessionState::Playing
                    )),
            Duration::from_secs(5)
        ),
        "the stalled request never reached an active state"
    );

    let status = coordinator::run(
        &runtime,
        CliCommand::Status { json: false },
        NoopBody,
        &retry(),
    )
    .expect("status reaches the stalled owner");
    assert!(matches!(status, CommandOutcome::Status(_)), "{status:?}");

    let started = Instant::now();
    let ack = coordinator::run(&runtime, CliCommand::Stop, NoopBody, &retry())
        .expect("stop reaches the stalled owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop must be prompt on a slow server, took {:?}",
        started.elapsed()
    );

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.teardown, expected_cancel_teardown());
    assert!(snapshot.audio_torn_down, "audio device was not released");
    assert!(
        server.disconnected(),
        "the stalled HTTP connection must be closed"
    );
    assert_clean(&runtime);
    server.join();
}

#[test]
fn cancellation_while_connecting_is_prompt_and_leaves_no_resources() {
    // No chunks at all: the server accepts the request and stalls before
    // sending any audio. A second toggle must still stop the owner.
    let server = FakeTtsServer::spawn(ServerConfig::stalled(Vec::new()));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = {
        let body = body_for(
            &server,
            "cancel while connecting",
            SelectionConfig::default(),
            None,
            AudioSink::Fake,
            Arc::clone(&report),
        );
        let runtime = runtime.clone();
        thread::spawn(move || coordinator::run(&runtime, CliCommand::Toggle, body, &retry()))
    };

    assert!(
        wait_until(|| server.connected(), Duration::from_secs(5)),
        "the TTS request never connected"
    );
    let started = Instant::now();
    let ack = coordinator::run(&runtime, CliCommand::Toggle, NoopBody, &retry())
        .expect("a second toggle reaches the connecting owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "connecting cancellation was slow: {:?}",
        started.elapsed()
    );

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.teardown, expected_cancel_teardown());
    assert!(
        server.disconnected(),
        "the HTTP request must be aborted, not just ignored"
    );
    assert_clean(&runtime);
    server.join();
}

#[test]
fn normal_completion_leaves_no_process_lock_or_socket() {
    let server = FakeTtsServer::spawn(ServerConfig::ok(chunks(&fixture(), 2048)));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let owner = {
        let body = body_for(
            &server,
            "finish normally",
            SelectionConfig::default(),
            None,
            AudioSink::Fake,
            Arc::clone(&report),
        );
        let runtime = runtime.clone();
        thread::spawn(move || coordinator::run(&runtime, CliCommand::Toggle, body, &retry()))
    };

    assert!(
        wait_until(|| owner.is_finished(), Duration::from_secs(5)),
        "the owner must exit on its own after playback completes"
    );
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.states.last(), Some(&SessionState::Stopping));
    assert_eq!(snapshot.active_players, 0);
    assert!(snapshot.audio_torn_down);
    assert_eq!(
        snapshot.teardown.last(),
        Some(&TeardownStep::InstanceLockReleased),
        "the lock is released last: {:?}",
        snapshot.teardown
    );
    assert_clean(&runtime);
    server.join();
}

/// Helper test spawned as a child process by
/// `a_crashed_owner_leaves_no_lock_and_a_fresh_owner_recovers`. With no env
/// var it is a no-op during a normal `cargo test` run.
#[test]
fn crashed_owner_helper_process() {
    let Ok(dir) = std::env::var(CRASH_ENV) else {
        return;
    };
    let runtime = RuntimePaths::new(PathBuf::from(dir));
    std::fs::create_dir_all(&runtime.dir).expect("runtime dir");

    // Hold the lock and bind the socket, then exit abruptly WITHOUT dropping
    // the guard/listener. The kernel closes the fd (releasing the flock) but
    // the socket file survives: exactly the stale state a crash leaves.
    let _guard = InstanceLock::try_acquire(&runtime.lock)
        .expect("lock")
        .expect("helper owns the lock");
    let _listener = std::os::unix::net::UnixListener::bind(&runtime.socket).expect("bind socket");
    std::fs::write(runtime.dir.join("owner-ready"), b"ready").expect("ready marker");
    std::process::exit(CRASH_EXIT_CODE);
}

#[test]
fn a_crashed_owner_leaves_no_lock_and_a_fresh_owner_recovers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    std::fs::create_dir_all(&runtime.dir).expect("runtime dir");

    let mut child = ProcessCommand::new(std::env::current_exe().expect("test executable"))
        .arg("crashed_owner_helper_process")
        .arg("--exact")
        .env(CRASH_ENV, runtime.dir.to_str().expect("utf-8 runtime path"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crashed-owner helper");

    assert!(
        wait_until(
            || runtime.dir.join("owner-ready").exists(),
            Duration::from_secs(5)
        ),
        "the helper never became owner"
    );
    let status = child.wait().expect("wait for crashed helper");
    assert_eq!(
        status.code(),
        Some(CRASH_EXIT_CODE),
        "the helper must exit abruptly, not unwind"
    );

    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "the kernel must release the flock after the owner crashes"
    );
    assert!(
        runtime.socket.exists(),
        "a crash leaves a stale socket file for the next owner to remove"
    );

    // A fresh owner: clean startup removes the stale socket and the session
    // completes with no leftover lock or socket.
    let server = FakeTtsServer::spawn(ServerConfig::ok(chunks(&fixture(), 2048)));
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let body = body_for(
        &server,
        "recovered after a crash",
        SelectionConfig::default(),
        None,
        AudioSink::Fake,
        Arc::clone(&report),
    );
    let outcome = coordinator::run(&runtime, CliCommand::Toggle, body, &retry())
        .expect("a fresh owner recovers from the crash");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert_clean(&runtime);
    server.join();
}

// ---------------------------------------------------------------------------
// Privacy
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("utf-8 log output")
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn privacy_no_credentials_or_selected_text_appear_in_logs() {
    let selected = "Bí mật: SELECTED_TEXT_ĐẶC_BIỆT_9933 🎵";
    let api_key = "sk-SECRET-API-KEY-7788";
    let sink = Capture::default();
    let subscriber = logging::make_subscriber(LevelFilter::DEBUG, sink.clone());
    // A process-wide subscriber captures events from every thread the session
    // touches (the body, the control server, reqwest), so a leak cannot hide on
    // a thread the test forgot to instrument. This test is the only one in the
    // binary that installs a global subscriber.
    tracing::subscriber::set_global_default(subscriber).expect("install log capture");

    let server = FakeTtsServer::spawn(ServerConfig::ok(chunks(&fixture(), 2048)));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));

    // Both sensitive values really flow through the system: the text into the
    // TTS request body, the key into the Authorization header. The log must
    // still contain neither — this is not a vacuous "nothing was logged" test.
    logging::log_startup(&Config::default(), "0.1.0", Some(api_key));
    let body = body_for(
        &server,
        selected,
        SelectionConfig::default(),
        Some(api_key.to_owned()),
        AudioSink::Fake,
        Arc::clone(&report),
    );
    let outcome = coordinator::run(&runtime, CliCommand::Toggle, body, &retry())
        .expect("the privacy session completes");
    assert!(matches!(outcome, CommandOutcome::SessionRan));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["input"], selected, "text must reach TTS");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer sk-SECRET-API-KEY-7788"),
        "the key must reach TTS"
    );

    let logs = sink.text();
    assert!(logs.contains("murmduck starting"), "{logs:?}");
    assert!(logs.contains("selection received"), "{logs:?}");
    assert!(logs.contains("<redacted>"), "{logs:?}");
    for secret in [selected, "SELECTED_TEXT", "sk-SECRET", api_key] {
        assert!(
            !logs.contains(secret),
            "sensitive value {secret:?} leaked into logs: {logs:?}"
        );
    }
    assert_clean(&runtime);
    server.join();
}
