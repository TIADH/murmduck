//! TTS HTTP-boundary seam: a small local server returns scripted successful
//! responses so validation of empty bodies and content types is testable
//! without GStreamer or a real TTS provider.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use murmduck::cancellation::Cancellation;
use murmduck::config::{ResponseFormat, TtsConfig};
use murmduck::tts::{TtsClient, TtsError};

mod common;
use common::wav_stream;

/// A complete raw HTTP response to send from the fake server.
struct Reply {
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn status(status: u16, content_type: Option<&str>, body: Vec<u8>) -> Self {
        let mut head = format!("HTTP/1.1 {status} Something\r\n");
        if let Some(value) = content_type {
            head.push_str(&format!("Content-Type: {value}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ));
        Self { head, body }
    }
}

/// Returns `(sink bytes, total, captured HTTP request)`.
fn speak_with_config(reply: Reply, tts: TtsConfig) -> Result<(Vec<u8>, u64, Vec<u8>), TtsError> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake TTS server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buf = Vec::new();
        let mut scratch = [0u8; 4096];
        // Read the full request, headers *and* JSON body, so the client can
        // finish writing before it reads the response.
        loop {
            let n = stream.read(&mut scratch).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&scratch[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&buf[..pos + 4]).to_ascii_lowercase();
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= pos + 4 + content_length {
                    break;
                }
            }
        }
        stream.write_all(reply.head.as_bytes()).expect("write head");
        stream.write_all(&reply.body).expect("write body");
        stream.flush().expect("flush");
        buf
    });

    let client = TtsClient::new(
        &TtsConfig {
            base_url: format!("{base_url}/v1"),
            ..tts
        },
        None,
    )
    .expect("build client");
    let mut sink = Vec::new();
    let result = client.speak_to_cancellable("sample text", &mut sink, &Cancellation::new());
    let request = server.join().expect("server joins");
    result.map(|total| (sink, total, request))
}

fn speak(reply: Reply) -> Result<(Vec<u8>, u64), TtsError> {
    speak_with_config(reply, TtsConfig::default()).map(|(sink, total, _)| (sink, total))
}

fn request_body(request: &[u8]) -> &[u8] {
    let pos = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("request has headers")
        + 4;
    &request[pos..]
}

#[test]
fn a_204_no_content_response_is_rejected() {
    let result = speak(Reply::status(204, None, Vec::new()));
    assert!(
        matches!(result, Err(TtsError::EmptyResponse)),
        "a 204 must not be treated as successful playback: {result:?}"
    );
}

#[test]
fn an_empty_200_response_is_rejected() {
    let result = speak(Reply::status(200, Some("audio/mpeg"), Vec::new()));
    assert!(
        matches!(result, Err(TtsError::EmptyResponse)),
        "a zero-byte 2xx body must be rejected: {result:?}"
    );
}

#[test]
fn an_unexpected_content_type_is_rejected() {
    let result = speak(Reply::status(
        200,
        Some("application/json"),
        b"{\"audio\":\"not really\"}".to_vec(),
    ));
    assert!(
        matches!(result, Err(TtsError::UnexpectedContentType { .. })),
        "a JSON response with a 200 status must not be played as audio: {result:?}"
    );
}

#[test]
fn a_successful_mp3_response_is_streamed_to_the_sink() {
    let body = b"ID3\x03fake-mp3-bytes".to_vec();
    let (sink, total) = speak(Reply::status(200, Some("audio/mpeg"), body.clone()))
        .expect("a real MP3 response succeeds");
    assert_eq!(sink, body);
    assert_eq!(total, body.len() as u64);
}

#[test]
fn configured_wav_format_is_sent_in_the_request() {
    let body = wav_stream(1, 48_000, 1);
    let (_, _, request) = speak_with_config(
        Reply::status(200, Some("audio/wav"), body),
        TtsConfig {
            response_format: ResponseFormat::Wav,
            ..TtsConfig::default()
        },
    )
    .expect("a configured WAV request succeeds");
    let payload: serde_json::Value =
        serde_json::from_slice(request_body(&request)).expect("request body is JSON");
    assert_eq!(payload["response_format"], "wav");
}

#[test]
fn default_config_still_sends_mp3_in_the_request() {
    let (_, _, request) = speak_with_config(
        Reply::status(200, Some("audio/mpeg"), b"ID3\x03mp3".to_vec()),
        TtsConfig::default(),
    )
    .expect("default request succeeds");
    let payload: serde_json::Value =
        serde_json::from_slice(request_body(&request)).expect("request body is JSON");
    assert_eq!(payload["response_format"], "mp3");
}

#[test]
fn an_audio_wav_content_type_is_accepted() {
    let body = wav_stream(1, 48_000, 1);
    let (sink, _, _) = speak_with_config(
        Reply::status(200, Some("audio/wav"), body.clone()),
        TtsConfig::default(),
    )
    .expect("audio/wav is an acceptable content type");
    assert_eq!(sink, body);
}

#[test]
fn an_octet_stream_content_type_is_accepted() {
    // Some OpenAI-compatible servers do not label MP3 specifically; a generic
    // binary content type is accepted where practical.
    let body = b"ID3\x03binary-audio".to_vec();
    let (sink, _) = speak(Reply::status(
        200,
        Some("application/octet-stream"),
        body.clone(),
    ))
    .expect("octet-stream is a practical MP3 content type");
    assert_eq!(sink, body);
}
