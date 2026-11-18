//! IPC transport seam: a real Unix control socket in a temp dir, with the
//! owner's control server on one side and the control client on the other.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::session::SessionState;
use murmduck::session::body::Cancellation;
use murmduck::session::ipc::{self, ClientOptions, ControlServer};
use murmduck::session::protocol::{Command, Response};

fn fast_options() -> ClientOptions {
    ClientOptions {
        attempts: 40,
        backoff: Duration::from_millis(5),
    }
}

#[test]
fn a_stop_command_reaches_the_owner_and_returns_an_ack() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let state = Arc::new(Mutex::new(SessionState::Playing));
    let mut server = ControlServer::spawn(listener, state.clone(), cancel.clone());

    let ack = ipc::send_command(&socket, Command::Stop, &fast_options()).expect("client delivers");
    assert_eq!(
        ack.response,
        Response::Ok {
            state: SessionState::Stopping
        }
    );
    assert!(cancel.is_cancelled(), "owner must be told to stop");

    server.shutdown();
}

#[test]
fn a_status_command_reports_the_owners_current_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let state = Arc::new(Mutex::new(SessionState::Buffering));
    let mut server = ControlServer::spawn(listener, state.clone(), Cancellation::new());

    let ack =
        ipc::send_command(&socket, Command::Status, &fast_options()).expect("client delivers");
    assert_eq!(
        ack.response,
        Response::Ok {
            state: SessionState::Buffering
        }
    );

    server.shutdown();
}

#[test]
fn replace_selection_cancels_the_current_reading_and_reports_stopping() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let mut server = ControlServer::spawn(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        cancel.clone(),
    );

    let ack = ipc::send_command(&socket, Command::ReplaceSelection, &fast_options())
        .expect("client delivers");
    assert_eq!(
        ack.response,
        Response::Ok {
            state: SessionState::Stopping
        }
    );
    assert!(
        cancel.is_cancelled(),
        "replacement stops the current reading before the new one starts"
    );

    server.shutdown();
}

#[test]
fn replace_test_voice_is_routed_without_stopping_the_session_lock() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let control = murmduck::session::SessionControl::new();
    let mut server = ControlServer::spawn_with_control(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        control.clone(),
    );

    let ack = ipc::send_command(&socket, Command::ReplaceTestVoice, &fast_options())
        .expect("client delivers");
    assert!(matches!(ack.response, Response::Ok { .. }));
    assert_eq!(
        control.replacement_kind(),
        murmduck::session::ReadingKind::TestVoice
    );
    assert!(
        control.replace_requested(),
        "a replacement must be visible to the owner loop"
    );
    assert!(
        !control.stop_requested(),
        "replacement keeps the owner alive; only Stop ends the session"
    );

    server.shutdown();
    assert!(!cancel.is_cancelled(), "the legacy handle is untouched");
}

#[test]
fn a_client_retries_until_the_socket_is_ready() {
    // Models "second invocation before the owner finished creating the socket":
    // the socket path is known but nothing is listening yet.
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let late_socket = socket.clone();
    let thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let listener = std::os::unix::net::UnixListener::bind(&late_socket).expect("bind socket");
        let mut server = ControlServer::spawn(
            listener,
            Arc::new(Mutex::new(SessionState::Playing)),
            Cancellation::new(),
        );
        thread::sleep(Duration::from_millis(120));
        server.shutdown();
    });

    let ack =
        ipc::send_command(&socket, Command::Status, &fast_options()).expect("eventually connects");
    assert!(matches!(ack.response, Response::Ok { .. }));
    thread.join().expect("server thread completes");
}

#[test]
fn a_client_gives_up_within_a_bounded_time_when_nobody_listens() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let options = ClientOptions {
        attempts: 3,
        backoff: Duration::from_millis(5),
    };

    let err = ipc::send_command(&socket, Command::Stop, &options).expect_err("no listener");
    assert!(matches!(err, ipc::ClientError::Unavailable), "{err:?}");
}

#[test]
fn a_client_that_never_completes_a_frame_does_not_delay_a_stop_command() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let mut server = ControlServer::spawn(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        cancel.clone(),
    );

    // Connect and send a partial JSON frame with no terminating newline. Give
    // the accept loop time to start waiting on this client before sending Stop.
    let mut stalled = UnixStream::connect(&socket).expect("connect stalled client");
    stalled
        .write_all(b"{\"version\":1,\"command\":\"S")
        .expect("send partial frame");
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let ack = ipc::send_command(&socket, Command::Stop, &fast_options()).expect("stop delivered");
    assert!(
        matches!(ack.response, Response::Ok { .. }),
        "stop must still be acknowledged: {:?}",
        ack.response
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_200),
        "a stalled client blocked a hotkey command for {:?}",
        started.elapsed()
    );
    assert!(
        cancel.is_cancelled(),
        "the delayed Stop must reach the owner"
    );
    drop(stalled);
    server.shutdown();
}

#[test]
fn a_stop_is_handled_promptly_when_all_worker_slots_hold_trickling_frames() {
    // Fill every worker slot with a partial frame that trickles a byte often
    // enough to beat any per-read inactivity timeout. The next real Stop must
    // still be served immediately: neither a busy worker pool nor a slow client
    // may hold up the hotkey command.
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let mut server = ControlServer::spawn(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        cancel.clone(),
    );

    let mut stalled = Vec::new();
    for _ in 0..8 {
        let mut stream = UnixStream::connect(&socket).expect("connect stalled client");
        stream
            .write_all(b"{\"version\":1,\"command\":\"S")
            .expect("send partial frame");
        stalled.push(stream);
    }
    // Let the accept loop hand every connection to a worker.
    thread::sleep(Duration::from_millis(150));

    let keep_trickling = Arc::new(AtomicBool::new(true));
    let trickle_flag = Arc::clone(&keep_trickling);
    let trickle = thread::spawn(move || {
        while trickle_flag.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
            for stream in &mut stalled {
                if stream.write_all(b"x").is_err() {
                    return;
                }
            }
        }
    });

    let started = Instant::now();
    let options = ClientOptions {
        attempts: 1,
        backoff: Duration::from_millis(1),
    };
    let ack = ipc::send_command(&socket, Command::Stop, &options);
    let elapsed = started.elapsed();
    // Always stop the trickle, including on assertion failure, so shutdown can
    // join the workers instead of hanging until the test process exits.
    keep_trickling.store(false, Ordering::Release);
    let _ = trickle.join();
    let ack = ack.expect("stop delivered despite a saturated worker pool");

    assert!(
        matches!(ack.response, Response::Ok { .. }),
        "stop must be acknowledged: {:?}",
        ack.response
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a saturated worker pool delayed Stop for {elapsed:?}"
    );
    assert!(
        cancel.is_cancelled(),
        "the delayed Stop must reach the owner"
    );
    server.shutdown();
}

#[test]
fn a_flood_of_incomplete_clients_cannot_delay_a_stop_command() {
    // Under saturation the at-capacity path must not read frames on the single
    // accept thread: a queue of stalled clients ahead of a genuine Stop would
    // otherwise serialize into a multi-second hotkey delay (review T2). Fill the
    // workers, then queue far more incomplete connections than there are
    // workers, and require Stop to still be prompt.
    const WORKERS: usize = 8;
    const FLOOD: usize = 60;

    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let cancel = Cancellation::new();
    let mut server = ControlServer::spawn(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        cancel.clone(),
    );

    // Occupy every worker slot with a partial frame so the following clients
    // take the at-capacity path.
    let mut hogs = Vec::new();
    for _ in 0..WORKERS {
        let mut stream = UnixStream::connect(&socket).expect("connect hog client");
        stream
            .write_all(b"{\"version\":1,\"command\":\"S")
            .expect("send partial frame");
        hogs.push(stream);
    }
    thread::sleep(Duration::from_millis(100));

    // Queue a large backlog of incomplete connections. Each one must be
    // rejected without occupying the accept thread for PRIORITY_WAIT.
    let mut flood = Vec::new();
    for _ in 0..FLOOD {
        let mut stream = UnixStream::connect(&socket).expect("connect flood client");
        stream
            .write_all(b"{\"version\":1,\"command\":\"S")
            .expect("send partial frame");
        flood.push(stream);
    }

    let started = Instant::now();
    let options = ClientOptions {
        attempts: 1,
        backoff: Duration::from_millis(1),
    };
    let ack = ipc::send_command(&socket, Command::Stop, &options);
    let elapsed = started.elapsed();
    let ack = ack.expect("stop delivered despite a flood of incomplete clients");

    assert!(
        matches!(ack.response, Response::Ok { .. }),
        "stop must be acknowledged: {:?}",
        ack.response
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a flood of incomplete clients delayed Stop for {elapsed:?}"
    );
    assert!(
        cancel.is_cancelled(),
        "the delayed Stop must reach the owner"
    );
    drop(flood);
    drop(hogs);
    server.shutdown();
}

#[test]
fn an_oversized_frame_cannot_block_other_clients() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

    let mut server = ControlServer::spawn(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        Cancellation::new(),
    );

    let mut huge = UnixStream::connect(&socket).expect("connect oversized client");
    // Well beyond any reasonable control frame, with no newline so the reader
    // cannot simply stop at a delimiter.
    huge.write_all(&vec![b'x'; 64 * 1024])
        .expect("send oversized frame");
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let ack =
        ipc::send_command(&socket, Command::Status, &fast_options()).expect("status delivered");
    assert!(
        matches!(ack.response, Response::Ok { .. }),
        "a runaway client must not block status: {:?}",
        ack.response
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_200),
        "an oversized frame delayed status for {:?}",
        started.elapsed()
    );
    drop(huge);
    server.shutdown();
}

#[test]
fn a_replacement_arriving_during_shutdown_is_declined_not_dropped() {
    use murmduck::session::ReadingKind;

    // A replace accepted just before shutdown is returned to the owner so it
    // is actually started; one arriving after shutdown began gets a clear
    // "declined" acknowledgement instead of Ok-then-nothing.
    let control = murmduck::session::SessionControl::new();
    assert!(control.request_replace(ReadingKind::Selection));
    assert_eq!(
        control.close_for_shutdown(),
        Some(ReadingKind::Selection),
        "a pending replacement must not be lost"
    );
    assert!(
        !control.request_replace(ReadingKind::Selection),
        "further replacements are refused once shutdown began"
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
    let control = murmduck::session::SessionControl::new();
    let mut server = ControlServer::spawn_with_control(
        listener,
        Arc::new(Mutex::new(SessionState::Stopping)),
        control.clone(),
    );
    control.close_for_shutdown();

    let ack = ipc::send_command(&socket, Command::ReplaceSelection, &fast_options())
        .expect("client delivers");
    assert!(
        matches!(&ack.response, Response::Error { message } if message.contains("declined")),
        "the client must be told the replacement was declined: {:?}",
        ack.response
    );
    assert!(
        !control.replace_requested(),
        "a declined replacement must not be queued"
    );
    server.shutdown();
}
