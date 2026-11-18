//! TTS HTTP-boundary seam: a small local server returns scripted successful
//! responses so validation of empty bodies and content types is testable
//! without GStreamer or a real TTS provider.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use murmduck::cancellation::Cancellation;
use murmduck::config::TtsConfig;
use murmduck::tts::{TtsClient, TtsError};

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

fn speak(reply: Reply) -> Result<(Vec<u8>, u64), TtsError> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake TTS server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buf = Vec::new();
        let mut scratch = [0u8; 4096];
        // Read the request headers (and any body) so the client can finish
        // writing before it reads the response.
        loop {
            let n = stream.read(&mut scratch).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&scratch[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        stream.write_all(reply.head.as_bytes()).expect("write head");
        stream.write_all(&reply.body).expect("write body");
        stream.flush().expect("flush");
    });

    let client = TtsClient::new(
        &TtsConfig {
            base_url: format!("{base_url}/v1"),
            ..TtsConfig::default()
        },
        None,
    )
    .expect("build client");
    let mut sink = Vec::new();
    let result = client.speak_to_cancellable("sample text", &mut sink, &Cancellation::new());
    server.join().expect("server joins");
    result.map(|total| (sink, total))
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
