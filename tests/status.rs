//! Ticket #8 acceptance tests: `status` reports the real lifecycle state from
//! the owner's shared state through the control socket, in human-readable and
//! JSON form. A scripted body publishes each state deterministically so no
//! timing-sensitive fake TTS server is needed here.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use murmduck::cli::Command;
use murmduck::session::body::{NoopBody, SessionBody, SessionContext};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::ipc::{self, ClientOptions};
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{SessionError, SessionState};

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

fn socket_options() -> ClientOptions {
    ClientOptions {
        attempts: 60,
        backoff: Duration::from_millis(5),
    }
}

/// Publishes a scripted state, then waits for the test to allow the next step.
struct ScriptedBody {
    script: Vec<(SessionState, mpsc::Receiver<()>)>,
}

impl SessionBody for ScriptedBody {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        for (state, gate) in &self.script {
            ctx.set_state(*state);
            let _ = gate.recv_timeout(Duration::from_secs(10));
            if ctx.cancel.is_cancelled() {
                break;
            }
        }
        Ok(())
    }
}

#[test]
fn status_reports_every_lifecycle_state_from_a_live_owner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let states = [
        SessionState::Starting,
        SessionState::Connecting,
        SessionState::Buffering,
        SessionState::Playing,
    ];
    let mut script = Vec::new();
    let mut gates = Vec::new();
    for state in states {
        let (tx, rx) = mpsc::channel();
        script.push((state, rx));
        gates.push(tx);
    }

    let owner = {
        let runtime = runtime.clone();
        thread::spawn(move || {
            coordinator::run(&runtime, Command::Toggle, ScriptedBody { script }, &retry())
        })
    };

    for (expected, gate) in states.iter().zip(gates) {
        let observed = loop {
            let outcome = coordinator::run(
                &runtime,
                Command::Status { json: false },
                NoopBody,
                &retry(),
            )
            .expect("status reaches the owner");
            match outcome {
                CommandOutcome::Status(state) => {
                    if state == *expected {
                        break state;
                    }
                }
                other => panic!("unexpected status outcome: {other:?}"),
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(observed, *expected);
        gate.send(()).expect("advance scripted body");
    }

    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
}

#[test]
fn status_reports_stopping_while_the_owner_is_tearing_down() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let (hold_tx, hold_rx) = mpsc::channel();
    let (last_tx, last_rx) = mpsc::channel();

    let owner = {
        let runtime = runtime.clone();
        thread::spawn(move || {
            coordinator::run(
                &runtime,
                Command::Toggle,
                ScriptedBody {
                    script: vec![
                        (SessionState::Playing, hold_rx),
                        (SessionState::Stopping, last_rx),
                    ],
                },
                &retry(),
            )
        })
    };

    // Wait until the body is holding at Playing.
    loop {
        let outcome = coordinator::run(
            &runtime,
            Command::Status { json: false },
            NoopBody,
            &retry(),
        )
        .expect("status reaches the owner");
        if matches!(outcome, CommandOutcome::Status(SessionState::Playing)) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    // A Stop command flips the shared state to Stopping before the body has
    // finished tearing down, so a status client still sees Stopping.
    let ack = ipc::send_command(
        &runtime.socket,
        murmduck::session::protocol::Command::Stop,
        &socket_options(),
    )
    .expect("stop command reaches the owner");
    assert_eq!(
        ack.response,
        murmduck::session::protocol::Response::Ok {
            state: SessionState::Stopping
        }
    );
    let outcome = coordinator::run(
        &runtime,
        Command::Status { json: false },
        NoopBody,
        &retry(),
    )
    .expect("status during teardown");
    assert!(
        matches!(outcome, CommandOutcome::Status(SessionState::Stopping)),
        "{outcome:?}"
    );

    hold_tx.send(()).expect("advance past Playing");
    last_tx.send(()).expect("advance past Stopping");
    let outcome = owner.join().expect("owner joins").expect("owner succeeds");
    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
}

#[test]
fn status_reports_inactive_when_no_owner_is_present() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = paths(&dir);
    let outcome = coordinator::run(
        &runtime,
        Command::Status { json: false },
        NoopBody,
        &retry(),
    )
    .expect("idle status");
    assert!(
        matches!(outcome, CommandOutcome::Status(SessionState::Inactive)),
        "{outcome:?}"
    );
    assert_eq!(outcome.report(), "Session status: Inactive");
}

#[test]
fn status_has_a_stable_json_report_for_every_state() {
    let expected = [
        (SessionState::Inactive, "{\"state\":\"Inactive\"}"),
        (SessionState::Starting, "{\"state\":\"Starting\"}"),
        (SessionState::Connecting, "{\"state\":\"Connecting\"}"),
        (SessionState::Buffering, "{\"state\":\"Buffering\"}"),
        (SessionState::Playing, "{\"state\":\"Playing\"}"),
        (SessionState::Stopping, "{\"state\":\"Stopping\"}"),
    ];
    for (state, json) in expected {
        let outcome = CommandOutcome::Status(state);
        assert_eq!(outcome.report_json(), json);
    }
}
