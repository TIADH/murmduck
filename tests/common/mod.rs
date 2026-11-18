//! Fixtures shared by integration tests. Each test crate uses a subset.
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// A streamed WAV with placeholder (`0xFFFFFFFF`) RIFF/data lengths and
/// deterministic 16-bit PCM — the shape emitted by TTS servers that do not
/// know the final audio size up front.
pub fn wav_stream(channels: u16, rate: u32, sample_count: usize) -> Vec<u8> {
    let block_align = channels as usize * 2;
    let data_len = sample_count * block_align;
    let mut bytes = Vec::with_capacity(44 + data_len);

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&((rate as usize * block_align) as u32).to_le_bytes());
    bytes.extend_from_slice(&(block_align as u16).to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());

    for i in 0..sample_count {
        let phase = 2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64;
        let sample = (8000.0 * phase.sin()) as i16;
        for _ in 0..channels {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    bytes
}

/// Poll `condition` until it holds or `timeout` elapses.
pub fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}
