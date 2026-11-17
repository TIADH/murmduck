# Proceed with GStreamer `appsrc` for streaming MP3 playback

Status: accepted. Ticket #5 validated the backend proposed in the spec; ticket
#6 should build the real audio subsystem on `murmduck::audio::StreamingMp3Player`
rather than switching to a different decoder or output library.

## Considered options

- **GStreamer `appsrc` → `mpegaudioparse` → `mpg123audiodec` → `audioconvert` →
  `audioresample` → audio sink (chosen).** One pipeline consumes arbitrary byte
  chunks, frames MPEG audio, decodes incrementally and releases the device on
  `cancel()`. No per-chunk buffering in Rust code.
- **Pure-Rust decoder (e.g. Symphonia) plus a Rust output crate (cpal/rodio).**
  Avoids system libraries, but would reimplement framing, bounded back-pressure,
  device discovery and teardown that GStreamer already provides. Not worth the
  cost for the MVP.
- **Shelling out to `ffmpeg`/`mpg123` and piping PCM.** Simpler to start, but
  process lifecycle, cancellation and device release become fragile, and it adds
  an external binary dependency to a "lightweight" CLI.

## Validation

`cargo test --test streaming_audio` (seams: decoded-PCM capture, lifecycle
events, buffered-byte gauge, tear-down state) covers every spike criterion:

| Criterion | Result |
| --- | --- |
| Misaligned chunks decode correctly | Decoded PCM from chunks of 1–511 bytes is byte-for-byte identical to feeding the whole fixture at once. |
| Playback starts early | The first decoded buffer reaches the sink while half the fixture is still unpushed. |
| Bounded buffering | With a real-time fake sink and a 2 KiB cap, queued bytes never exceed the cap plus one chunk. |
| Underruns are handled | After the feed stalls and the queue drains, appsrc reports an underrun; resuming produces output identical to the uninterrupted baseline, with no error or crash. |
| Cancellation | `cancel()` unblocks a thread stuck in `push()`, moves both pipeline and sink to Null, and a fresh player starts and plays immediately. |

Three non-obvious settings are required and are now encoded in the module:
`appsrc` needs **`block = true`** for `max-bytes` to apply back-pressure (the
cap alone did not bound the queue — this was caught by a failing test),
**`min-percent`** must be non-zero for `need-data` (the underrun notification)
to be re-emitted as the queue drains, and **`do-timestamp` is left off** so the
framer/decoder derive timestamps from MPEG frame counts — stamping with the
wall-clock push time bunches up when data is fed faster than real time.
GStreamer 1.28 has no dedicated `underrun` signal, so `need-data` is used as the
underrun proxy. Waiting for `ASYNC_DONE` in `start()` is also wrong for an
`appsrc`: no data can be prerolled until the first chunk is pushed.

## Consequences and risks for ticket #6

- Keep `capture_decoded = false` in production; it exists only so tests can
  compare PCM and would otherwise grow without bound on long streams.
- `mpegaudioparse` accepts `audio/mpeg, mpegversion=1` and `mpg123audiodec`
  requires `parsed = true`; the framer supplies that. Servers emitting MPEG-2
  Layer III or a different format would need a different framer/decoder — the
  spec's MP3 response format is assumed to be MPEG-1.
- Validation ran against GStreamer 1.28.2 from a local sysroot on a headless
  Ubuntu host with a `fakesink`; no audible playback through a real device was
  verified here. The virtual PulseAudio sink does not drain in real time: an
  equivalent `gst-launch ... ! pulsesink` run also never reaches EOS, so this
  is an environment limitation rather than a pipeline defect.
  `examples/stream_mp3.rs` drives `AudioSink::Auto` (and `--sink <factory>`) for
  that check on the Fedora KDE target.
- Startup calls `gst::init()` (idempotent), so callers need no GStreamer
  setup. GStreamer dev libraries are now a build-time prerequisite for the
  crate.
