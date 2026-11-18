//! Structured logging setup and privacy-safe log helpers.
//!
//! The subscriber is built from the level in configuration. Helpers in this
//! module are the only place startup/selection information is logged; they
//! deliberately emit metadata only, never API keys or selected text.

use std::error::Error;
use std::io;
use std::sync::{Arc, RwLock};

use tracing::Subscriber;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};

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

/// Handle for the level filter of an installed subscriber.
///
/// The control path installs logging before it knows whether this invocation is
/// a client or the owner, and before the owner has read `logging.level`. Keeping
/// a handle lets the owner tighten or widen the filter once the config is
/// available (review PR #10 fourth re-review V1).
#[derive(Clone, Debug)]
pub struct LoggingHandle(Arc<RwLock<LevelFilter>>);

impl LoggingHandle {
    /// Change the effective level. Subsequent events are filtered at `level`.
    pub fn set(&self, level: LevelFilter) {
        *self.0.write().unwrap_or_else(|err| err.into_inner()) = level;
    }

    /// The level currently in effect (diagnostics and tests).
    pub fn current(&self) -> LevelFilter {
        *self.0.read().unwrap_or_else(|err| err.into_inner())
    }
}

/// Build a subscriber whose level can be changed after installation.
///
/// The dynamic filter has no static max-level hint, so callsite interest is
/// re-evaluated per event and a later `LoggingHandle::set` takes effect. The
/// initial filter is INFO so control-client startup needs no configuration.
pub fn make_reloadable_subscriber<W>(
    writer: W,
) -> (impl Subscriber + Send + Sync + 'static, LoggingHandle)
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let handle = LoggingHandle(Arc::new(RwLock::new(LevelFilter::INFO)));
    let filter = {
        let handle = handle.clone();
        dynamic_filter_fn(
            move |metadata: &tracing::Metadata<'_>, _ctx: &Context<'_, Registry>| {
                let current = *handle.0.read().unwrap_or_else(|err| err.into_inner());
                current >= LevelFilter::from(*metadata.level())
            },
        )
    };
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_target(false)
        .with_filter(filter);
    (tracing_subscriber::registry().with(layer), handle)
}

/// Install the process-wide subscriber writing to stderr.
///
/// stdout is reserved for command results, so `status --json` emits a single
/// clean JSON document that scripts can parse.
pub fn init(level: LevelFilter) -> Result<LoggingHandle, Box<dyn Error>> {
    let (subscriber, handle) = make_reloadable_subscriber(io::stderr);
    handle.set(level);
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(handle)
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
