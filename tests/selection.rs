use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use murmduck::config::SelectionConfig;
use murmduck::logging;
use murmduck::selection::{self, SelectionError, SelectionSource, WaylandPrimary};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Test double for the selection-source seam. It simply returns a canned
/// result; no external process or Wayland display is involved.
struct FakeSource {
    result: Result<String, SelectionError>,
}

impl FakeSource {
    fn with_text(text: &str) -> Self {
        Self {
            result: Ok(text.to_owned()),
        }
    }

    fn failing(error: SelectionError) -> Self {
        Self { result: Err(error) }
    }
}

impl SelectionSource for FakeSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        self.result.clone()
    }
}

fn config(max_chars: usize) -> SelectionConfig {
    SelectionConfig {
        max_chars,
        ..SelectionConfig::default()
    }
}

/// Build a provider source from an arbitrary command. Tests point this at
/// `printf`/`sh`/`sleep` to exercise subprocess handling without `wl-paste`.
fn command_source(program: &str, args: &[&str], timeout_ms: u64) -> WaylandPrimary {
    WaylandPrimary::with_command(program, args, Duration::from_millis(timeout_ms))
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("utf-8 log output")
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_valid_selection_is_returned_to_the_caller() {
    let source = FakeSource::with_text("Hello from the PRIMARY selection");
    let selected = selection::read_selection(&source, &config(200)).expect("valid selection");

    assert_eq!(selected.text(), "Hello from the PRIMARY selection");
    assert_eq!(selected.char_count(), 32);
}

#[test]
fn an_empty_selection_is_rejected() {
    let source = FakeSource::with_text("");
    let err = selection::read_selection(&source, &config(200)).expect_err("empty rejected");

    assert_eq!(err, SelectionError::Empty);
}

#[test]
fn a_selection_over_the_char_limit_is_rejected() {
    let source = FakeSource::with_text("abcd");
    let err = selection::read_selection(&source, &config(3)).expect_err("oversized rejected");

    assert_eq!(err, SelectionError::TooLong { max: 3, actual: 4 });
}

#[test]
fn the_char_limit_counts_unicode_characters_not_bytes() {
    // Four emoji are four characters but sixteen bytes. The limit is in chars.
    let source = FakeSource::with_text("🎵🎵🎵🎵");
    let err = selection::read_selection(&source, &config(3)).expect_err("oversized rejected");
    assert_eq!(err, SelectionError::TooLong { max: 3, actual: 4 });

    let at_limit = FakeSource::with_text("🎵🎵🎵");
    assert!(selection::read_selection(&at_limit, &config(3)).is_ok());
}

#[test]
fn provider_errors_are_propagated_unchanged() {
    let failure = SelectionError::ProviderFailed {
        status: "exit status: 1".to_owned(),
        detail: "boom".to_owned(),
    };
    let source = FakeSource::failing(failure.clone());

    let err = selection::read_selection(&source, &config(200)).expect_err("propagated");
    assert_eq!(err, failure);
}

#[test]
fn reading_a_selection_never_logs_its_text() {
    let secret = "ULTRA_SECRET_SELECTION_7788";
    let sink = Capture::default();
    let subscriber = logging::make_subscriber(LevelFilter::TRACE, sink.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let source = FakeSource::with_text(secret);
    let _ = selection::read_selection(&source, &config(200)).expect("valid selection");

    let output = sink.text();
    assert!(!output.contains(secret), "selected text leaked: {output:?}");
    assert!(
        output.contains("selection received"),
        "expected a size-only summary: {output:?}"
    );
}

#[test]
fn error_messages_never_contain_selection_text() {
    let message = SelectionError::TooLong {
        max: 10,
        actual: 25,
    }
    .to_string();
    assert!(
        message.contains("10") && message.contains("25"),
        "error should name the limit and the size: {message:?}"
    );

    let empty = SelectionError::Empty.to_string();
    assert!(
        empty.to_lowercase().contains("empty"),
        "error should explain the problem: {empty:?}"
    );
}

#[test]
fn the_success_report_shows_only_a_character_count() {
    let secret = "DO_NOT_PRINT_THIS_SELECTION_5566";
    let source = FakeSource::with_text(secret);
    let selected = selection::read_selection(&source, &config(200)).expect("valid selection");

    let report = selection::success_report(&selected);
    assert_eq!(
        report, "Read 32 characters from the PRIMARY selection.",
        "report should contain the count and nothing else: {report:?}"
    );
    assert!(
        !report.contains(secret),
        "text leaked into report: {report:?}"
    );
}

#[test]
fn the_provider_source_returns_the_text_it_reads() {
    let source = command_source("printf", &["hello primary"], 1_000);
    let text = source.read_text().expect("provider succeeds");

    assert_eq!(text, "hello primary");
}

#[test]
fn the_provider_source_preserves_utf8_text() {
    let source = command_source("printf", &["Việt Nam 🎵 —_selection"], 1_000);
    let text = source.read_text().expect("provider succeeds");

    assert_eq!(text, "Việt Nam 🎵 —_selection");
}

#[test]
fn an_empty_provider_output_is_empty_text() {
    let source = command_source("printf", &[""], 1_000);
    let text = source.read_text().expect("provider succeeds");

    assert_eq!(text, "");
    // Rejection of empty text is the orchestration layer's job.
    assert_eq!(
        selection::read_selection(&source, &config(200)).expect_err("empty rejected"),
        SelectionError::Empty
    );
}

#[test]
fn a_nonzero_provider_exit_is_surfaced_as_an_error() {
    let source = command_source(
        "sh",
        &["-c", "printf 'selected-secret'; printf 'boom' >&2; exit 3"],
        1_000,
    );
    let err = source.read_text().expect_err("non-zero exit is an error");

    match &err {
        SelectionError::ProviderFailed { status, detail } => {
            assert!(
                status.contains('3'),
                "status should name exit code: {status:?}"
            );
            assert!(
                detail.contains("boom"),
                "stderr should be captured: {detail:?}"
            );
        }
        other => panic!("expected ProviderFailed, got {other:?}"),
    }
    let message = err.to_string();
    assert!(
        !message.contains("selected-secret"),
        "stdout must not leak into an error: {message:?}"
    );
}

#[test]
fn non_utf8_provider_output_is_rejected() {
    // GNU printf interprets \377 as a single 0xFF byte, which is invalid UTF-8.
    let source = command_source("printf", &["\\377"], 1_000);
    let err = source.read_text().expect_err("invalid UTF-8 is rejected");

    assert_eq!(err, SelectionError::InvalidUtf8);
}

#[test]
fn a_timed_out_provider_is_terminated_and_reported() {
    // `sleep 30` never writes to stdout; without termination this test would
    // hang for thirty seconds.
    let source = command_source("sleep", &["30"], 120);
    let started = Instant::now();
    let err = source.read_text().expect_err("timeout is an error");
    let elapsed = started.elapsed();

    assert_eq!(
        err,
        SelectionError::Timeout {
            after: Duration::from_millis(120)
        }
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "provider should be killed promptly, took {elapsed:?}"
    );
}

#[test]
fn a_missing_provider_binary_is_a_clear_error() {
    let source = command_source("murmduck-no-such-provider-xyz", &[], 1_000);
    let err = source.read_text().expect_err("spawn failure is an error");

    assert!(matches!(err, SelectionError::Spawn { .. }), "{err:?}");
    assert!(
        err.to_string()
            .contains("could not start selection provider"),
        "{err}"
    );
}
