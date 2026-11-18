//! Ticket #8 acceptance tests: replacement via `read-selection`/`test-voice`.
//!
//! Everything runs through public seams with fakes only: queued/static fake
//! selection sources, a multi-request fake TTS server, and `AudioSink::Fake`
//! so no Wayland display or audible device is needed. The owner keeps the same
//! lock and control socket across replacements; a fresh cancellation token is
//! used per reading so the new one is not born cancelled.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::audio::AudioSink;
use murmduck::cli::Command;
use murmduck::config::{SelectionConfig, TtsConfig};
use murmduck::selection::{SelectionError, SelectionSource, StaticSource, TEST_VOICE_SAMPLE};
use murmduck::session::body::{
    NoopBody, ReadAloudSession, SessionBody, SessionContext, SessionReport,
};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::ipc::ClientOptions;
use murmduck::session::lock::InstanceLock;
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{ReadingKind, SessionError, TeardownStep};

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

/// Yields a queued selection per call. Two calls model a new PRIMARY selection
/// being read for a replacement.
struct QueuedSource {
    texts: Arc<Mutex<VecDeque<String>>>,
}

impl SelectionSource for QueuedSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        self.texts
            .lock()
            .expect("texts lock")
            .pop_front()
            .ok_or(SelectionError::Empty)
    }
}

#[derive(Debug, Clone)]
struct ReceivedRequest {
    input: String,
}

enum Reply {
    /// Respond immediately with the whole fixture.
    Data(Vec<u8>),
    /// Send the first half, wait on `gate`, then finish. Keeps a reading alive
    /// long enough for a control command to replace it.
    Gated {
        first: Vec<u8>,
        second: Vec<u8>,
        gate: mpsc::Receiver<()>,
    },
}

struct FakeTtsServer {
    base_url: String,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeTtsServer {
    fn spawn(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake TTS server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let log = Arc::clone(&requests);
        let stop = Arc::clone(&shutdown);
        let thread = thread::spawn(move || accept(listener, replies, log, stop));
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

/// Accept connections concurrently: a gated first reply must not block the
/// replacement's request from being served.
fn accept(
    listener: TcpListener,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    shutdown: Arc<AtomicBool>,
) {
    listener
        .set_nonblocking(true)
        .expect("set fake server nonblocking");
    while !shutdown.load(Ordering::Acquire) {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(_) => return,
        };
        let reply = replies.lock().expect("replies lock").pop_front();
        let Some(reply) = reply else {
            return;
        };
        let log = Arc::clone(&requests);
        let stop = Arc::clone(&shutdown);
        thread::spawn(move || serve_connection(stream, reply, log, stop));
    }
}

fn serve_connection(
    mut stream: std::net::TcpStream,
    reply: Reply,
    requests: Arc<Mutex<Vec<ReceivedRequest>>>,
    shutdown: Arc<AtomicBool>,
) {
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
    requests
        .lock()
        .expect("requests lock")
        .push(ReceivedRequest {
            input: body["input"].as_str().unwrap_or("").to_owned(),
        });

    let head = b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if stream.write_all(head).is_err() {
        return;
    }
    match reply {
        Reply::Data(bytes) => {
            if send_chunk(&mut stream, &bytes).is_err() {
                return;
            }
        }
        Reply::Gated {
            first,
            second,
            gate,
        } => {
            if send_chunk(&mut stream, &first).is_err() {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !shutdown.load(Ordering::Acquire) {
                if gate.recv_timeout(Duration::from_millis(20)).is_ok() {
                    break;
                }
            }
            if send_chunk(&mut stream, &second).is_err() {
                return;
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn send_chunk(stream: &mut std::net::TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:X}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn tts_config(server: &FakeTtsServer) -> TtsConfig {
    TtsConfig {
        base_url: server.base_url(),
        ..TtsConfig::default()
    }
}

/// A body whose concrete kind follows the replacement request. `read-selection`
/// uses a queued selection source; `test-voice` uses a fixed sample.
enum TestBody {
    Selection(ReadAloudSession<QueuedSource>),
    TestVoice(ReadAloudSession<StaticSource>),
}

impl SessionBody for TestBody {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        match self {
            Self::Selection(body) => body.run(ctx),
            Self::TestVoice(body) => body.run(ctx),
        }
    }

    fn record_teardown(&self, step: TeardownStep) {
        match self {
            Self::Selection(body) => body.record_teardown(step),
            Self::TestVoice(body) => body.record_teardown(step),
        }
    }
}

fn make_factory(
    server: &FakeTtsServer,
    texts: Arc<Mutex<VecDeque<String>>>,
    report: Arc<Mutex<SessionReport>>,
) -> impl FnMut(ReadingKind) -> TestBody + use<> {
    let config = tts_config(server);
    fn body<S: SelectionSource>(
        source: S,
        config: &TtsConfig,
        report: &Arc<Mutex<SessionReport>>,
    ) -> ReadAloudSession<S> {
        ReadAloudSession::new(
            source,
            SelectionConfig::default(),
            config.clone(),
            None,
            AudioSink::Fake,
        )
        .with_report(Arc::clone(report))
    }
    move |kind| match kind {
        ReadingKind::Selection => TestBody::Selection(body(
            QueuedSource {
                texts: Arc::clone(&texts),
            },
            &config,
            &report,
        )),
        ReadingKind::TestVoice => {
            TestBody::TestVoice(body(StaticSource::new(TEST_VOICE_SAMPLE), &config, &report))
        }
    }
}

fn paths(dir: &tempfile::TempDir) -> RuntimePaths {
    RuntimePaths::new(dir.path().join("murmduck"))
}

fn retry() -> ClientRetry {
    ClientRetry {
        lock_attempts: 10,
        lock_backoff: Duration::from_millis(5),
        connect: ClientOptions {
            attempts: 60,
            backoff: Duration::from_millis(5),
        },
    }
}

// ---------------------------------------------------------------------------
// Acceptance tests
// ---------------------------------------------------------------------------

#[test]
fn read_selection_replaces_a_running_reading_without_overlapping_audio() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (_gate_tx, gate_rx) = mpsc::channel();
    let server = FakeTtsServer::spawn(vec![
        Reply::Gated {
            first: bytes[..midpoint].to_vec(),
            second: bytes[midpoint..].to_vec(),
            gate: gate_rx,
        },
        Reply::Data(bytes.clone()),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let texts = Arc::new(Mutex::new(VecDeque::from([
        "first selection".to_owned(),
        "second selection".to_owned(),
    ])));
    let factory = make_factory(&server, Arc::clone(&texts), Arc::clone(&report));

    let owner = {
        let (runtime, factory) = (runtime.clone(), factory);
        thread::spawn(move || {
            coordinator::run_with_factory(&runtime, Command::ReadSelection, factory, &retry())
        })
    };

    assert!(
        wait_until(
            || {
                server.requests().len() == 1
                    && report
                        .lock()
                        .expect("report lock")
                        .states
                        .contains(&murmduck::session::SessionState::Playing)
            },
            Duration::from_secs(5)
        ),
        "the first reading never reached Playing"
    );

    // A second `read-selection` while active must be routed as a replacement.
    let ack = coordinator::run(&runtime, Command::ReadSelection, NoopBody, &retry())
        .expect("replacement reaches the owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");

    assert!(
        wait_until(|| server.requests().len() == 2, Duration::from_secs(5)),
        "the replacement never started a new TTS request"
    );

    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests[0].input, "first selection");
    assert_eq!(requests[1].input, "second selection");

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(
        snapshot.peak_active_players, 1,
        "audio pipelines overlapped"
    );
    assert_eq!(snapshot.active_players, 0, "a pipeline was left running");
    assert!(
        snapshot
            .teardown
            .iter()
            .filter(|step| **step == TeardownStep::AudioPipelineStopped)
            .count()
            >= 2,
        "both readings must tear down their pipeline: {:?}",
        snapshot.teardown
    );
    assert_eq!(
        snapshot.states.last(),
        Some(&murmduck::session::SessionState::Stopping),
        "session ends in Stopping: {:?}",
        snapshot.states
    );

    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after the replacement finished"
    );
    assert!(!runtime.socket.exists(), "socket removed after the session");
    server.join();
}

#[test]
fn read_selection_with_no_active_session_starts_a_new_reading() {
    let bytes = fixture();
    let server = FakeTtsServer::spawn(vec![Reply::Data(bytes.clone())]);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let texts = Arc::new(Mutex::new(VecDeque::from(["only selection".to_owned()])));
    let factory = make_factory(&server, texts, Arc::clone(&report));

    let outcome =
        coordinator::run_with_factory(&runtime, Command::ReadSelection, factory, &retry())
            .expect("read-selection starts a session while idle");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].input, "only selection");
    assert!(
        report.lock().expect("report lock").decoded_bytes > 0,
        "the selection was played"
    );
    server.join();
}

#[test]
fn test_voice_plays_the_sample_and_obeys_stop() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (gate_tx, gate_rx) = mpsc::channel();
    let server = FakeTtsServer::spawn(vec![Reply::Gated {
        first: bytes[..midpoint].to_vec(),
        second: bytes[midpoint..].to_vec(),
        gate: gate_rx,
    }]);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let texts = Arc::new(Mutex::new(VecDeque::new()));
    let factory = make_factory(&server, texts, Arc::clone(&report));

    let owner = {
        let (runtime, factory) = (runtime.clone(), factory);
        thread::spawn(move || {
            coordinator::run_with_factory(&runtime, Command::TestVoice, factory, &retry())
        })
    };

    assert!(
        wait_until(|| server.requests().len() == 1, Duration::from_secs(5)),
        "test-voice never contacted the TTS server"
    );
    assert_eq!(server.requests()[0].input, TEST_VOICE_SAMPLE);

    let started = Instant::now();
    let ack = coordinator::run(&runtime, Command::Stop, NoopBody, &retry())
        .expect("stop reaches the test-voice owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");
    drop(gate_tx);
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "test-voice must stop promptly: {:?}",
        started.elapsed()
    );

    let snapshot = report.lock().expect("report lock").clone();
    assert_eq!(snapshot.peak_active_players, 1);
    assert_eq!(snapshot.active_players, 0);
    assert!(
        InstanceLock::try_acquire(&runtime.lock)
            .expect("lock check")
            .is_some(),
        "lock released after stop"
    );
    server.join();
}

#[test]
fn read_selection_from_a_client_replaces_a_running_test_voice() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (_gate_tx, gate_rx) = mpsc::channel();
    let server = FakeTtsServer::spawn(vec![
        Reply::Gated {
            first: bytes[..midpoint].to_vec(),
            second: bytes[midpoint..].to_vec(),
            gate: gate_rx,
        },
        Reply::Data(bytes.clone()),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let texts = Arc::new(Mutex::new(VecDeque::from(["from selection".to_owned()])));
    let factory = make_factory(&server, Arc::clone(&texts), Arc::clone(&report));

    let owner = {
        let (runtime, factory) = (runtime.clone(), factory);
        thread::spawn(move || {
            coordinator::run_with_factory(&runtime, Command::TestVoice, factory, &retry())
        })
    };

    assert!(
        wait_until(|| server.requests().len() == 1, Duration::from_secs(5)),
        "test-voice never started"
    );
    let ack = coordinator::run(&runtime, Command::ReadSelection, NoopBody, &retry())
        .expect("read-selection reaches the test-voice owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");

    assert!(
        wait_until(|| server.requests().len() == 2, Duration::from_secs(5)),
        "replacement never started"
    );
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests[0].input, TEST_VOICE_SAMPLE);
    assert_eq!(
        requests[1].input, "from selection",
        "a read-selection replacement must read the PRIMARY selection"
    );
    assert_eq!(
        report.lock().expect("report lock").peak_active_players,
        1,
        "audio pipelines overlapped across the replacement"
    );
    server.join();
}

#[test]
fn test_voice_from_a_client_replaces_a_running_reading() {
    let bytes = fixture();
    let midpoint = bytes.len() / 2;
    let (_gate_tx, gate_rx) = mpsc::channel();
    let server = FakeTtsServer::spawn(vec![
        Reply::Gated {
            first: bytes[..midpoint].to_vec(),
            second: bytes[midpoint..].to_vec(),
            gate: gate_rx,
        },
        Reply::Data(bytes.clone()),
    ]);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let report = Arc::new(Mutex::new(SessionReport::default()));
    let texts = Arc::new(Mutex::new(VecDeque::from(
        ["original selection".to_owned()],
    )));
    let factory = make_factory(&server, texts, Arc::clone(&report));

    let owner = {
        let (runtime, factory) = (runtime.clone(), factory);
        thread::spawn(move || {
            coordinator::run_with_factory(&runtime, Command::ReadSelection, factory, &retry())
        })
    };

    assert!(
        wait_until(|| server.requests().len() == 1, Duration::from_secs(5)),
        "first reading never started"
    );
    let ack = coordinator::run(&runtime, Command::TestVoice, NoopBody, &retry())
        .expect("test-voice reaches the owner");
    assert!(matches!(ack, CommandOutcome::Controlled { .. }), "{ack:?}");

    assert!(
        wait_until(|| server.requests().len() == 2, Duration::from_secs(5)),
        "test-voice replacement never started"
    );
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");

    let requests = server.requests();
    assert_eq!(requests[0].input, "original selection");
    assert_eq!(requests[1].input, TEST_VOICE_SAMPLE);
    server.join();
}
