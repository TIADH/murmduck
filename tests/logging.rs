use std::io;
use std::sync::{Arc, Mutex};

use murmduck::config::Config;
use murmduck::logging;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

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

fn capture_at(level: LevelFilter, emit: impl FnOnce()) -> String {
    let sink = Capture::default();
    let subscriber = logging::make_subscriber(level, sink.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    emit();
    sink.text()
}

#[test]
fn configured_level_filters_lower_priority_events() {
    let output = capture_at(LevelFilter::WARN, || {
        tracing::info!("info-event-should-be-hidden");
        tracing::warn!("warn-event-should-be-shown");
    });

    assert!(
        !output.contains("info-event-should-be-hidden"),
        "{output:?}"
    );
    assert!(output.contains("warn-event-should-be-shown"), "{output:?}");
}

#[test]
fn off_level_silences_every_event() {
    let output = capture_at(LevelFilter::OFF, || {
        tracing::error!("even-errors-are-hidden");
    });

    assert!(!output.contains("even-errors-are-hidden"), "{output:?}");
}

#[test]
fn trace_level_allows_verbose_events() {
    let output = capture_at(LevelFilter::TRACE, || {
        tracing::debug!("debug-event-is-visible");
    });

    assert!(output.contains("debug-event-is-visible"), "{output:?}");
}

#[test]
fn startup_log_includes_safe_context_but_not_the_api_key() {
    let config = Config::default();
    let secret = "SUPER_SECRET_API_KEY_98341";

    let output = capture_at(LevelFilter::INFO, || {
        logging::log_startup(&config, "9.9.9", Some(secret));
    });

    assert!(output.contains("murmduck starting"), "{output:?}");
    assert!(output.contains("9.9.9"), "{output:?}");
    assert!(output.contains("<redacted>"), "{output:?}");
    assert!(
        !output.contains(secret),
        "API key leaked into logs: {output:?}"
    );
}

#[test]
fn selection_summary_never_logs_the_selected_text() {
    let text = "HIGHLY_SENSITIVE_SELECTED_TEXT_42";

    let output = capture_at(LevelFilter::TRACE, || {
        logging::log_selection_summary(text);
    });

    assert!(output.contains("selection received"), "{output:?}");
    assert!(
        !output.contains(text),
        "selected text leaked into logs: {output:?}"
    );
}
