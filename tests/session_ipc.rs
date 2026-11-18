//! IPC transport seam: a real Unix control socket in a temp dir, with the
//! owner's control server on one side and the control client on the other.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
