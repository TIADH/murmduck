//! Command-line interface definition.
//!
//! Kept as a library module so command parsing can be tested without spawning
//! a process, while `main.rs` uses the same definition to drive the binary.

use clap::{Parser, Subcommand};

/// On-demand streaming text-to-speech for Linux.
#[derive(Debug, Parser)]
#[command(name = "murmduck", version, about, long_about = None)]
pub struct Cli {
    /// Action to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Actions supported by `murmduck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Start reading the PRIMARY selection, or stop the active session.
    Toggle,

    /// Read the current PRIMARY selection (replacing any active session).
    #[command(name = "read-selection")]
    ReadSelection,

    /// Stop the active reading session, if any.
    Stop,

    /// Report the current session status.
    Status {
        /// Emit machine-readable JSON instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },

    /// Play a short sample using the configured TTS server.
    #[command(name = "test-voice")]
    TestVoice,
}
