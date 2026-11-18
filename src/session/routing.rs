//! Single home for the command → wire command → reading-kind mapping.
//!
//! CLI commands are turned into wire commands in exactly one place
//! ([`wire_command`]), the kind of reading a fresh session starts is decided in
//! exactly one place ([`initial_reading_kind`]), and the kind a replacement
//! requests is decoded in exactly one place ([`replacement_kind`]). This keeps
//! `cli`, `coordinator`, `ipc` and `main` from growing parallel switches that
//! could drift apart.

use crate::cli::Command;

use super::control::ReadingKind;
use super::protocol::Command as WireCommand;

/// The wire command a control client sends for a CLI command.
pub fn wire_command(command: Command) -> WireCommand {
    match command {
        // A second `toggle` cancels the current session (spec §4).
        Command::Toggle | Command::Stop => WireCommand::Stop,
        Command::ReadSelection => WireCommand::ReplaceSelection,
        Command::TestVoice => WireCommand::ReplaceTestVoice,
        Command::Status { .. } => WireCommand::Status,
    }
}

/// The kind of reading an idle session should start for this CLI command.
pub fn initial_reading_kind(command: Command) -> ReadingKind {
    match command {
        Command::TestVoice => ReadingKind::TestVoice,
        Command::Toggle | Command::ReadSelection | Command::Stop | Command::Status { .. } => {
            ReadingKind::Selection
        }
    }
}

/// If `command` asks to replace the current reading, the kind to start next.
pub fn replacement_kind(command: WireCommand) -> Option<ReadingKind> {
    match command {
        WireCommand::ReplaceSelection => Some(ReadingKind::Selection),
        WireCommand::ReplaceTestVoice => Some(ReadingKind::TestVoice),
        WireCommand::Stop | WireCommand::Status => None,
    }
}
