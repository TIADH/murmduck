use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use murmduck::cli::{Cli, Command};
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

/// Route a parsed command. `read-selection` reads and validates the PRIMARY
/// selection; the remaining commands keep their walking-skeleton behaviour
/// until their own milestones.
fn dispatch(config: &config::Config, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ReadSelection => {
            let timeout = Duration::from_millis(config.selection.timeout_ms);
            let source = selection::WaylandPrimary::new(timeout);
            let selected = selection::read_selection(&source, &config.selection)?;
            println!("{}", selection::success_report(&selected));
        }
        other => {
            let name = match other {
                Command::Toggle => "toggle",
                Command::Stop => "stop",
                Command::Status => "status",
                Command::TestVoice => "test-voice",
                Command::ReadSelection => unreachable!("handled above"),
            };
            tracing::info!(
                command = name,
                "command accepted; execution arrives in a later milestone"
            );
        }
    }
    Ok(())
}
