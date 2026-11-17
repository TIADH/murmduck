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
fn replace_selection_is_acknowledged_without_stopping_the_owner() {
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
    assert!(matches!(ack.response, Response::Ok { .. }));
    assert!(
        !cancel.is_cancelled(),
        "replacement must not stop the owner in this milestone"
    );

    server.shutdown();
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
