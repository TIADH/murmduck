//! Structured logging setup and privacy-safe log helpers.
//!
//! The subscriber is built from the level in configuration. Helpers in this
//! module are the only place startup/selection information is logged; they
//! deliberately emit metadata only, never API keys or selected text.

use std::error::Error;
use std::io;

use tracing::Subscriber;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::config::Config;

/// Build a subscriber that filters at `level` and writes to `writer`.
///
/// Exposed so tests can capture output with an in-memory writer.
pub fn make_subscriber<W>(level: LevelFilter, writer: W) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(writer)
        .with_target(false)
        .finish()
}

/// Install the process-wide subscriber writing to stdout.
pub fn init(level: LevelFilter) -> Result<(), Box<dyn Error>> {
    tracing::subscriber::set_global_default(make_subscriber(level, io::stdout))?;
    Ok(())
}

/// Log startup context. The API key value, when supplied, is used only to
/// decide whether to print a redaction marker — it is never formatted into the
/// event.
pub fn log_startup(config: &Config, version: &str, api_key: Option<&str>) {
    let api_key_state = if api_key.is_some() {
        "<redacted>"
    } else {
        "none"
    };
    tracing::info!(
        version = version,
        level = %config.logging.level,
        api_key = api_key_state,
        "murmduck starting"
    );
}

/// Log that a selection was received, recording only its size.
///
/// The text itself is never included in the event.
pub fn log_selection_summary(text: &str) {
    tracing::debug!(
        char_count = text.chars().count(),
        byte_count = text.len(),
        "selection received"
    );
}
