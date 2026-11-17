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
