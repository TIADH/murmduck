use std::process::Command;

use clap::Parser;
use murmduck::cli::{Cli, Command as CliCommand};

fn murmduck() -> Command {
    Command::new(env!("CARGO_BIN_EXE_murmduck"))
}

#[test]
fn version_flag_reports_the_package_version() {
    let output = murmduck().arg("--version").output().expect("run binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected version in {stdout:?}"
    );
    assert!(stdout.contains("murmduck"), "expected name in {stdout:?}");
}

#[test]
fn help_flag_lists_every_user_facing_command() {
    let output = murmduck().arg("--help").output().expect("run binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    for command in ["toggle", "read-selection", "stop", "status", "test-voice"] {
        assert!(
            stdout.contains(command),
            "expected {command:?} in {stdout:?}"
        );
    }
}

#[test]
fn every_documented_command_parses() {
    let commands = [
        ("toggle", CliCommand::Toggle),
        ("read-selection", CliCommand::ReadSelection),
        ("stop", CliCommand::Stop),
        ("status", CliCommand::Status { json: false }),
        ("test-voice", CliCommand::TestVoice),
    ];

    for (name, expected) in commands {
        let cli = Cli::try_parse_from(["murmduck", name])
            .unwrap_or_else(|err| panic!("{name} should parse: {err}"));
        assert_eq!(cli.command, expected, "wrong command parsed for {name}");
    }
}

#[test]
fn status_accepts_an_optional_json_flag() {
    let cli = Cli::try_parse_from(["murmduck", "status", "--json"]).expect("status --json parses");
    assert_eq!(cli.command, CliCommand::Status { json: true });

    let cli = Cli::try_parse_from(["murmduck", "status"]).expect("status parses");
    assert_eq!(cli.command, CliCommand::Status { json: false });
}

#[test]
fn unknown_command_is_rejected() {
    assert!(Cli::try_parse_from(["murmduck", "not-a-command"]).is_err());
}

#[test]
fn read_selection_is_wired_to_the_selection_provider() {
    // Isolate from any real user config. On the headless CI host this fails
    // because wl-paste is absent; on the Wayland target it reports a count.
    // Either way it must no longer be the placeholder command.
    let dir = tempfile::tempdir().expect("temp dir");
    let output = run_isolated(
        &["read-selection"],
        dir.path(),
        std::time::Duration::from_secs(3),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !combined.contains("later milestone"),
        "read-selection must run the provider, not the placeholder: {combined:?}"
    );
    assert!(
        combined.contains("PRIMARY selection") || combined.contains("selection provider"),
        "expected a selection result or provider error: {combined:?}"
    );
    if output.status.success() {
        assert!(
            combined.contains("characters"),
            "a successful read must report a character count: {combined:?}"
        );
    }
}

/// Run the binary with a private runtime dir, failing (and killing it) if it
/// does not exit within `timeout` — a hung owner would hold the lock forever.
fn run_isolated(
    args: &[&str],
    dir: &std::path::Path,
    timeout: std::time::Duration,
) -> std::process::Output {
    use std::thread;
    use std::time::Instant;

    let runtime = dir.join("runtime");
    let empty_path = dir.join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty PATH dir");
    let mut child = murmduck()
        .args(args)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("PATH", &empty_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn binary");
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll child") {
            Some(_) => break,
            None if Instant::now() < deadline => {
                thread::sleep(std::time::Duration::from_millis(10))
            }
            None => {
                let _ = child.kill();
                panic!("murmduck {args:?} did not exit within {timeout:?}");
            }
        }
    }
    child.wait_with_output().expect("collect output")
}

#[test]
fn stop_and_status_are_noops_when_no_session_is_active() {
    let dir = tempfile::tempdir().expect("temp dir");
    let timeout = std::time::Duration::from_secs(3);

    let stop = run_isolated(&["stop"], dir.path(), timeout);
    assert!(
        stop.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        String::from_utf8_lossy(&stop.stdout).contains("No active session"),
        "stop should report a no-op: {}",
        String::from_utf8_lossy(&stop.stdout)
    );

    let status = run_isolated(&["status"], dir.path(), timeout);
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("Inactive"),
        "status should report Inactive: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    let status_json = run_isolated(&["status", "--json"], dir.path(), timeout);
    assert!(
        status_json.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status_json.stderr)
    );
    let stdout = String::from_utf8_lossy(&status_json.stdout);
    assert!(
        stdout.contains("\"state\":\"Inactive\""),
        "status --json should emit structured state: {stdout:?}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_ok(),
        "status --json output must be valid JSON: {stdout:?}"
    );
}

#[test]
fn stop_and_status_do_not_require_the_api_key_to_be_set() {
    // `stop` and `status` never contact the TTS server. A configured
    // `api_key_env` whose variable is absent must not prevent them from
    // reaching an existing owner (or reporting an idle session).
    let dir = tempfile::tempdir().expect("temp dir");
    let config_dir = dir.path().join("murmduck");
    std::fs::create_dir(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "[tts]\napi_key_env = \"MURMDUCK_TEST_DEFINITELY_UNSET_KEY_9F3A\"\n",
    )
    .expect("write config");
    let timeout = std::time::Duration::from_secs(3);

    for args in [&["stop"][..], &["status"][..], &["status", "--json"][..]] {
        let output = run_isolated(args, dir.path(), timeout);
        assert!(
            output.status.success(),
            "murmduck {args:?} should not need TTS credentials; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn toggle_without_credentials_stops_an_active_owner() {
    // A second `toggle` only sends Stop over IPC. It must reach the owner even
    // when this invocation has no TTS credentials: resolution belongs to the
    // process that actually starts a reading (review re-review N1).
    use std::sync::{Arc, Mutex};

    use murmduck::session::SessionControl;
    use murmduck::session::SessionState;
    use murmduck::session::ipc::ControlServer;
    use murmduck::session::lock::InstanceLock;
    use murmduck::session::paths::RuntimePaths;

    let dir = tempfile::tempdir().expect("temp dir");
    let config_dir = dir.path().join("murmduck");
    std::fs::create_dir(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "[tts]\napi_key_env = \"MURMDUCK_TEST_UNSET_KEY_7C2E\"\n",
    )
    .expect("write config");

    let runtime = RuntimePaths::new(dir.path().join("runtime").join("murmduck"));
    let guard = InstanceLock::try_acquire(&runtime.lock)
        .expect("acquire lock")
        .expect("become fake owner");
    let listener =
        std::os::unix::net::UnixListener::bind(&runtime.socket).expect("bind control socket");
    let control = SessionControl::new();
    let mut server = ControlServer::spawn_with_control(
        listener,
        Arc::new(Mutex::new(SessionState::Playing)),
        control.clone(),
    );

    // `run_isolated` gives the child a private config/runtime dir and an empty
    // PATH; the configured API-key variable is deliberately never set.
    let output = run_isolated(&["toggle"], dir.path(), std::time::Duration::from_secs(3));

    assert!(
        output.status.success(),
        "toggle must reach the owner without credentials; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Stopping"),
        "the owner must be told to stop: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        control.stop_requested(),
        "the fake owner must have received Stop"
    );

    server.shutdown();
    drop(guard);
}

#[test]
fn stop_and_status_bypass_an_invalid_config_while_an_owner_is_active() {
    // Control-only commands need only XDG_RUNTIME_DIR and the socket. An
    // invalid TOML edit during playback must not block stopping the owner
    // (review re-review N4).
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    use murmduck::session::SessionState;
    use murmduck::session::lock::InstanceLock;
    use murmduck::session::paths::RuntimePaths;
    use murmduck::session::protocol::{self, Response};

    let dir = tempfile::tempdir().expect("temp dir");
    let config_dir = dir.path().join("murmduck");
    std::fs::create_dir(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("config.toml"), "[tts]\nspeed = 0.0\n")
        .expect("write deliberately invalid config");

    let runtime = RuntimePaths::new(dir.path().join("runtime").join("murmduck"));
    let guard = InstanceLock::try_acquire(&runtime.lock)
        .expect("acquire lock")
        .expect("become fake owner");
    let listener = UnixListener::bind(&runtime.socket).expect("bind control socket");
    let owner = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept command");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).expect("read command");
            let frame = protocol::encode_response(Response::Ok {
                state: SessionState::Stopping,
            });
            stream.write_all(frame.as_bytes()).expect("write response");
        }
        drop(guard);
    });

    let timeout = std::time::Duration::from_secs(3);
    let stop = run_isolated(&["stop"], dir.path(), timeout);
    assert!(
        stop.status.success(),
        "stop must bypass config parsing; stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        String::from_utf8_lossy(&stop.stdout).contains("Stopping"),
        "stop must reach the owner: {}",
        String::from_utf8_lossy(&stop.stdout)
    );

    let status = run_isolated(&["status"], dir.path(), timeout);
    assert!(
        status.status.success(),
        "status must bypass config parsing; stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("Stopping"),
        "status must reach the owner: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    owner.join().expect("fake owner joins");
}

#[test]
fn a_declined_control_command_makes_the_cli_exit_with_failure() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    use murmduck::session::lock::InstanceLock;
    use murmduck::session::paths::RuntimePaths;
    use murmduck::session::protocol::{self, Response};

    let dir = tempfile::tempdir().expect("temp dir");
    let paths = RuntimePaths::new(dir.path().join("runtime").join("murmduck"));
    let guard = InstanceLock::try_acquire(&paths.lock)
        .expect("acquire lock")
        .expect("become fake owner");
    let listener = UnixListener::bind(&paths.socket).expect("bind control socket");

    // Stand in for an owner that refuses the command (e.g. a replacement
    // declined during shutdown). The wire protocol is unchanged; only the CLI's
    // exit status must now reflect the refusal.
    let owner = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept command");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let frame = protocol::encode_response(Response::Error {
            message: "declined by test".to_owned(),
        });
        stream.write_all(frame.as_bytes()).expect("write refusal");
        drop(stream);
        drop(guard);
    });

    let output = run_isolated(&["stop"], dir.path(), std::time::Duration::from_secs(3));
    owner.join().expect("fake owner joins");
    assert!(
        !output.status.success(),
        "a refused control command must exit nonzero; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("declined by test"),
        "the refusal must be reported: {stderr:?}"
    );
}

#[test]
fn toggle_exits_promptly_when_the_selection_provider_is_unavailable() {
    // An empty PATH guarantees the selection provider cannot start, so the
    // owner fails while reading the selection. It must report the error and
    // clean up, never hanging or leaving a live process behind.
    let dir = tempfile::tempdir().expect("temp dir");
    let timeout = std::time::Duration::from_secs(3);
    let output = run_isolated(&["toggle"], dir.path(), timeout);

    assert!(!output.status.success(), "a missing provider is an error");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("selection provider") || combined.contains("PRIMARY selection"),
        "expected a selection error, got {combined:?}"
    );
    let runtime = dir.path().join("runtime").join("murmduck");
    assert!(
        runtime.join("instance.lock").is_file(),
        "lock file is retained"
    );
    assert!(
        !runtime.join("control.sock").exists(),
        "socket is cleaned up"
    );
}

#[test]
fn invalid_config_fails_with_an_actionable_error() {
    use std::fs;

    let dir = tempfile::tempdir().expect("temp dir");
    let config_dir = dir.path().join("murmduck");
    fs::create_dir(&config_dir).expect("create config dir");
    fs::write(config_dir.join("config.toml"), "[tts]\nspeed = 0.0\n").expect("write config");

    let output = murmduck()
        .arg("toggle")
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .expect("run binary");

    assert!(!output.status.success(), "invalid config must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tts.speed"),
        "actionable error in {stderr:?}"
    );
}
