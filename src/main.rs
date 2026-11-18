use std::process::ExitCode;

use clap::Parser;
use murmduck::audio::{AudioSink, StreamConfig};
use murmduck::cli::{Cli, Command};
use murmduck::selection::{self, SelectionSource, StaticSource, WaylandPrimary};
use murmduck::session::body::{NoopBody, ReadAloudSession, SessionBody, SessionContext};
use murmduck::session::coordinator::{self, ClientRetry};
use murmduck::session::paths::RuntimePaths;
use murmduck::session::routing;
use murmduck::session::{ReadingKind, SessionError, TeardownStep};
use murmduck::{config, logging};

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

/// Build a fresh reading for each attempt the session owner runs.
fn reading_factory(
    config: &config::Config,
    api_key: Option<String>,
) -> impl FnMut(ReadingKind) -> AppBody {
    let config = config.clone();
    move |kind| match kind {
        ReadingKind::Selection => AppBody::Selection(make_body(
            &config,
            api_key.clone(),
            WaylandPrimary::new(config.selection.timeout())
                .with_max_chars(config.selection.max_chars),
        )),
        ReadingKind::TestVoice => AppBody::TestVoice(make_body(
            &config,
            api_key.clone(),
            StaticSource::new(selection::TEST_VOICE_SAMPLE),
        )),
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
    // Configuration is located, loaded and validated before any selection,
    // network or audio work begins.
    let path = config::config_path()?;
    let config = config::Config::load(&path)?;
    let level = config.logging.level_filter()?;

    logging::init(level)?;
    tracing::debug!(path = %path.display(), "configuration loaded");

    // Credentials are only resolved by an invocation that will START a reading.
    // `stop` and `status` talk to an existing owner and never contact TTS, so a
    // missing `api_key_env` variable must not prevent them from working.
    let api_key = if routing::starts_session(cli.command) {
        config.tts.resolve_api_key()?
    } else {
        None
    };
    logging::log_startup(&config, env!("CARGO_PKG_VERSION"), api_key.as_deref());

    dispatch(&config, cli.command, api_key)
}

/// Route a parsed command through the session coordinator.
///
/// Every command that can produce audio becomes an owner when idle and a
/// control client when a session is active. `toggle`/`stop` request a stop;
/// `read-selection`/`test-voice` request replacement with a fresh reading.
fn dispatch(
    config: &config::Config,
    command: Command,
    api_key: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = RuntimePaths::from_env()?;
    let retry = ClientRetry::default();

    let outcome = if routing::starts_session(command) {
        let factory = reading_factory(config, api_key);
        coordinator::run_with_factory(&runtime, command, factory, &retry)?
    } else {
        match command {
            Command::Stop => coordinator::run(&runtime, Command::Stop, NoopBody, &retry)?,
            Command::Status { json } => {
                let outcome =
                    coordinator::run(&runtime, Command::Status { json }, NoopBody, &retry)?;
                if json {
                    println!("{}", outcome.report_json());
                } else {
                    println!("{}", outcome.report());
                }
                return Ok(());
            }
            // `starts_session` covers the three audio-producing commands.
            Command::Toggle | Command::ReadSelection | Command::TestVoice => unreachable!(),
        }
    };
    println!("{}", outcome.report());
    Ok(())
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
