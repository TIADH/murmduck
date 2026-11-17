//! Coordinator seam: owner-vs-client routing, lock/IPC lifecycle, and the
//! race cases from spec §6. All paths are injected temp dirs and all session
//! bodies are fakes, so no Wayland session or real selection provider is used.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmduck::cli::Command;
use murmduck::config::SelectionConfig;
use murmduck::selection::{SelectionError, SelectionSource};
use murmduck::session::body::{Cancellation, PlaceholderSession, SessionBody, SessionContext};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::ipc::ClientOptions;
use murmduck::session::lock::InstanceLock;
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{SessionError, SessionState};
use std::os::unix::net::UnixListener;

fn paths(dir: &tempfile::TempDir) -> RuntimePaths {
    RuntimePaths::new(dir.path().join("murmduck"))
}

fn fast_retry() -> ClientRetry {
    ClientRetry {
        lock_attempts: 20,
        lock_backoff: Duration::from_millis(5),
        connect: ClientOptions {
            attempts: 40,
            backoff: Duration::from_millis(5),
        },
    }
}

fn wait_until(condition: impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Fake body that signals when it starts and then waits to be stopped.
struct SignalBody {
    started: Arc<AtomicBool>,
    max_wait: Duration,
}

impl SessionBody for SignalBody {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        self.started.store(true, Ordering::Release);
        ctx.set_state(SessionState::Playing);
        ctx.cancel.wait_timeout(self.max_wait);
        Ok(())
    }
}

/// Body that records how many run at once, to detect overlapping sessions.
struct ConcurrencyBody {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl SessionBody for ConcurrencyBody {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        ctx.cancel.wait_timeout(Duration::from_millis(40));
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

struct NoopBody;

impl SessionBody for NoopBody {
    fn run(&mut self, _ctx: &SessionContext) -> Result<(), SessionError> {
        Ok(())
    }
}

struct FakeSource {
    text: Result<String, SelectionError>,
}

impl SelectionSource for FakeSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        self.text.clone()
    }
}

#[test]
fn stop_with_no_session_is_a_noop_and_starts_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);

    let outcome = coordinator::run(&paths, Command::Stop, NoopBody, &fast_retry())
        .expect("stop succeeds while idle");

    assert!(matches!(outcome, CommandOutcome::Idle), "{outcome:?}");
    assert!(!paths.socket.exists(), "no socket should be created");
}

#[test]
fn toggle_becomes_owner_and_a_second_toggle_stops_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    let started = Arc::new(AtomicBool::new(false));

    let owner = {
        let (paths, started) = (paths.clone(), started.clone());
        thread::spawn(move || {
            coordinator::run(
                &paths,
                Command::Toggle,
                SignalBody {
                    started,
                    max_wait: Duration::from_secs(10),
                },
                &fast_retry(),
            )
        })
    };

    wait_until(|| started.load(Ordering::Acquire), Duration::from_secs(2));
    assert!(
        InstanceLock::try_acquire(&paths.lock)
            .expect("lock check")
            .is_none(),
        "the owner must hold the lock for the whole session"
    );
    assert!(
        paths.socket.exists(),
        "the owner must expose a control socket"
    );

    let second = coordinator::run(&paths, Command::Toggle, NoopBody, &fast_retry())
        .expect("second toggle delivers a stop");
    assert!(
        matches!(second, CommandOutcome::Controlled { .. }),
        "{second:?}"
    );

    let owner_outcome = owner
        .join()
        .expect("owner does not panic")
        .expect("owner succeeds");
    assert!(
        matches!(owner_outcome, CommandOutcome::SessionRan),
        "{owner_outcome:?}"
    );

    assert!(
        InstanceLock::try_acquire(&paths.lock)
            .expect("lock check")
            .is_some(),
        "lock is released after the owner exits"
    );
    assert!(paths.lock.is_file(), "the lock file is never deleted");
    assert!(
        !paths.socket.exists(),
        "the owner removes only its own socket"
    );
}

#[test]
fn status_is_inactive_when_idle_and_live_while_a_session_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);

    let idle = coordinator::run(&paths, Command::Status, NoopBody, &fast_retry())
        .expect("status succeeds while idle");
    assert!(
        matches!(idle, CommandOutcome::Status(SessionState::Inactive)),
        "{idle:?}"
    );

    let started = Arc::new(AtomicBool::new(false));
    let owner = {
        let (paths, started) = (paths.clone(), started.clone());
        thread::spawn(move || {
            coordinator::run(
                &paths,
                Command::Toggle,
                SignalBody {
                    started,
                    max_wait: Duration::from_secs(10),
                },
                &fast_retry(),
            )
        })
    };
    wait_until(|| started.load(Ordering::Acquire), Duration::from_secs(2));

    let live = coordinator::run(&paths, Command::Status, NoopBody, &fast_retry())
        .expect("status succeeds while active");
    assert!(
        matches!(live, CommandOutcome::Status(SessionState::Playing)),
        "{live:?}"
    );

    let _ =
        coordinator::run(&paths, Command::Stop, NoopBody, &fast_retry()).expect("stop delivers");
    let _ = owner.join().expect("owner joins");
}

#[test]
fn concurrent_first_invocations_never_overlap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (paths, active, peak, barrier) =
                (paths.clone(), active.clone(), peak.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                coordinator::run(
                    &paths,
                    Command::Toggle,
                    ConcurrencyBody { active, peak },
                    &fast_retry(),
                )
            })
        })
        .collect();

    for handle in handles {
        handle
            .join()
            .expect("invocation does not panic")
            .expect("invocation succeeds");
    }

    assert_eq!(peak.load(Ordering::SeqCst), 1, "sessions overlapped");
}

#[test]
fn a_stale_socket_file_does_not_block_a_new_owner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    std::fs::create_dir_all(&paths.dir).expect("runtime dir");
    // Drop the listener without removing the socket file: a crashed owner.
    let _stale = UnixListener::bind(&paths.socket).expect("stale socket");

    let started = Arc::new(AtomicBool::new(false));
    let outcome = coordinator::run(
        &paths,
        Command::Toggle,
        SignalBody {
            started: started.clone(),
            max_wait: Duration::ZERO,
        },
        &fast_retry(),
    )
    .expect("new owner replaces the stale socket");

    assert!(matches!(outcome, CommandOutcome::SessionRan), "{outcome:?}");
    assert!(started.load(Ordering::Acquire));
    assert!(
        !paths.socket.exists(),
        "the new owner removes its own socket"
    );
}

#[test]
fn a_client_takes_over_when_the_owner_exits_during_initialization() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    // Pretend an owner grabbed the lock but has not created a socket yet.
    let guard = InstanceLock::try_acquire(&paths.lock)
        .expect("acquire")
        .expect("pseudo-owner");

    let client = {
        let paths = paths.clone();
        thread::spawn(move || coordinator::run(&paths, Command::Stop, NoopBody, &fast_retry()))
    };

    thread::sleep(Duration::from_millis(30));
    drop(guard); // the pseudo-owner "crashed" mid-initialisation

    let outcome = client
        .join()
        .expect("client joins")
        .expect("client succeeds");
    assert!(matches!(outcome, CommandOutcome::Idle), "{outcome:?}");
}

#[test]
fn a_client_waits_for_the_socket_before_delivering_a_command() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    let guard = InstanceLock::try_acquire(&paths.lock)
        .expect("acquire")
        .expect("pseudo-owner");

    let late_paths = paths.clone();
    let fake_owner = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        let listener = UnixListener::bind(&late_paths.socket).expect("late socket");
        let server = murmduck::session::ipc::ControlServer::spawn(
            listener,
            Arc::new(Mutex::new(SessionState::Playing)),
            Cancellation::new(),
        );
        thread::sleep(Duration::from_millis(200));
        drop(server);
    });

    let client = {
        let paths = paths.clone();
        thread::spawn(move || coordinator::run(&paths, Command::Toggle, NoopBody, &fast_retry()))
    };

    let outcome = client
        .join()
        .expect("client joins")
        .expect("client succeeds");
    assert!(
        matches!(outcome, CommandOutcome::Controlled { .. }),
        "{outcome:?}"
    );

    drop(guard);
    fake_owner.join().expect("fake owner joins");
}

#[test]
fn the_placeholder_body_reads_once_and_runs_until_cancelled() {
    struct CountingSource {
        reads: Arc<AtomicUsize>,
    }
    impl SelectionSource for CountingSource {
        fn read_text(&self) -> Result<String, SelectionError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok("hello from a fake PRIMARY selection".to_owned())
        }
    }

    let reads = Arc::new(AtomicUsize::new(0));
    let body = PlaceholderSession::new(
        CountingSource {
            reads: reads.clone(),
        },
        SelectionConfig::default(),
    );
    let cancel = Cancellation::new();
    let state = Arc::new(Mutex::new(SessionState::Starting));
    let ctx = SessionContext {
        cancel: cancel.clone(),
        state: state.clone(),
    };

    let handle = thread::spawn(move || {
        let mut body = body;
        body.run(&ctx)
    });

    wait_until(
        || *state.lock().expect("state lock") == SessionState::Playing,
        Duration::from_secs(2),
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "selection read exactly once"
    );
    assert!(
        !handle.is_finished(),
        "placeholder stays alive until stopped"
    );

    cancel.cancel();
    handle.join().expect("body joins").expect("body succeeds");
    assert_eq!(
        *state.lock().expect("state lock"),
        SessionState::Stopping,
        "placeholder reports Stopping after cancellation"
    );
}

#[test]
fn a_selection_error_still_cleans_up_the_lock_and_socket() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(&dir);
    let failing_source = FakeSource {
        text: Err(SelectionError::Empty),
    };
    let body = PlaceholderSession::new(failing_source, SelectionConfig::default());

    let err = coordinator::run(&paths, Command::Toggle, body, &fast_retry())
        .expect_err("empty selection aborts the session");
    assert!(
        matches!(err, SessionError::Selection(SelectionError::Empty)),
        "{err:?}"
    );

    assert!(
        InstanceLock::try_acquire(&paths.lock)
            .expect("lock check")
            .is_some(),
        "lock must be released even when the body fails"
    );
    assert!(paths.lock.is_file());
    assert!(!paths.socket.exists());
}
