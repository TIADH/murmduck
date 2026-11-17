use std::process::ExitCode;

use clap::Parser;
use murmduck::cli::{Cli, Command};
use murmduck::{config, logging};

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

    dispatch(cli.command);
    Ok(())
}

/// Route a parsed command. Execution is added by later milestones; the walking
/// skeleton accepts every documented command and reports it via tracing.
fn dispatch(command: Command) {
    let name = match command {
        Command::Toggle => "toggle",
        Command::ReadSelection => "read-selection",
        Command::Stop => "stop",
        Command::Status => "status",
        Command::TestVoice => "test-voice",
    };
    tracing::info!(
        command = name,
        "command accepted; execution arrives in a later milestone"
    );
}
