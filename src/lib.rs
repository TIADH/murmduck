//! Murmduck library crate.
//!
//! Later milestones add selection, IPC, TTS and audio modules here. This
//! skeleton provides the CLI definition, configuration loading/validation,
//! and structured logging that those modules build on.

pub mod audio;
pub mod cancellation;
pub mod cli;
pub mod config;
pub mod logging;
pub mod selection;
pub mod session;
pub mod tts;
