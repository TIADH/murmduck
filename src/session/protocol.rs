//! Versioned, newline-delimited control protocol.
//!
//! One JSON object per line travels in each direction. Every frame carries a
//! [`PROTOCOL_VERSION`] so a mismatched peer is rejected rather than silently
//! misinterpreted. Selected text and API keys are never part of any frame.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::SessionState;

/// Wire-format version. Bump when frames change incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// A command sent by a control client to the session owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Command {
    /// Stop the active session.
    Stop,
    /// Replace the current reading with a fresh PRIMARY selection.
    ReplaceSelection,
    /// Replace the current reading with the predefined test-voice sample.
    ReplaceTestVoice,
    /// Ask for the owner's current lifecycle state.
    Status,
}

/// The owner's reply to a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Response {
    /// The command was accepted; `state` is the owner's state afterwards.
    Ok { state: SessionState },
    /// The command could not be handled.
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub version: u32,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub version: u32,
    pub response: Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The frame was empty or contained no complete JSON object.
    Empty,
    /// The frame was not valid JSON for the expected shape.
    Malformed(String),
    /// The peer spoke a different protocol version.
    UnsupportedVersion { expected: u32, actual: u32 },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty control protocol frame"),
            Self::Malformed(detail) => write!(f, "malformed control protocol frame: {detail}"),
            Self::UnsupportedVersion { expected, actual } => {
                write!(
                    f,
                    "unsupported protocol version {actual} (expected {expected})"
                )
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Encode a command as one newline-terminated frame.
///
/// Serializing these fixed shapes cannot fail, so the newline-terminated string
/// is returned directly.
pub fn encode_command(command: Command) -> String {
    let envelope = CommandEnvelope {
        version: PROTOCOL_VERSION,
        command,
    };
    let mut line = serde_json::to_string(&envelope).expect("command envelope is serializable");
    line.push('\n');
    line
}

/// Decode one command frame (trailing newline optional).
pub fn decode_command(line: &[u8]) -> Result<CommandEnvelope, ProtocolError> {
    let envelope: CommandEnvelope = parse(line)?;
    check_version(envelope.version)?;
    Ok(envelope)
}

/// Encode a response as one newline-terminated frame.
pub fn encode_response(response: Response) -> String {
    let envelope = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        response,
    };
    let mut line = serde_json::to_string(&envelope).expect("response envelope is serializable");
    line.push('\n');
    line
}

/// Decode one response frame (trailing newline optional).
pub fn decode_response(line: &[u8]) -> Result<ResponseEnvelope, ProtocolError> {
    let envelope: ResponseEnvelope = parse(line)?;
    check_version(envelope.version)?;
    Ok(envelope)
}

fn parse<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, ProtocolError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Err(ProtocolError::Empty);
    }
    serde_json::from_slice(line).map_err(|err| ProtocolError::Malformed(err.to_string()))
}

fn check_version(actual: u32) -> Result<(), ProtocolError> {
    if actual == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion {
            expected: PROTOCOL_VERSION,
            actual,
        })
    }
}
