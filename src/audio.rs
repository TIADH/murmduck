//! Progressive MP3 and WAV playback over GStreamer `appsrc`.
//!
//! This module grew out of the streaming-audio spike (issue #5) and keeps its
//! validated pipeline shape and observable seams. The decode chain is chosen
//! by [`StreamFormat`]; everything downstream of it is shared.
//!
//! ```text
//! MP3: appsrc -> mpegaudioparse -> mpg123audiodec -> audioconvert -> audioresample -> volume -> sink
//! WAV: appsrc -> wavparse                        -> audioconvert -> audioresample -> volume -> sink
//! ```
//!
//! `mpegaudioparse` re-frames MPEG audio so a frame may be split across any
//! number of pushed buffers. `wavparse` demuxes streamed WAV directly, including
//! the placeholder (`0xFFFFFFFF`) RIFF/data lengths emitted by TTS servers that
//! do not know the final size up front.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppSrcCallbacks, AppStreamType};

/// Callback invoked once when the first decoded buffer reaches the sink.
type StartedCallback = Arc<dyn Fn() + Send + Sync>;

/// Where decoded audio is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSink {
    /// Let GStreamer pick the platform device (PulseAudio on the spike host).
    Auto,
    /// Consume audio without a device. Used by tests and headless runs.
    Fake,
    /// A fake sink that consumes in real time, so a fast feed meets back-pressure.
    SyncedFake,
    /// An explicit GStreamer sink element factory, e.g. `"pulsesink"`.
    Factory(String),
}

impl AudioSink {
    fn factory(&self) -> &str {
        match self {
            Self::Auto => "autoaudiosink",
            Self::Fake | Self::SyncedFake => "fakesink",
            Self::Factory(name) => name,
        }
    }
}

/// The container/encoding of the bytes fed to the player.
///
/// This is deliberately a playback concern, not a TTS-config concern; the
/// conversion from the configured `ResponseFormat` lives in the `From` impl.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamFormat {
    #[default]
    Mp3,
    Wav,
}

impl From<crate::config::ResponseFormat> for StreamFormat {
    fn from(value: crate::config::ResponseFormat) -> Self {
        match value {
            crate::config::ResponseFormat::Mp3 => Self::Mp3,
            crate::config::ResponseFormat::Wav => Self::Wav,
        }
    }
}

/// Bounded-buffering and decoder tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamConfig {
    /// Playback volume from `audio.volume`, in `0.0..=1.0`. Applied by a
    /// `volume` element in the pipeline; `1.0` is the neutral default.
    pub volume: f32,
    /// Hard cap on bytes queued in `appsrc`; `push` back-pressures past this.
    pub max_bytes: u64,
    /// Forwarded to `appsrc`'s `max-latency` property for latency reporting.
    /// Underrun *events* come from `need-data` (`min-percent`), not this value.
    pub max_latency: Duration,
    /// Copy decoded PCM into memory so tests can compare output. Off by default
    /// so long streams never grow memory without bound in production.
    pub capture_decoded: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            volume: 1.0,
            max_bytes: 64 * 1024,
            max_latency: Duration::from_millis(20),
            capture_decoded: false,
        }
    }
}

/// Observable playback lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerEvent {
    /// The first decoded buffer reached the sink: playback has begun.
    Started,
    /// The source ran dry (`appsrc` asked for more data).
    Underrun,
    /// End-of-stream was reached and playback finished normally.
    Eos,
    /// Playback was stopped by [`StreamingMp3Player::cancel`].
    Cancelled,
}

/// Measurements collected during one playback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackStats {
    pub bytes_pushed: u64,
    pub decoded_bytes: u64,
    pub max_buffered_bytes: u64,
    pub underruns: u64,
}

/// Failures from building or driving the audio pipeline.
#[derive(Debug)]
pub enum AudioError {
    /// The pipeline could not be created, linked or state-changed.
    Pipeline(String),
    /// Feeding bytes into `appsrc` failed (including teardown during cancel).
    Push(String),
    /// GStreamer reported a decoding/sink error on the bus.
    Decode(String),
    /// Playback did not reach EOS before the deadline.
    Timeout,
    /// Playback was cancelled while a chunk was being fed.
    Cancelled,
    /// The response stream produced bytes but no decodable audio samples.
    NoDecodedAudio,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pipeline(message) => write!(f, "audio pipeline error: {message}"),
            Self::Push(message) => write!(f, "audio input error: {message}"),
            Self::Decode(message) => write!(f, "audio decode error: {message}"),
            Self::Timeout => write!(f, "timed out waiting for playback to finish"),
            Self::Cancelled => write!(f, "playback was cancelled"),
            Self::NoDecodedAudio => write!(f, "TTS response contained no decodable audio"),
        }
    }
}

impl Error for AudioError {}

#[derive(Default)]
struct Inner {
    events: Vec<PlayerEvent>,
    captured: Vec<u8>,
    stats: PlaybackStats,
    started: bool,
}

/// A single streaming playback session.
///
/// Despite the historical name, [`Self::with_format`] can decode WAV as well
/// as MP3. All methods take `&self` so a second thread can call [`Self::cancel`]
/// while another is blocked in [`Self::push`].
pub struct StreamingMp3Player {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    sink: gst::Element,
    /// The `volume` element, kept so the applied value is observable.
    volume_element: gst::Element,
    bus: gst::Bus,
    inner: Arc<Mutex<Inner>>,
    cancelled: Arc<AtomicBool>,
    /// Invoked once from the GStreamer streaming thread when the first decoded
    /// buffer reaches the sink (see [`Self::on_started`]).
    started_callback: Arc<Mutex<Option<StartedCallback>>>,
}

impl StreamingMp3Player {
    /// Build an MP3 pipeline for `sink`. No device is opened until [`Self::start`].
    pub fn new(sink: AudioSink, config: StreamConfig) -> Result<Self, AudioError> {
        Self::with_format(sink, config, StreamFormat::Mp3)
    }

    /// Build a pipeline for `sink` that decodes `format`.
    ///
    /// MP3 needs framing (`mpegaudioparse`) before `mpg123audiodec`; WAV is
    /// demuxed by `wavparse`, which accepts the placeholder RIFF/data lengths
    /// sent by streaming TTS servers. This was verified against GStreamer
    /// 1.28.2 with `0xFFFFFFFF` RIFF/data lengths, so no hand-rolled header
    /// rewrite is needed. Both formats share the bounded `appsrc`,
    /// conversion/resampling, volume and cancellation machinery below.
    pub fn with_format(
        sink: AudioSink,
        config: StreamConfig,
        format: StreamFormat,
    ) -> Result<Self, AudioError> {
        // `gst::init` is idempotent; doing it here keeps callers from needing
        // to know that the backend is GStreamer-based.
        gst::init().map_err(|err| AudioError::Pipeline(format!("gst init: {err}")))?;
        let pipeline = gst::Pipeline::new();
        let (caps, decode): (gst::Caps, Vec<gst::Element>) = match format {
            StreamFormat::Mp3 => (
                gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 1i32)
                    .build(),
                vec![
                    gst::ElementFactory::make("mpegaudioparse")
                        .name("parser")
                        .build()
                        .map_err(|err| AudioError::Pipeline(format!("mpegaudioparse: {err}")))?,
                    gst::ElementFactory::make("mpg123audiodec")
                        .name("decoder")
                        .build()
                        .map_err(|err| AudioError::Pipeline(format!("mpg123audiodec: {err}")))?,
                ],
            ),
            StreamFormat::Wav => (
                gst::Caps::builder("audio/x-wav").build(),
                vec![
                    gst::ElementFactory::make("wavparse")
                        .name("decoder")
                        .build()
                        .map_err(|err| AudioError::Pipeline(format!("wavparse: {err}")))?,
                ],
            ),
        };

        let inner = Arc::new(Mutex::new(Inner::default()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let started_callback: Arc<Mutex<Option<StartedCallback>>> = Arc::new(Mutex::new(None));

        let underrun_inner = Arc::clone(&inner);
        let underrun_cancelled = Arc::clone(&cancelled);
        let appsrc = AppSrc::builder()
            .name("src")
            .caps(&caps)
            .stream_type(AppStreamType::Stream)
            .format(gst::Format::Bytes)
            // Let the framer/decoder generate timestamps from MPEG frame
            // counts. Stamping with wall-clock push time bunches up when a
            // local file (or a fast HTTP burst) is fed faster than real time.
            .do_timestamp(false)
            .block(true)
            .max_bytes(config.max_bytes)
            .min_percent(20)
            .max_latency(
                i64::try_from(config.max_latency.as_nanos())
                    .map_err(|_| AudioError::Pipeline("max_latency is too large".into()))?,
            )
            .build();
        appsrc.set_callbacks(
            AppSrcCallbacks::builder()
                .need_data(move |_, _| {
                    if underrun_cancelled.load(Ordering::Acquire) {
                        return;
                    }
                    if let Ok(mut state) = underrun_inner.lock() {
                        state.stats.underruns += 1;
                        state.events.push(PlayerEvent::Underrun);
                    }
                })
                .build(),
        );

        let convert = gst::ElementFactory::make("audioconvert")
            .name("convert")
            .build()
            .map_err(|err| AudioError::Pipeline(format!("audioconvert: {err}")))?;
        let resample = gst::ElementFactory::make("audioresample")
            .name("resample")
            .build()
            .map_err(|err| AudioError::Pipeline(format!("audioresample: {err}")))?;
        let sink_element = gst::ElementFactory::make(sink.factory())
            .name("sink")
            .build()
            .map_err(|err| AudioError::Pipeline(format!("sink: {err}")))?;
        if matches!(sink, AudioSink::SyncedFake) {
            sink_element.set_property("sync", true);
        }
        // `audio.volume` is applied here, just before the sink, so it works for
        // every sink type (including real devices) and is observable in tests.
        let volume_element = gst::ElementFactory::make("volume")
            .name("volume")
            .build()
            .map_err(|err| AudioError::Pipeline(format!("volume: {err}")))?;
        volume_element.set_property("volume", f64::from(config.volume));

        let mut elements: Vec<&gst::Element> = Vec::with_capacity(6 + decode.len());
        elements.push(appsrc.upcast_ref());
        elements.extend(decode.iter());
        elements.push(&convert);
        elements.push(&resample);
        elements.push(&volume_element);
        elements.push(&sink_element);
        pipeline
            .add_many(&elements)
            .map_err(|err| AudioError::Pipeline(format!("add elements: {err}")))?;
        gst::Element::link_many(&elements)
            .map_err(|err| AudioError::Pipeline(format!("link elements: {err}")))?;

        // A pad probe on the sink's input observes decoded audio for every sink
        // type (including real devices), so tests need no special sink.
        let probe_inner = Arc::clone(&inner);
        let probe_callback = Arc::clone(&started_callback);
        let capture = config.capture_decoded;
        sink_element
            .static_pad("sink")
            .expect("sink has a sink pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                let Some(buffer) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let map = buffer.map_readable().expect("map decoded buffer");
                // Update the shared state and decide whether this is the first
                // decoded buffer while holding the lock, then release it before
                // invoking the callback (which may call back into the player).
                let callback = {
                    let mut state = probe_inner.lock().expect("lock probe state");
                    state.stats.decoded_bytes += map.as_slice().len() as u64;
                    let first = !state.started;
                    if first {
                        state.started = true;
                        state.events.push(PlayerEvent::Started);
                    }
                    if capture {
                        state.captured.extend_from_slice(map.as_slice());
                    }
                    if first {
                        probe_callback
                            .lock()
                            .expect("lock started callback")
                            .clone()
                    } else {
                        None
                    }
                };
                if let Some(callback) = callback {
                    callback();
                }
                gst::PadProbeReturn::Ok
            });

        let bus = pipeline.bus().expect("pipeline has a bus");
        Ok(Self {
            pipeline,
            appsrc,
            sink: sink_element,
            volume_element,
            bus,
            inner,
            cancelled,
            started_callback,
        })
    }

    /// Register a callback invoked once, on GStreamer's streaming thread, when
    /// the first decoded buffer reaches the sink. Register before [`Self::start`]
    /// so no startup event can be missed; the callback must not block.
    pub fn on_started<F>(&self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.started_callback.lock().expect("lock started callback") = Some(Arc::new(callback));
    }

    /// Move the pipeline to PLAYING. Call before the first [`Self::push`].
    ///
    /// If cancellation arrives before or during the state change, the pipeline
    /// is returned to Null and no device is left open.
    pub fn start(&self) -> Result<(), AudioError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| AudioError::Pipeline(format!("set PLAYING: {err}")))?;
        if self.cancelled.load(Ordering::Acquire) {
            let _ = self.pipeline.set_state(gst::State::Null);
            return Err(AudioError::Cancelled);
        }
        self.check_bus()
    }

    /// Feed the next chunk of the audio stream.
    ///
    /// Chunk boundaries are arbitrary: `mpegaudioparse` reassembles MPEG
    /// frames and `wavparse` buffers the RIFF header and demuxes WAV.
    /// With a bounded `max_bytes`, this blocks until the sink has consumed
    /// enough data, providing back-pressure rather than growing memory.
    pub fn push(&self, chunk: &[u8]) -> Result<(), AudioError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        self.check_bus()?;
        if let Err(err) = self
            .appsrc
            .push_buffer(gst::Buffer::from_slice(chunk.to_vec()))
        {
            return Err(if self.cancelled.load(Ordering::Acquire) {
                AudioError::Cancelled
            } else {
                AudioError::Push(err.to_string())
            });
        }
        let buffered = self.appsrc.current_level_bytes();
        let mut state = self.inner.lock().expect("lock player state");
        state.stats.bytes_pushed += chunk.len() as u64;
        state.stats.max_buffered_bytes = state.stats.max_buffered_bytes.max(buffered);
        Ok(())
    }

    /// Signal end-of-stream and wait for playback to finish.
    ///
    /// The pipeline is always moved to Null afterwards, releasing the device.
    pub fn finish(&self) -> Result<PlaybackStats, AudioError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        self.check_bus()?;
        self.appsrc
            .end_of_stream()
            .map_err(|err| AudioError::Push(err.to_string()))?;
        // Poll in short slices so cancellation (which may arrive from the
        // watchdog thread while we wait for EOS) is observed promptly instead
        // of blocking for the whole 30 s completion deadline.
        let slice = Some(gst::ClockTime::from_mseconds(200));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                let _ = self.pipeline.set_state(gst::State::Null);
                return Err(AudioError::Cancelled);
            }
            match self
                .bus
                .timed_pop_filtered(slice, &[gst::MessageType::Eos, gst::MessageType::Error])
            {
                Some(message) => match message.view() {
                    gst::MessageView::Eos(_) => {
                        self.inner
                            .lock()
                            .expect("lock player state")
                            .events
                            .push(PlayerEvent::Eos);
                        break;
                    }
                    gst::MessageView::Error(err) => {
                        let detail = format!("{}: {:?}", err.error(), err.debug());
                        let _ = self.pipeline.set_state(gst::State::Null);
                        return Err(AudioError::Decode(detail));
                    }
                    _ => {}
                },
                None if std::time::Instant::now() >= deadline => {
                    let _ = self.pipeline.set_state(gst::State::Null);
                    return Err(AudioError::Timeout);
                }
                None => {}
            }
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|err| AudioError::Pipeline(format!("set NULL: {err}")))?;
        Ok(self.stats())
    }

    /// Stop immediately, unblocking any in-flight [`Self::push`] and returning
    /// the sink and pipeline to Null so the audio device is released.
    ///
    /// Safe to call from a different thread than the one feeding audio.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.inner
            .lock()
            .expect("lock player state")
            .events
            .push(PlayerEvent::Cancelled);
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Collected lifecycle events, in order.
    pub fn events(&self) -> Vec<PlayerEvent> {
        self.inner.lock().expect("lock player state").events.clone()
    }

    /// True once the decoded-buffer probe has observed playback starting.
    ///
    /// Exposed so callers can drive their own state from the player instead of
    /// scanning [`Self::events`] for [`PlayerEvent::Started`].
    pub fn has_started(&self) -> bool {
        self.inner.lock().expect("lock player state").started
    }

    /// The volume actually applied to the pipeline (read back from the element).
    pub fn volume(&self) -> f64 {
        self.volume_element.property::<f64>("volume")
    }

    /// Bytes currently queued in `appsrc`.
    pub fn buffered_bytes(&self) -> u64 {
        self.appsrc.current_level_bytes()
    }

    /// Measurements for this playback so far.
    pub fn stats(&self) -> PlaybackStats {
        self.inner.lock().expect("lock player state").stats
    }

    /// Decoded PCM when [`StreamConfig::capture_decoded`] is enabled.
    pub fn captured_decoded(&self) -> Vec<u8> {
        self.inner
            .lock()
            .expect("lock player state")
            .captured
            .clone()
    }

    /// Current GStreamer pipeline state (Null after `finish`/`cancel`).
    pub fn pipeline_state(&self) -> gst::State {
        self.pipeline.current_state()
    }

    /// Current state of the sink element (Null once the device is released).
    pub fn sink_state(&self) -> gst::State {
        self.sink.current_state()
    }

    /// True once both pipeline and sink are Null (device released).
    pub fn is_torn_down(&self) -> bool {
        self.pipeline_state() == gst::State::Null && self.sink_state() == gst::State::Null
    }

    fn check_bus(&self) -> Result<(), AudioError> {
        match self
            .bus
            .timed_pop_filtered(Some(gst::ClockTime::ZERO), &[gst::MessageType::Error])
        {
            Some(message) => match message.view() {
                gst::MessageView::Error(err) => {
                    let detail = format!("{}: {:?}", err.error(), err.debug());
                    Err(AudioError::Decode(detail))
                }
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }
}
