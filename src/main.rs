use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use murmduck::cli::{Cli, Command};
use murmduck::session::body::NoopBody;
use murmduck::session::body::PlaceholderSession;
use murmduck::session::coordinator::{self, ClientRetry};
use murmduck::session::paths::RuntimePaths;
use murmduck::{config, logging, selection};

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
    let api_key = config.tts.resolve_api_key()?;
    let level = config.logging.level_filter()?;

    logging::init(level)?;
    tracing::debug!(path = %path.display(), "configuration loaded");
    logging::log_startup(&config, env!("CARGO_PKG_VERSION"), api_key.as_deref());

    dispatch(&config, cli.command)
}

/// Route a parsed command.
///
/// `toggle`, `stop` and `status` go through the session coordinator, which
/// makes exactly one invocation the owner (holding the lock and serving the
/// control socket) and turns the rest into control clients. `read-selection`
/// and `test-voice` keep their current behaviour until their own milestones.
fn dispatch(config: &config::Config, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Toggle => {
            let runtime = RuntimePaths::from_env()?;
            let timeout = Duration::from_millis(config.selection.timeout_ms);
            let source = selection::WaylandPrimary::new(timeout);
            let body = PlaceholderSession::new(source, config.selection.clone());
            let outcome = coordinator::run(&runtime, command, body, &ClientRetry::default())?;
            println!("{}", outcome.report());
        }
        Command::Stop | Command::Status => {
            let runtime = RuntimePaths::from_env()?;
            let outcome = coordinator::run(&runtime, command, NoopBody, &ClientRetry::default())?;
            println!("{}", outcome.report());
        }
        Command::ReadSelection => {
            let timeout = Duration::from_millis(config.selection.timeout_ms);
            let source = selection::WaylandPrimary::new(timeout);
            let selected = selection::read_selection(&source, &config.selection)?;
            println!("{}", selection::success_report(&selected));
        }
        Command::TestVoice => {
            tracing::info!("test-voice execution arrives in a later milestone");
        }
    }
    Ok(())
}
