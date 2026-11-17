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
        ("status", CliCommand::Status),
        ("test-voice", CliCommand::TestVoice),
    ];

    for (name, expected) in commands {
        let cli = Cli::try_parse_from(["murmduck", name])
            .unwrap_or_else(|err| panic!("{name} should parse: {err}"));
        assert_eq!(cli.command, expected, "wrong command parsed for {name}");
    }
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
    let output = murmduck()
        .arg("read-selection")
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .expect("run binary");
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
