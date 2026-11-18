//! Stream a local MP3 in deliberately misaligned chunks.
//!
//! This example is the manual counterpart to `tests/streaming_audio.rs`: it
//! drives the same `murmduck::audio` seam through a real audio sink, so the
//! audible result can be checked on a desktop host. `--fake` runs it without a
//! device (useful on headless machines).
//!
//! ```text
//! cargo run --example stream_mp3 -- tests/fixtures/tone.mp3
//! cargo run --example stream_mp3 -- tests/fixtures/tone.mp3 --fake
//! ```

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::time::Duration;

use murmduck::audio::{AudioSink, StreamConfig, StreamingMp3Player};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: stream_mp3 <file.mp3> [--fake]",
        )
    })?;
    let mut sink = AudioSink::Auto;
    let mut fake = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fake" => fake = true,
            "--sink" => {
                let name = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--sink needs a factory name")
                })?;
                sink = AudioSink::Factory(name);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {other:?}"),
                )
                .into());
            }
        }
    }
    if fake {
        sink = AudioSink::Fake;
    }

    let config = StreamConfig {
        volume: 1.0,
        max_bytes: 64 * 1024,
        max_latency: Duration::from_millis(20),
        capture_decoded: false,
    };
    let player = StreamingMp3Player::new(sink, config)?;
    player.start()?;

    // Chunk sizes that rarely line up with MPEG frames, like HTTP chunks.
    let pattern = [4096usize, 777, 13, 2048, 5, 1024];
    let mut file = File::open(&path)?;
    let mut buffer = vec![0u8; 4096];
    let mut index = 0;
    loop {
        let wanted = pattern[index % pattern.len()];
        let mut filled = 0;
        while filled < wanted {
            let read = file.read(&mut buffer[filled..wanted])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        player.push(&buffer[..filled])?;
        index += 1;
    }

    let stats = player.finish()?;
    println!(
        "pushed {} bytes, decoded {} bytes, max buffered {} bytes, {} underruns",
        stats.bytes_pushed, stats.decoded_bytes, stats.max_buffered_bytes, stats.underruns
    );
    Ok(())
}
