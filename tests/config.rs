use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use murmduck::config::{self, Config, ResponseFormat, SelectionProvider};
use tracing::level_filters::LevelFilter;

fn config_in(body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    fs::write(&path, body).expect("write config");
    (dir, path)
}

fn assert_rejected(body: &str, expected_fragment: &str) {
    let (_dir, path) = config_in(body);
    let err = Config::load(&path).expect_err("config should be rejected");
    let message = err.to_string();
    assert!(
        message.contains(expected_fragment),
        "expected {expected_fragment:?} in error {message:?}"
    );
}

#[test]
fn missing_config_file_uses_documented_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("does-not-exist.toml");

    let config = Config::load(&path).expect("missing file is not an error");

    assert_eq!(config.tts.base_url, "http://localhost:8880/v1");
    assert_eq!(config.tts.api_key_env, None);
    assert_eq!(config.tts.model, "tts-1");
    assert_eq!(config.tts.voice, "alloy");
    assert_eq!(config.tts.speed, 1.0);
    assert_eq!(config.tts.response_format, ResponseFormat::Mp3);
    assert_eq!(config.audio.volume, 1.0);
    assert_eq!(config.selection.provider, SelectionProvider::WaylandPrimary);
    assert_eq!(config.selection.max_chars, 20_000);
    assert_eq!(config.selection.timeout_ms, 2_000);
    assert_eq!(config.logging.level, "info");
}

#[test]
fn partial_config_fills_omitted_keys_with_defaults() {
    let (_dir, path) = config_in(
        r#"
        [tts]
        voice = "nova"

        [logging]
        level = "debug"
        "#,
    );

    let config = Config::load(&path).expect("partial config loads");

    assert_eq!(config.tts.voice, "nova");
    assert_eq!(config.tts.model, "tts-1");
    assert_eq!(config.tts.base_url, "http://localhost:8880/v1");
    assert_eq!(config.audio.volume, 1.0);
    assert_eq!(config.logging.level, "debug");
}

#[test]
fn fully_specified_config_is_loaded() {
    let (_dir, path) = config_in(
        r#"
        [tts]
        base_url = "http://localhost:9999/v1"
        api_key_env = "MY_TTS_KEY"
        model = "tts-custom"
        voice = "onyx"
        speed = 1.25
        response_format = "mp3"

        [audio]
        volume = 0.5

        [selection]
        provider = "wayland-primary"
        max_chars = 1234
        timeout_ms = 567

        [logging]
        level = "warn"
        "#,
    );

    let config = Config::load(&path).expect("full config loads");

    assert_eq!(config.tts.base_url, "http://localhost:9999/v1");
    assert_eq!(config.tts.api_key_env.as_deref(), Some("MY_TTS_KEY"));
    assert_eq!(config.tts.model, "tts-custom");
    assert_eq!(config.tts.voice, "onyx");
    assert_eq!(config.tts.speed, 1.25);
    assert_eq!(config.audio.volume, 0.5);
    assert_eq!(config.selection.max_chars, 1_234);
    assert_eq!(config.selection.timeout_ms, 567);
    assert_eq!(config.logging.level, "warn");
}

#[test]
fn invalid_values_produce_actionable_errors() {
    assert_rejected("[tts]\nspeed = 0.0\n", "tts.speed");
    assert_rejected("[tts]\nspeed = -1.0\n", "tts.speed");
    assert_rejected("[tts]\nbase_url = \"ftp://example.com\"\n", "tts.base_url");
    assert_rejected("[tts]\nbase_url = \"localhost:1234\"\n", "tts.base_url");
    assert_rejected("[tts]\nmodel = \"\"\n", "tts.model");
    assert_rejected("[tts]\nvoice = \"\"\n", "tts.voice");
    assert_rejected("[tts]\nresponse_format = \"wav\"\n", "response_format");
    assert_rejected("[audio]\nvolume = 1.5\n", "audio.volume");
    assert_rejected("[selection]\nmax_chars = 0\n", "selection.max_chars");
    assert_rejected("[selection]\ntimeout_ms = 0\n", "selection.timeout_ms");
    assert_rejected("[logging]\nlevel = \"verbose\"\n", "logging.level");
}

#[test]
fn malformed_required_tts_values_name_the_offending_key() {
    // §8: a present-but-malformed TTS value must say *which* key is wrong.
    // A bare TOML parse dump ("invalid type: integer") does not.
    assert_rejected("[tts]\nbase_url = 42\n", "tts.base_url");
    assert_rejected("[tts]\nmodel = 42\n", "tts.model");
    assert_rejected("[tts]\nvoice = 42\n", "tts.voice");
    assert_rejected("[tts]\nspeed = \"fast\"\n", "tts.speed");
    assert_rejected("[tts]\nresponse_format = 42\n", "tts.response_format");
}

#[test]
fn malformed_toml_reports_the_config_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "this is not = toml =").expect("write config");

    let err = Config::load(&path).expect_err("malformed config is rejected");
    assert!(
        err.to_string().contains(path.to_string_lossy().as_ref()),
        "error should name the config file: {err}"
    );
}

#[test]
fn config_path_uses_xdg_config_home() {
    let path = config::config_path_from(
        Some(OsStr::new("/xdg/config")),
        Some(OsStr::new("/home/user")),
    )
    .expect("resolve path");
    assert_eq!(path, PathBuf::from("/xdg/config/murmduck/config.toml"));
}

#[test]
fn config_path_falls_back_to_home_config() {
    for xdg in [None, Some(OsStr::new(""))] {
        let path =
            config::config_path_from(xdg, Some(OsStr::new("/home/user"))).expect("resolve path");
        assert_eq!(
            path,
            PathBuf::from("/home/user/.config/murmduck/config.toml")
        );
    }
}

#[test]
fn config_path_requires_a_home_when_xdg_is_unset() {
    assert!(config::config_path_from(None, None).is_err());
}

#[test]
fn configured_log_level_is_parsed() {
    let config = Config::default();
    assert_eq!(config.logging.level_filter().unwrap(), LevelFilter::INFO);

    let (_dir, path) = config_in("[logging]\nlevel = \"trace\"\n");
    let config = Config::load(&path).expect("config loads");
    assert_eq!(config.logging.level_filter().unwrap(), LevelFilter::TRACE);
}

#[test]
fn config_is_reusable_across_paths() {
    // Guards against accidental global/cached config state as more modules
    // begin loading it on each invocation.
    let first = tempfile::tempdir().expect("temp dir");
    let second = tempfile::tempdir().expect("temp dir");
    fs::write(first.path().join("config.toml"), "[tts]\nvoice = \"a\"\n").unwrap();
    fs::write(second.path().join("config.toml"), "[tts]\nvoice = \"b\"\n").unwrap();

    let a = Config::load(&Path::new(first.path()).join("config.toml")).unwrap();
    let b = Config::load(&second.path().join("config.toml")).unwrap();
    assert_eq!(a.tts.voice, "a");
    assert_eq!(b.tts.voice, "b");
}

#[test]
fn absent_api_key_env_means_no_authentication() {
    let config = Config::default();
    assert_eq!(config.tts.resolve_api_key().unwrap(), None);
}

#[test]
fn empty_api_key_env_name_is_rejected() {
    let (_dir, path) = config_in("[tts]\napi_key_env = \"\"\n");
    let config = Config::load(&path).expect("loads structurally");
    let err = config
        .tts
        .resolve_api_key()
        .expect_err("empty name is invalid");
    assert!(err.to_string().contains("tts.api_key_env"), "{err}");
}
