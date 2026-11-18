//! Spike (issue #5) acceptance tests for progressive MP3 playback.
//!
//! These exercise the public `murmduck::audio` seam, not GStreamer internals.
//! They use a `fakesink` so no audio device is required on the test host.

use std::fs;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use murmduck::audio::{AudioError, AudioSink, PlayerEvent, StreamConfig, StreamingMp3Player};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone.mp3");

fn fixture() -> Vec<u8> {
    fs::read(FIXTURE).expect("read MP3 fixture")
}

fn capture_config() -> StreamConfig {
    StreamConfig {
        volume: 1.0,
        max_bytes: 256 * 1024,
        max_latency: Duration::from_millis(20),
        capture_decoded: true,
    }
}

fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}

fn decode(chunks: &[&[u8]]) -> Vec<u8> {
    let player = StreamingMp3Player::new(AudioSink::Fake, capture_config()).expect("create player");
    player.start().expect("start pipeline");
    for chunk in chunks {
        player.push(chunk).expect("push chunk");
    }
    player.finish().expect("finish playback");
    player.captured_decoded()
}

#[test]
fn misaligned_chunks_decode_to_the_same_pcm_as_one_buffer() {
    let bytes = fixture();

    let baseline = decode(&[&bytes]);

    // Deliberately split inside MPEG frames: tiny chunks mixed with odd-sized
    // ones so no produced chunk lines up with an MP3 frame boundary.
    let pattern = [7usize, 13, 3, 511, 29, 101, 1, 257, 17, 71];
    let mut offset = 0;
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut index = 0;
    while offset < bytes.len() {
        let size = pattern[index % pattern.len()].min(bytes.len() - offset);
        chunks.push(&bytes[offset..offset + size]);
        offset += size;
        index += 1;
    }
    let chunked = decode(&chunks);

    assert!(!baseline.is_empty(), "decoder produced no PCM");
    assert_eq!(chunked, baseline, "chunking changed the decoded audio");
}

#[test]
fn playback_starts_before_the_full_stream_is_pushed() {
    let bytes = fixture();
    let split = bytes.len() / 2;
    let player = StreamingMp3Player::new(AudioSink::Fake, capture_config()).expect("create player");
    player.start().expect("start pipeline");

    player.push(&bytes[..split]).expect("push first half");
    let started = wait_for(
        || player.events().contains(&PlayerEvent::Started),
        Duration::from_secs(5),
    );
    assert!(
        started,
        "audio must reach the sink while half the stream is still unpushed"
    );

    player.push(&bytes[split..]).expect("push second half");
    let stats = player.finish().expect("finish playback");
    assert!(stats.decoded_bytes > 0, "no decoded audio was produced");
}

#[test]
fn an_underrun_is_reported_and_playback_recovers_cleanly() {
    let bytes = fixture();
    let baseline = decode(&[&bytes]);
    let split = bytes.len() / 3;
    let config = StreamConfig {
        volume: 1.0,
        max_bytes: 1024,
        max_latency: Duration::from_millis(5),
        capture_decoded: true,
    };
    let player =
        StreamingMp3Player::new(AudioSink::SyncedFake, config).expect("create synced player");
    player.start().expect("start pipeline");

    for chunk in bytes[..split].chunks(256) {
        player.push(chunk).expect("push prefix");
    }
    assert!(wait_for(
        || player.events().contains(&PlayerEvent::Started),
        Duration::from_secs(5)
    ));
    let before = player.stats().underruns;
    assert!(wait_for(
        || player.buffered_bytes() == 0,
        Duration::from_secs(5)
    ));

    std::thread::sleep(Duration::from_millis(150));
    let after = player.stats().underruns;
    assert!(
        after > before,
        "a feed stall should be reported as an underrun (before={before}, after={after})"
    );
    assert!(
        player.events().contains(&PlayerEvent::Underrun),
        "the underrun should be observable as an event"
    );

    for chunk in bytes[split..].chunks(512) {
        player.push(chunk).expect("push rest");
    }
    player.finish().expect("finish after underrun");
    assert_eq!(
        player.captured_decoded(),
        baseline,
        "an underrun must not corrupt or truncate decoded audio"
    );
}

#[test]
fn cancellation_unblocks_the_feed_and_releases_the_sink() {
    let bytes = fixture();
    let long_stream = bytes.repeat(16); // ~32 s at 64 kbps: push will block
    let config = StreamConfig {
        max_bytes: 2048,
        capture_decoded: false,
        ..StreamConfig::default()
    };
    let player = Arc::new(
        StreamingMp3Player::new(AudioSink::SyncedFake, config).expect("create synced player"),
    );
    player.start().expect("start pipeline");

    let feeder = Arc::clone(&player);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result: Result<(), AudioError> = (|| {
            for chunk in long_stream.chunks(256) {
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
        .expect("a blocked push must return promptly on cancel");
    assert!(push_error.is_some(), "push should report cancellation");
    assert!(
        player.is_torn_down(),
        "pipeline and sink must be Null after cancel"
    );
    assert!(
        player.events().contains(&PlayerEvent::Cancelled),
        "cancellation should be observable as an event"
    );

    // A new session can start straight away: nothing is left holding the device.
    let next = StreamingMp3Player::new(AudioSink::Fake, capture_config()).expect("new player");
    next.start().expect("new start");
    next.push(&bytes).expect("new push");
    let stats = next.finish().expect("new finish");
    assert!(stats.decoded_bytes > 0, "new session did not play");
}

#[test]
fn buffering_stays_bounded_when_the_sink_is_slower_than_the_feed() {
    const MAX_BYTES: u64 = 2048;
    const CHUNK: usize = 512;
    let bytes = fixture();
    let config = StreamConfig {
        max_bytes: MAX_BYTES,
        capture_decoded: false,
        ..StreamConfig::default()
    };
    let player =
        StreamingMp3Player::new(AudioSink::SyncedFake, config).expect("create synced player");
    player.start().expect("start pipeline");

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
    assert!(stats.decoded_bytes > 0, "no decoded audio was produced");
}

#[test]
fn configured_volume_is_applied_to_the_pipeline() {
    // §8: audio.volume is a real playback setting, not a parsed no-op.
    let player = StreamingMp3Player::new(
        AudioSink::Fake,
        StreamConfig {
            volume: 0.42,
            ..StreamConfig::default()
        },
    )
    .expect("create player with a configured volume");
    assert!(
        (player.volume() - 0.42).abs() < 1e-6,
        "the configured volume must reach the audio pipeline: {}",
        player.volume()
    );

    let default_player =
        StreamingMp3Player::new(AudioSink::Fake, StreamConfig::default()).expect("default player");
    assert_eq!(default_player.volume(), 1.0);
}

#[test]
fn has_started_tracks_the_playback_probe_without_reading_events() {
    let player = StreamingMp3Player::new(AudioSink::Fake, capture_config()).expect("create player");
    assert!(!player.has_started(), "not started before playback");
    player.start().expect("start pipeline");
    for chunk in fixture().chunks(512) {
        player.push(chunk).expect("push chunk");
    }
    assert!(
        wait_for(|| player.has_started(), Duration::from_secs(2)),
        "has_started() must become true once decoded audio reaches the sink"
    );
    player.finish().expect("finish playback");
}
