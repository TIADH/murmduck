use std::process::ExitCode;

use clap::Parser;
use murmduck::audio::{AudioSink, StreamConfig};
use murmduck::cli::{Cli, Command};
use murmduck::selection::{self, SelectionSource, StaticSource, WaylandPrimary};
use murmduck::session::body::{NoopBody, ReadAloudSession, SessionBody, SessionContext};
use murmduck::session::coordinator::{self, ClientRetry, CommandOutcome};
use murmduck::session::paths::RuntimePaths;
use murmduck::session::{ReadingKind, SessionError, TeardownStep};
use murmduck::{config, logging};
use tracing::level_filters::LevelFilter;

/// Concrete bodies the real CLI can run. `read-selection`/`toggle` read the
/// PRIMARY selection; `test-voice` uses the fixed sample. A replacement picks
/// the variant from the control command, so switching between them works.
enum AppBody {
    Selection(ReadAloudSession<WaylandPrimary>),
    TestVoice(ReadAloudSession<StaticSource>),
}

impl SessionBody for AppBody {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        match self {
            Self::Selection(body) => body.run(ctx),
            Self::TestVoice(body) => body.run(ctx),
        }
    }

    fn record_teardown(&self, step: TeardownStep) {
        match self {
            Self::Selection(body) => body.record_teardown(step),
            Self::TestVoice(body) => body.record_teardown(step),
        }
    }
}

fn make_body<S: SelectionSource>(
    config: &config::Config,
    api_key: Option<String>,
    source: S,
) -> ReadAloudSession<S> {
    ReadAloudSession::new(
        source,
        config.selection.clone(),
        config.tts.clone(),
        api_key,
        AudioSink::Auto,
    )
    .with_stream_config(StreamConfig {
        // §8: audio.volume must reach the audio pipeline, not be a parsed no-op.
        volume: config.audio.volume,
        ..StreamConfig::default()
    })
}

/// Owner-only setup that loads and validates configuration the first time this
/// invocation actually becomes the session owner.
///
/// The coordinator invokes the factory only after the instance lock has been
/// won. A `toggle`/`read-selection`/`test-voice` that finds a live owner is just
/// a control client: it must not parse (or fail on) a malformed config before
/// forwarding its command over IPC (review re-review T1). Credentials stay
/// owner-only for the same reason, and both are cached so a replacement reuses
/// the same validated config without reloading the file.
struct OwnerSetup {
    config: Option<config::Config>,
    api_key: Option<Option<String>>,
    logging: logging::LoggingHandle,
}

impl OwnerSetup {
    fn new(logging: logging::LoggingHandle) -> Self {
        Self {
            config: None,
            api_key: None,
            logging,
        }
    }

    /// Build a fresh reading for one attempt of the session owner.
    fn body(&mut self, kind: ReadingKind) -> Result<AppBody, SessionError> {
        if self.config.is_none() {
            let path = config::config_path()?;
            let config = config::Config::load(&path)?;
            // Logging was installed at INFO for config-free client routing.
            // The owner now knows its config, so honour `logging.level` before
            // emitting any further events (review PR #10 re-review V1).
            self.logging.set(config.logging.level_filter()?);
            tracing::debug!(path = %path.display(), "configuration loaded");
            self.config = Some(config);
        }
        let config = self.config.as_ref().expect("config loaded above");
        if self.api_key.is_none() {
            let resolved = config.tts.resolve_api_key()?;
            logging::log_startup(config, env!("CARGO_PKG_VERSION"), resolved.as_deref());
            self.api_key = Some(resolved);
        }
        let key = self.api_key.clone().flatten();
        Ok(match kind {
            ReadingKind::Selection => AppBody::Selection(make_body(
                config,
                key,
                WaylandPrimary::new(config.selection.timeout())
                    .with_max_chars(config.selection.max_chars),
            )),
            ReadingKind::TestVoice => AppBody::TestVoice(make_body(
                config,
                key,
                StaticSource::new(selection::TEST_VOICE_SAMPLE),
            )),
        })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("murmduck: error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        // Control-only commands need the runtime dir/socket and nothing else.
        // They must keep working when the TOML config has been edited into an
        // invalid state during playback (review re-review N4).
        Command::Stop | Command::Status { .. } => run_control(cli.command),
        // These may produce audio, so config and credentials are resolved only
        // if this invocation actually becomes the owner. An invocation that
        // routes to a control client needs neither and stays config-independent.
        Command::Toggle | Command::ReadSelection | Command::TestVoice => run_owner(cli.command),
    }
}

/// Minimal path for commands that only talk to a live owner. No config file is
/// located or parsed and no credentials are resolved.
fn run_control(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    // Control routing must not depend on config, so the initial INFO filter is
    // all it needs; no reload handle is required on this path.
    let _logging = logging::init(LevelFilter::INFO)?;
    let runtime = RuntimePaths::from_env()?;
    let retry = ClientRetry::default();
    let outcome = coordinator::run(&runtime, command, NoopBody, &retry)?;
    print_outcome(command, outcome);
    Ok(())
}

/// Full path for an invocation that may become the session owner.
///
/// Logging is installed before role routing so a control-client invocation (the
/// second `toggle`) can reach a live owner even while the TOML is invalid. All
/// config/credential work is deferred to [`OwnerSetup`], which the coordinator
/// only runs once this process has won the lock and is genuinely the owner.
fn run_owner(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    // Install at INFO so a control-client invocation works with an invalid
    // TOML; the handle lets OwnerSetup apply the configured level once it wins
    // the lock and loads config.
    let logging = logging::init(LevelFilter::INFO)?;
    let runtime = RuntimePaths::from_env()?;
    let retry = ClientRetry::default();

    let mut setup = OwnerSetup::new(logging);
    let outcome =
        coordinator::run_with_fallible_factory(&runtime, command, |kind| setup.body(kind), &retry)?;
    print_outcome(command, outcome);
    Ok(())
}

fn print_outcome(command: Command, outcome: CommandOutcome) {
    if matches!(command, Command::Status { json: true }) {
        println!("{}", outcome.report_json());
    } else {
        println!("{}", outcome.report());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_audio_volume_reaches_the_reading_body() {
        let mut cfg = config::Config::default();
        cfg.audio.volume = 0.25;
        let body = make_body(&cfg, None, selection::StaticSource::new("sample"));
        assert_eq!(
            body.stream_config().volume,
            0.25,
            "audio.volume must be wired from config into the audio pipeline"
        );
    }
}
