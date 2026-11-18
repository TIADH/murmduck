//! OpenAI-compatible streaming TTS client.
//!
//! This module owns only the HTTP boundary: it builds the `/audio/speech`
//! request from configuration and streams the audio response bytes into a
//! caller-supplied [`std::io::Write`]. It has no knowledge of selection text
//! storage or GStreamer — those stay in their own modules (spec §5).
//!
//! The request runs on a small Tokio runtime so it can be *aborted* (not merely
//! ignored) the moment cancellation is signalled: dropping an in-flight
//! `reqwest` request closes the connection, which is what makes a second
//! `toggle` responsive while the server is connecting or stalling mid-stream.

use std::error::Error;
use std::fmt;
use std::io::Write;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;

use crate::cancellation::Cancellation;
use crate::config::{ResponseFormat, TtsConfig};

/// Time allowed to establish a connection to the TTS server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall time allowed for a request and its streamed body. Bounds a server
/// that accepts a request but never finishes responding when no one cancels.
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
    /// A successful status carried no audio bytes at all (e.g. 204 or an empty
    /// 200). Previously this was reported as a completed reading.
    EmptyResponse,
    /// The server's `Content-Type` clearly did not describe audio. A missing
    /// header or `application/octet-stream` is accepted where practical.
    UnexpectedContentType { value: String },
    /// The request was aborted because the session was cancelled.
    Cancelled,
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request { detail } => write!(f, "TTS request failed: {detail}"),
            Self::Http { status } => write!(f, "TTS server returned HTTP {status}"),
            Self::Stream { detail } => write!(f, "TTS response stream failed: {detail}"),
            Self::EmptyResponse => write!(f, "TTS server returned an empty audio response"),
            Self::UnexpectedContentType { value } => write!(
                f,
                "TTS server returned unexpected content type {value:?}; expected an audio content type"
            ),
            Self::Cancelled => write!(f, "TTS request cancelled"),
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
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
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
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| TtsError::Request {
                detail: err.to_string(),
            })?;
        // A dedicated multi-thread runtime lets the streaming writer block on
        // GStreamer back-pressure without preventing the cancellation token
        // from being observed.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|err| TtsError::Request {
                detail: format!("cannot start async runtime: {err}"),
            })?;

        let response_format = match config.response_format {
            ResponseFormat::Mp3 => "mp3",
            ResponseFormat::Wav => "wav",
        };

        Ok(Self {
            http,
            runtime,
            url: format!("{}/audio/speech", config.base_url.trim_end_matches('/')),
            model: config.model.clone(),
            voice: config.voice.clone(),
            response_format,
            speed: config.speed,
            api_key,
        })
    }

    /// Send `text` to the TTS endpoint and stream the response body into
    /// `sink` as it arrives. Aborts the request promptly when `cancel` is
    /// signalled — whether the request is still connecting or already
    /// streaming — so the caller's sink sees no bytes after cancellation.
    pub fn speak_to_cancellable(
        &self,
        text: &str,
        sink: &mut dyn Write,
        cancel: &Cancellation,
    ) -> Result<u64, TtsError> {
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

        let token = cancel.token();
        self.runtime.block_on(async {
            let response = tokio::select! {
                result = request.send() => {
                    result.map_err(|err| TtsError::Request { detail: err.to_string() })?
                }
                _ = token.cancelled() => return Err(TtsError::Cancelled),
            };

            let status = response.status();
            if !status.is_success() {
                return Err(TtsError::Http {
                    status: status.as_u16(),
                });
            }
            // Validate the declared content type where practical: accept audio
            // types and generic binary streams, but reject clearly textual or
            // JSON responses that a 2xx status alone would let through.
            if let Some(value) = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or(value)
                        .trim()
                        .to_ascii_lowercase()
                })
                && !(value.starts_with("audio/") || value == "application/octet-stream")
            {
                return Err(TtsError::UnexpectedContentType { value });
            }
            if cancel.is_cancelled() {
                return Err(TtsError::Cancelled);
            }

            let mut stream = response.bytes_stream();
            let mut total = 0u64;
            loop {
                let next = tokio::select! {
                    item = stream.next() => item,
                    _ = token.cancelled() => return Err(TtsError::Cancelled),
                };
                let Some(item) = next else {
                    break;
                };
                let chunk = item.map_err(|err| TtsError::Stream {
                    detail: err.to_string(),
                })?;
                if let Err(err) = sink.write_all(&chunk) {
                    return Err(if cancel.is_cancelled() {
                        TtsError::Cancelled
                    } else {
                        TtsError::Stream {
                            detail: err.to_string(),
                        }
                    });
                }
                total += chunk.len() as u64;
            }
            if total == 0 {
                return Err(TtsError::EmptyResponse);
            }
            Ok(total)
        })
    }
}
