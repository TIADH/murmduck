//! Ticket #6 acceptance tests: the real streaming read-aloud body, driven at
//! the public seams. A local fake HTTP server stands in for the TTS endpoint,
//! the injectable [`SelectionSource`] replaces `wl-paste`, and the audio player
//! uses `AudioSink::Fake` so no device is required on the headless test host.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::audio::{AudioSink, StreamConfig};
use murmduck::cli::Command;
use murmduck::config::{SelectionConfig, TtsConfig};
use murmduck::selection::{SelectionError, SelectionSource};
use murmduck::session::body::{
    Cancellation, NoopBody, ReadAloudSession, SessionBody, SessionContext, SessionReport,
};
use murmduck::session::coordinator::{self, ClientRetry, ClientRetry as Retry, CommandOutcome};
use murmduck::session::lock::InstanceLock;
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{SessionError, SessionState};
use murmduck::tts::TtsError;

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

struct FakeSource {
    text: Result<String, SelectionError>,
    reads: Arc<Mutex<usize>>,
}

impl FakeSource {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: Ok(text.into()),
            reads: Arc::new(Mutex::new(0)),
        }
    }
}

impl SelectionSource for FakeSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        *self.reads.lock().expect("reads lock") += 1;
        self.text.clone()
    }
}

#[derive(Debug, Clone)]
struct ReceivedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: serde_json::Value,
}

struct ServerConfig {
    status: u16,
    chunks: Vec<Vec<u8>>,
    /// When set, the server sends `chunks[0]`, waits on this receiver, then
    /// sends the rest — letting a test observe playback before the full body.
    release: Option<mpsc::Receiver<()>>,
}

impl ServerConfig {
    fn ok(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            chunks,
            release: None,
        }
    }

    fn error(status: u16) -> Self {
        Self {
            status,
            chunks: Vec::new(),
            release: None,
        }
    }
}

struct FakeTtsServer {
    base_url: String,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeTtsServer {
    fn spawn(config: ServerConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake TTS server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let request_log = Arc::clone(&requests);
        let stop = Arc::clone(&shutdown);
        let thread = thread::spawn(move || serve_one(listener, config, request_log, stop));
        Self {
            base_url,
            requests,
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
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn serve_one(
    listener: TcpListener,
    config: ServerConfig,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    shutdown: Arc<AtomicBool>,
) {
    listener
        .set_nonblocking(true)
        .expect("set fake server nonblocking");
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

    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];
    let header_end = loop {
        let Ok(n) = stream.read(&mut scratch) else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&scratch[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
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
        let Ok(n) = stream.read(&mut scratch) else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
    }
    let body_end = (header_end + content_length).min(buf.len());
    let body: serde_json::Value =
        serde_json::from_slice(&buf[header_end..body_end]).unwrap_or(serde_json::Value::Null);
    let mut words = header.lines().next().unwrap_or("").split_whitespace();
    let method = words.next().unwrap_or("").to_owned();
    let path = words.next().unwrap_or("").to_owned();
    let authorization = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    requests
        .lock()
        .expect("requests lock")
        .push(ReceivedRequest {
            method,
            path,
            authorization,
            body,
        });

    if config.status != 200 {
        let _ = write!(
            stream,
            "HTTP/1.1 {} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            config.status
        );
        let _ = stream.flush();
        return;
    }

    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    for (index, chunk) in config.chunks.iter().enumerate() {
        if index == 1
            && let Some(release) = &config.release
        {
            // Bounded so a failing assertion cannot hang the test thread.
            let _ = release.recv_timeout(Duration::from_secs(20));
        }
        let _ = write!(stream, "{:X}\r\n", chunk.len());
        let _ = stream.write_all(chunk);
        let _ = stream.write_all(b"\r\n");
        let _ = stream.flush();
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn tts_config(server: &FakeTtsServer) -> TtsConfig {
    TtsConfig {
        base_url: server.base_url(),
        ..TtsConfig::default()
    }
}

fn body_for(
    server: &FakeTtsServer,
    source: FakeSource,
    api_key: Option<String>,
    report: Arc<Mutex<SessionReport>>,
) -> ReadAloudSession<FakeSource> {
    ReadAloudSession::new(
        source,
        SelectionConfig::default(),
        tts_config(server),
        api_key,
        AudioSink::Fake,
    )
    .with_stream_config(StreamConfig::default())
    .with_report(report)
}

fn paths(dir: &tempfile::TempDir) -> RuntimePaths {
    RuntimePaths::new(dir.path().join("murmduck"))
}

fn retry() -> Retry {
    ClientRetry {
        lock_attempts: 5,
        lock_backoff: Duration::from_millis(5),
        connect: murmduck::session::ipc::ClientOptions {
            attempts: 20,
            backoff: Duration::from_millis(5),
        },
    }
}

#[test]
fn toggle_streams_the_selection_to_tts_and_plays_it_to_completion() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let server = FakeTtsServer::spawn(ServerConfig::ok(vec![
        bytes[..midpoint].to_vec(),
        bytes[midpoint..].to_vec(),
    ]));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let body = body_for(
        &server,
        FakeSource::new("Hello from Murmduck."),
        None,
        Arc::clone(&report),
    );

    let outcome = coordinator::run(&runtime, Command::Toggle, body, &retry())
        .expect("the happy path completes");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one TTS request");
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/audio/speech");
    assert_eq!(
        request.authorization, None,
        "servers without auth are supported"
    );
    assert_eq!(request.body["model"], "tts-1");
    assert_eq!(request.body["voice"], "alloy");
    assert_eq!(request.body["input"], "Hello from Murmduck.");
    assert_eq!(request.body["response_format"], "mp3");
    assert_eq!(request.body["speed"], 1.0);

    let report = report.lock().expect("report lock");
    assert_eq!(report.bytes_pushed, bytes.len() as u64);
    assert!(report.decoded_bytes > 0, "decoded audio reached the sink");
    assert!(
        report.states.contains(&SessionState::Buffering),
        "buffering state was published: {:?}",
        report.states
    );
    assert!(
        report.states.contains(&SessionState::Playing),
        "playing state was published: {:?}",
        report.states
    );
    assert_eq!(
        report.states.last(),
        Some(&SessionState::Stopping),
        "session ends in Stopping"
    );
    drop(report);

    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after completion"
    );
    assert!(!runtime.socket.exists(), "socket removed after completion");
    server.join();
}

#[test]
fn playback_starts_before_the_whole_response_has_downloaded() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (release, gate) = mpsc::channel();
    let server = FakeTtsServer::spawn(ServerConfig {
        status: 200,
        chunks: vec![bytes[..midpoint].to_vec(), bytes[midpoint..].to_vec()],
        release: Some(gate),
    });
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let mut body = body_for(
        &server,
        FakeSource::new("half now, half later"),
        None,
        Arc::clone(&report),
    );
    let cancel = Cancellation::new();
    let state = Arc::new(Mutex::new(SessionState::Starting));
    let ctx = SessionContext {
        cancel: cancel.clone(),
        state: Arc::clone(&state),
    };

    let owner = thread::spawn(move || body.run(&ctx));
    let started = wait_until(
        || *state.lock().expect("state lock") == SessionState::Playing,
        Duration::from_secs(5),
    );
    assert!(
        started,
        "audio must start while the server still holds the second half"
    );

    release.send(()).expect("release the second half");
    owner.join().expect("body joins").expect("body succeeds");
    server.join();
}

#[test]
fn the_control_socket_reports_buffering_while_the_server_is_slow() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (release, gate) = mpsc::channel();
    let server = FakeTtsServer::spawn(ServerConfig {
        status: 200,
        chunks: vec![bytes[..midpoint].to_vec(), bytes[midpoint..].to_vec()],
        release: Some(gate),
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let body = body_for(
        &server,
        FakeSource::new("look busy"),
        None,
        Arc::clone(&report),
    );

    let owner = {
        let (runtime, body) = (runtime.clone(), body);
        thread::spawn(move || coordinator::run(&runtime, Command::Toggle, body, &retry()))
    };

    let observed = wait_until(
        || {
            let paths = runtime.clone();
            match coordinator::run(&paths, Command::Status, NoopBody, &retry()) {
                Ok(CommandOutcome::Status(state)) => {
                    state == SessionState::Buffering || state == SessionState::Playing
                }
                _ => false,
            }
        },
        Duration::from_secs(5),
    );
    assert!(
        observed,
        "status clients can reach the owner while it streams"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_none(),
        "owner still holds the lock while streaming"
    );

    release.send(()).expect("release the second half");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    server.join();
}

#[test]
fn a_configured_api_key_is_sent_as_a_bearer_token() {
    let server = FakeTtsServer::spawn(ServerConfig::ok(vec![fixture()]));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let body = body_for(
        &server,
        FakeSource::new("read me aloud"),
        Some("sk-test-token".to_owned()),
        Arc::new(Mutex::new(SessionReport::default())),
    );

    coordinator::run(&runtime, Command::Toggle, body, &retry())
        .expect("session completes with auth");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer sk-test-token")
    );
    server.join();
}

#[test]
fn an_http_error_surfaces_and_cleans_up_the_session() {
    let server = FakeTtsServer::spawn(ServerConfig::error(503));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let body = body_for(
        &server,
        FakeSource::new("this will fail"),
        None,
        Arc::new(Mutex::new(SessionReport::default())),
    );

    let err = coordinator::run(&runtime, Command::Toggle, body, &retry())
        .expect_err("a non-success status aborts the session");
    assert!(
        matches!(err, SessionError::Tts(TtsError::Http { status: 503 })),
        "{err:?}"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after an HTTP error"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after an HTTP error"
    );
    server.join();
}

#[test]
fn an_unreachable_server_surfaces_and_cleans_up_the_session() {
    let addr = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe addr")
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let source = FakeSource::new("nobody is listening");
    let body = ReadAloudSession::new(
        source,
        SelectionConfig::default(),
        TtsConfig {
            base_url: format!("http://{addr}/v1"),
            ..TtsConfig::default()
        },
        None,
        AudioSink::Fake,
    );

    let err = coordinator::run(&runtime, Command::Toggle, body, &retry())
        .expect_err("a refused connection aborts the session");
    assert!(
        matches!(err, SessionError::Tts(TtsError::Request { .. })),
        "{err:?}"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after a network error"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after a network error"
    );
}

#[test]
fn an_empty_selection_never_contacts_the_tts_server() {
    let server = FakeTtsServer::spawn(ServerConfig::ok(vec![fixture()]));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let source = FakeSource {
        text: Err(SelectionError::Empty),
        reads: Arc::new(Mutex::new(0)),
    };
    let body = ReadAloudSession::new(
        source,
        SelectionConfig::default(),
        tts_config(&server),
        None,
        AudioSink::Fake,
    );

    let err = coordinator::run(&runtime, Command::Toggle, body, &retry())
        .expect_err("an empty selection aborts the session");
    assert!(
        matches!(err, SessionError::Selection(SelectionError::Empty)),
        "{err:?}"
    );
    assert!(server.requests().is_empty(), "no TTS request was sent");
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after a selection error"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after a selection error"
    );
    server.join();
}

#[test]
fn a_decoder_error_surfaces_and_cleans_up_the_session() {
    let server = FakeTtsServer::spawn(ServerConfig::ok(vec![vec![0x00; 4096]; 4]));
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let body = body_for(
        &server,
        FakeSource::new("garbage audio"),
        None,
        Arc::new(Mutex::new(SessionReport::default())),
    );

    let result = coordinator::run(&runtime, Command::Toggle, body, &retry());
    assert!(
        matches!(result, Err(SessionError::Audio(_))),
        "a decoder failure must be reported as an audio error: {result:?}"
    );
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after a decoder error"
    );
    assert!(
        !runtime.socket.exists(),
        "socket removed after a decoder error"
    );
    server.join();
}
