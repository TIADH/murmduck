//! Session-body seam: cancellation, shared lifecycle state and the running
//! body. This module is what a later milestone plugs the real TTS/audio
//! pipeline into; the owner's lock/IPC machinery does not change.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::{AudioError, AudioSink, PlayerEvent, StreamConfig, StreamingMp3Player};
use crate::config::{SelectionConfig, TtsConfig};
use crate::selection::{self, SelectionSource};
use crate::session::{SessionError, SessionState};
use crate::tts::TtsClient;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Shared cancellation signal. `Clone` handles all point at the same flag.
#[derive(Debug, Clone)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Request cancellation of the running session.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Wait until cancelled or `timeout` elapses. Returns `true` if cancelled.
    ///
    /// `Duration::MAX` is handled without overflowing `Instant`.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if self.is_cancelled() {
                return true;
            }
            match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return false;
                    }
                    thread::sleep(remaining.min(POLL_INTERVAL));
                }
                None => thread::sleep(POLL_INTERVAL),
            }
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// Values a running body can read and update while it works.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// Set by the control server when a Stop command arrives.
    pub cancel: Cancellation,
    /// Current lifecycle state, visible to `status` clients.
    pub state: Arc<Mutex<SessionState>>,
}

impl SessionContext {
    /// Publish a new lifecycle state.
    pub fn set_state(&self, state: SessionState) {
        let mut guard = self.state.lock().unwrap_or_else(|err| err.into_inner());
        *guard = state;
    }

    /// Read the current lifecycle state.
    pub fn state(&self) -> SessionState {
        *self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// The work an owner performs while holding the lock and serving control
/// commands. A body returns when its work completes or `ctx.cancel` is set.
pub trait SessionBody: Send {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError>;
}

/// A body that starts and immediately finishes. Used by commands (`stop`,
/// `status`) that only become an owner transiently and never run a session.
pub struct NoopBody;

impl SessionBody for NoopBody {
    fn run(&mut self, _ctx: &SessionContext) -> Result<(), SessionError> {
        Ok(())
    }
}

/// Placeholder body for this milestone: read the PRIMARY selection, report
/// `Playing`, then stay alive until cancelled (or an optional deadline).
///
/// No TTS request or audio playback happens here — ticket #6 replaces this
/// body with the real pipeline, without changing the owner/IPC machinery.
pub struct PlaceholderSession<S> {
    source: S,
    selection_config: SelectionConfig,
    max_duration: Option<Duration>,
}

impl<S: SelectionSource> PlaceholderSession<S> {
    pub fn new(source: S, selection_config: SelectionConfig) -> Self {
        Self {
            source,
            selection_config,
            max_duration: None,
        }
    }

    /// Bound the placeholder's lifetime (useful for tests and safe shutdown).
    pub fn with_max_duration(mut self, duration: Duration) -> Self {
        self.max_duration = Some(duration);
        self
    }
}

impl<S: SelectionSource + Send> SessionBody for PlaceholderSession<S> {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        ctx.set_state(SessionState::Connecting);
        let _selected = selection::read_selection(&self.source, &self.selection_config)
            .map_err(SessionError::Selection)?;

        if ctx.cancel.is_cancelled() {
            ctx.set_state(SessionState::Stopping);
            return Ok(());
        }

        ctx.set_state(SessionState::Playing);
        let wait = self.max_duration.unwrap_or(Duration::MAX);
        ctx.cancel.wait_timeout(wait);
        ctx.set_state(SessionState::Stopping);
        Ok(())
    }
}

/// Observable, text-free outcome of a real read-aloud session.
///
/// Tests inject a shared instance to observe lifecycle states and playback
/// measurements through a public seam; the session text/credentials never
/// appear here.
#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    pub states: Vec<SessionState>,
    pub bytes_pushed: u64,
    pub decoded_bytes: u64,
}

/// Publish a state both to the live session (for `status` clients) and to the
/// optional report (for observers).
fn publish(ctx: &SessionContext, report: &Option<Arc<Mutex<SessionReport>>>, state: SessionState) {
    ctx.set_state(state);
    if let Some(report) = report
        && let Ok(mut guard) = report.lock()
    {
        guard.states.push(state);
    }
}

/// Bridges the TTS client's streaming `Write` sink to the audio player.
///
/// Each HTTP chunk is pushed straight into `appsrc`; the first decoded buffer
/// flips the published state to `Playing`. Cancellation and decoder failures
/// are latched so the caller can tear the pipeline down deterministically.
struct AudioSinkWriter<'a> {
    player: &'a StreamingMp3Player,
    ctx: &'a SessionContext,
    report: Option<Arc<Mutex<SessionReport>>>,
    playing: bool,
    /// Bounded window in which to await the asynchronous `Started` probe.
    start_deadline: Option<Instant>,
    cancelled: bool,
    audio_error: Option<AudioError>,
}

impl Write for AudioSinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.ctx.cancel.is_cancelled() {
            self.cancelled = true;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "session cancelled",
            ));
        }
        if let Err(err) = self.player.push(buf) {
            self.audio_error = Some(err);
            return Err(io::Error::other("audio pipeline rejected a chunk"));
        }
        if !self.playing {
            // The decoded-buffer probe runs on GStreamer's streaming thread,
            // so `Started` may be recorded just after `push` returns. Wait
            // briefly (once, not per chunk) before declaring playback under way.
            let deadline = *self
                .start_deadline
                .get_or_insert_with(|| Instant::now() + Duration::from_millis(1_000));
            while Instant::now() < deadline && !self.player.events().contains(&PlayerEvent::Started)
            {
                thread::sleep(Duration::from_millis(2));
            }
            if self.player.events().contains(&PlayerEvent::Started) {
                self.playing = true;
                publish(self.ctx, &self.report, SessionState::Playing);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The real happy path: selection → streaming TTS → progressive playback.
///
/// This is the body `toggle` runs when no session is active. It keeps the
/// owner/IPC machinery untouched; cancellation is observed cooperatively here
/// and the ordered teardown across stages belongs to ticket #7.
pub struct ReadAloudSession<S> {
    source: S,
    selection_config: SelectionConfig,
    tts_config: TtsConfig,
    api_key: Option<String>,
    sink: AudioSink,
    stream_config: StreamConfig,
    report: Option<Arc<Mutex<SessionReport>>>,
}

impl<S: SelectionSource> ReadAloudSession<S> {
    pub fn new(
        source: S,
        selection_config: SelectionConfig,
        tts_config: TtsConfig,
        api_key: Option<String>,
        sink: AudioSink,
    ) -> Self {
        Self {
            source,
            selection_config,
            tts_config,
            api_key,
            sink,
            stream_config: StreamConfig::default(),
            report: None,
        }
    }

    /// Override audio tuning (back-pressure cap, decoded capture) for tests.
    pub fn with_stream_config(mut self, stream_config: StreamConfig) -> Self {
        self.stream_config = stream_config;
        self
    }

    /// Attach an observer for lifecycle/playback measurements.
    pub fn with_report(mut self, report: Arc<Mutex<SessionReport>>) -> Self {
        self.report = Some(report);
        self
    }
}

impl<S: SelectionSource + Send> SessionBody for ReadAloudSession<S> {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        // Starting (set by the coordinator): read and validate the selection.
        let selected = match selection::read_selection(&self.source, &self.selection_config) {
            Ok(selected) => selected,
            Err(err) => {
                publish(ctx, &self.report, SessionState::Stopping);
                return Err(SessionError::Selection(err));
            }
        };

        if ctx.cancel.is_cancelled() {
            publish(ctx, &self.report, SessionState::Stopping);
            return Ok(());
        }

        let client = match TtsClient::new(&self.tts_config, self.api_key.clone()) {
            Ok(client) => client,
            Err(err) => {
                publish(ctx, &self.report, SessionState::Stopping);
                return Err(SessionError::Tts(err));
            }
        };

        // Connecting: send the TTS request and begin receiving audio.
        publish(ctx, &self.report, SessionState::Connecting);

        let player = StreamingMp3Player::new(self.sink.clone(), self.stream_config)
            .map_err(SessionError::Audio)?;
        if let Err(err) = player.start() {
            player.cancel();
            publish(ctx, &self.report, SessionState::Stopping);
            return Err(SessionError::Audio(err));
        }

        // Buffering: audio is arriving but has not reached the sink yet.
        publish(ctx, &self.report, SessionState::Buffering);

        let mut writer = AudioSinkWriter {
            player: &player,
            ctx,
            report: self.report.clone(),
            playing: false,
            start_deadline: None,
            cancelled: false,
            audio_error: None,
        };
        let stream_result = client.speak_to(selected.text(), &mut writer);

        if writer.cancelled {
            player.cancel();
            publish(ctx, &self.report, SessionState::Stopping);
            return Ok(());
        }
        if let Some(err) = writer.audio_error.take() {
            player.cancel();
            publish(ctx, &self.report, SessionState::Stopping);
            return Err(SessionError::Audio(err));
        }
        if let Err(err) = stream_result {
            player.cancel();
            publish(ctx, &self.report, SessionState::Stopping);
            return Err(SessionError::Tts(err));
        }

        // A very short stream can finish before the probe marks Started.
        if !writer.playing {
            publish(ctx, &self.report, SessionState::Playing);
        }

        let stats = match player.finish() {
            Ok(stats) => stats,
            Err(err) => {
                player.cancel();
                publish(ctx, &self.report, SessionState::Stopping);
                return Err(SessionError::Audio(err));
            }
        };
        if let Some(report) = &self.report
            && let Ok(mut guard) = report.lock()
        {
            guard.bytes_pushed = stats.bytes_pushed;
            guard.decoded_bytes = stats.decoded_bytes;
        }

        // Stopping: `finish()` already returned the pipeline to Null; the
        // coordinator releases the lock and control socket on our return.
        publish(ctx, &self.report, SessionState::Stopping);
        Ok(())
    }
}
