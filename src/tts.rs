//! OpenAI-compatible streaming TTS client.
//!
//! This module owns only the HTTP boundary: it builds the `/audio/speech`
//! request from configuration and streams the MP3 response bytes into a
//! caller-supplied [`std::io::Write`]. It has no knowledge of selection text
//! storage or GStreamer — those stay in their own modules (spec §5).
//!
//! Streaming uses `reqwest::blocking::Response`'s `Read` implementation through
//! `copy_to`, so bytes reach the writer as each HTTP chunk arrives rather than
//! after the whole body has been downloaded.

use std::error::Error;
use std::fmt;
use std::io::Write;
use std::time::Duration;

use serde::Serialize;

use crate::config::{ResponseFormat, TtsConfig};

/// Time allowed to establish a connection to the TTS server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall time allowed for a request and its streamed body. Bounds a server
/// that accepts a request but never finishes responding.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Errors from building or driving a TTS request. Messages contain only
/// metadata (status codes, transport diagnostics) — never the selected text
/// or the API key.
#[derive(Debug)]
pub enum TtsError {
    /// The request could not be built or sent (network/DNS/TLS failure).
    Request { detail: String },
    /// The server answered with a non-success HTTP status.
    Http { status: u16 },
    /// The response body could not be read or forwarded to the sink.
    Stream { detail: String },
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { detail } => write!(f, "TTS request failed: {detail}"),
            Self::Http { status } => write!(f, "TTS server returned HTTP {status}"),
            Self::Stream { detail } => write!(f, "TTS response stream failed: {detail}"),
        }
    }
}

impl Error for TtsError {}

/// Wire shape of `POST {base_url}/audio/speech` (spec §10).
#[derive(Debug, Serialize)]
struct SpeechRequest<'a> {
    model: &'a str,
    voice: &'a str,
    input: &'a str,
    response_format: &'a str,
    speed: f32,
}

/// A reusable client for one configured TTS server.
pub struct TtsClient {
    http: reqwest::blocking::Client,
    url: String,
    model: String,
    voice: String,
    response_format: &'static str,
    speed: f32,
    api_key: Option<String>,
}

impl TtsClient {
    /// Build a client from validated configuration. `api_key` is the resolved
    /// secret (already read from `api_key_env`); it is stored only to attach a
    /// Bearer header and is never logged.
    pub fn new(config: &TtsConfig, api_key: Option<String>) -> Result<Self, TtsError> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| TtsError::Request {
                detail: err.to_string(),
            })?;

        let response_format = match config.response_format {
            ResponseFormat::Mp3 => "mp3",
        };

        Ok(Self {
            http,
            url: format!("{}/audio/speech", config.base_url.trim_end_matches('/')),
            model: config.model.clone(),
            voice: config.voice.clone(),
            response_format,
            speed: config.speed,
            api_key,
        })
    }

    /// Send `text` to the TTS endpoint and stream the response body into
    /// `sink` as it arrives. Returns the number of bytes written.
    pub fn speak_to(&self, text: &str, sink: &mut dyn Write) -> Result<u64, TtsError> {
        let payload = SpeechRequest {
            model: &self.model,
            voice: &self.voice,
            input: text,
            response_format: self.response_format,
            speed: self.speed,
        };

        let mut request = self.http.post(&self.url).json(&payload);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let mut response = request.send().map_err(|err| TtsError::Request {
            detail: err.to_string(),
        })?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            return Err(TtsError::Http {
                status: status.as_u16(),
            });
        }

        response.copy_to(sink).map_err(|err| TtsError::Stream {
            detail: err.to_string(),
        })
    }
}
