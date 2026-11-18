//! Streaming WAV playback over the public `murmduck::audio` seam.
//!
//! The fixtures mimic OpenAI-compatible TTS servers that stream a RIFF header
//! with placeholder (`0xFFFFFFFF`) RIFF/data lengths followed by signed 16-bit
//! PCM. A `fakesink` keeps the tests device-free.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use murmduck::audio::{AudioSink, PlayerEvent, StreamConfig, StreamFormat, StreamingMp3Player};

mod common;
use common::{wav_stream, wait_for};

fn wav_player(config: StreamConfig) -> StreamingMp3Player {
    StreamingMp3Player::with_format(AudioSink::Fake, config, StreamFormat::Wav)
        .expect("create WAV player")
}

fn capture_config() -> StreamConfig {
    StreamConfig {
        max_bytes: 256 * 1024,
        capture_decoded: true,
        ..StreamConfig::default()
    }
}

fn decode(chunks: &[&[u8]]) -> Vec<u8> {
    let player = wav_player(capture_config());
    player.start().expect("start WAV pipeline");
    for chunk in chunks {
        player.push(chunk).expect("push chunk");
    }
    player.finish().expect("finish playback");
    player.captured_decoded()
}

#[test]
fn unknown_length_wav_decodes_identically_from_arbitrary_chunks() {
    let bytes = wav_stream(1, 48_000, 24_000);
    let baseline = decode(&[&bytes]);

    // Split inside the header as well as inside PCM samples.
    let pattern = [1usize, 7, 13, 3, 511, 29, 101, 17];
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut offset = 0;
    let mut index = 0;
    while offset < bytes.len() {
        let size = pattern[index % pattern.len()].min(bytes.len() - offset);
        chunks.push(&bytes[offset..offset + size]);
        offset += size;
        index += 1;
    }
    let chunked = decode(&chunks);

    assert!(!baseline.is_empty(), "WAV decoder produced no PCM");
    assert_eq!(chunked, baseline, "chunking changed the decoded WAV audio");
}

#[test]
fn wav_playback_starts_before_the_full_stream_is_pushed() {
    let bytes = wav_stream(1, 48_000, 24_000);
    let split = bytes.len() / 2;
    let player = wav_player(capture_config());
    player.start().expect("start WAV pipeline");

    player.push(&bytes[..split]).expect("push first half");
    assert!(
        wait_for(
            || player.events().contains(&PlayerEvent::Started),
            Duration::from_secs(5)
        ),
        "WAV audio must reach the sink while half the stream is still unpushed"
    );

    player.push(&bytes[split..]).expect("push second half");
    let stats = player.finish().expect("finish playback");
    assert!(stats.decoded_bytes > 0, "no WAV audio was decoded");
}

#[test]
fn wav_honours_source_channels_and_sample_width() {
    let channels = 2;
    let sample_count = 12_000;
    let bytes = wav_stream(channels, 44_100, sample_count);
    let decoded = decode(&[&bytes]);

    // 16-bit stereo PCM: exactly two bytes per channel per sample.
    assert_eq!(
        decoded.len(),
        sample_count * channels as usize * 2,
        "decoded PCM must preserve the WAV source's channels and sample width"
    );
}

#[test]
fn wav_cancellation_unblocks_the_feed_and_releases_the_sink() {
    let bytes = wav_stream(1, 48_000, 48_000 * 30);
    let config = StreamConfig {
        max_bytes: 2048,
        capture_decoded: false,
        ..StreamConfig::default()
    };
    let player = Arc::new(
        StreamingMp3Player::with_format(AudioSink::SyncedFake, config, StreamFormat::Wav)
            .expect("create synced WAV player"),
    );
    player.start().expect("start WAV pipeline");

    let feeder = Arc::clone(&player);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result: Result<(), murmduck::audio::AudioError> = (|| {
            for chunk in bytes.chunks(256) {
                feeder.push(chunk)?;
            }
            Ok(())
        })();
        let _ = tx.send(result.err().map(|err| err.to_string()));
    });

    assert!(wait_for(
        || player.events().contains(&PlayerEvent::Started),
        Duration::from_secs(5)
    ));
    player.cancel();

    let push_error = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a blocked WAV push must return promptly on cancel");
    assert!(push_error.is_some(), "push should report cancellation");
    assert!(
        player.is_torn_down(),
        "WAV pipeline and sink must be Null after cancel"
    );
}

#[test]
fn configured_volume_is_applied_on_the_wav_path() {
    let player = wav_player(StreamConfig {
        volume: 0.42,
        ..StreamConfig::default()
    });
    assert!(
        (player.volume() - 0.42).abs() < 1e-6,
        "configured volume must reach the WAV pipeline: {}",
        player.volume()
    );
}

#[test]
fn wav_buffering_stays_bounded_when_the_sink_is_slower_than_the_feed() {
    const MAX_BYTES: u64 = 2048;
    const CHUNK: usize = 512;
    let bytes = wav_stream(1, 48_000, 24_000);
    let config = StreamConfig {
        max_bytes: MAX_BYTES,
        capture_decoded: false,
        ..StreamConfig::default()
    };
    let player = StreamingMp3Player::with_format(AudioSink::SyncedFake, config, StreamFormat::Wav)
        .expect("create synced WAV player");
    player.start().expect("start WAV pipeline");

    for chunk in bytes.chunks(CHUNK) {
        player.push(chunk).expect("push chunk");
    }
    let stats = player.finish().expect("finish playback");

    assert_eq!(stats.bytes_pushed, bytes.len() as u64);
    assert!(
        stats.max_buffered_bytes <= MAX_BYTES + CHUNK as u64,
        "appsrc queued {} bytes with a {MAX_BYTES}-byte cap",
        stats.max_buffered_bytes
    );
    assert!(stats.decoded_bytes > 0, "no WAV audio was decoded");
}
