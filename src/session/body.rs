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
use crate::selection::{self, SelectionError, SelectionSource};
use crate::session::{SessionError, SessionState, TeardownStep};
use crate::tts::{TtsClient, TtsError};

// Re-exported so existing callers keep using `session::body::Cancellation`; the
// primitive itself lives in a neutral module that selection and TTS also use.
pub use crate::cancellation::Cancellation;

/// Hook invoked during audio-pipeline initialisation (see
/// [`ReadAloudSession::with_init_hook`]).
type InitHook = Arc<dyn Fn(&SessionContext) + Send + Sync>;

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

    /// Record one teardown step for observers. Bodies that own no resources
    /// can keep the default; the coordinator still records the socket/lock
    /// steps on the same log.
    fn record_teardown(&self, _step: TeardownStep) {}
}

/// Boxed bodies remain bodies, so the one-body coordinator wrapper can reuse
/// the same factory seam as the replacement-aware loop.
impl<T: SessionBody + ?Sized> SessionBody for Box<T> {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        (**self).run(ctx)
    }

    fn record_teardown(&self, step: TeardownStep) {
        (**self).record_teardown(step);
    }
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
    /// Ordered teardown steps, used by tests to verify deterministic cleanup.
    pub teardown: Vec<TeardownStep>,
    /// Number of audio pipelines currently live, and the high-water mark.
    /// Shared across bodies so overlapping sessions are detectable.
    pub active_players: i32,
    pub peak_active_players: i32,
    /// True once `cancel()` has returned both pipeline and sink to Null.
    pub audio_torn_down: bool,
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

fn record_teardown_step(report: &Option<Arc<Mutex<SessionReport>>>, step: TeardownStep) {
    if let Some(report) = report
        && let Ok(mut guard) = report.lock()
    {
        guard.teardown.push(step);
    }
}

fn player_opened(report: &Option<Arc<Mutex<SessionReport>>>) {
    if let Some(report) = report
        && let Ok(mut guard) = report.lock()
    {
        guard.active_players += 1;
        guard.peak_active_players = guard.peak_active_players.max(guard.active_players);
    }
}

fn player_closed(report: &Option<Arc<Mutex<SessionReport>>>, player: &StreamingMp3Player) {
    if let Some(report) = report
        && let Ok(mut guard) = report.lock()
    {
        guard.active_players -= 1;
        guard.audio_torn_down = player.is_torn_down();
    }
}

/// Watches cancellation from a separate thread and stops the pipeline so a
/// `push()` blocked on GStreamer back-pressure unblocks immediately. Without
/// this, a slow sink could keep the body alive long after `toggle` was answered.
struct PipelineWatchdog {
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PipelineWatchdog {
    fn spawn(player: Arc<StreamingMp3Player>, cancel: Cancellation) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let guard_done = Arc::clone(&done);
        let thread = thread::spawn(move || {
            while !guard_done.load(Ordering::Acquire) {
                if cancel.is_cancelled() {
                    // `cancel()` is idempotent; return right after the first
                    // call so a stuck push is unblocked exactly once.
                    player.cancel();
                    return;
                }
                thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }

    fn disarm(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PipelineWatchdog {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// Stop the pipeline, release the device and record those steps in order.
fn stop_pipeline(report: &Option<Arc<Mutex<SessionReport>>>, player: &StreamingMp3Player) {
    player.cancel();
    record_teardown_step(report, TeardownStep::AudioPipelineStopped);
    player_closed(report, player);
}

/// Run the full cancellation teardown in the spec's order (§10). Steps whose
/// resource does not exist at this stage are still recorded as no-ops so the
/// sequence is deterministic and observable.
fn record_cancel_teardown(report: &Option<Arc<Mutex<SessionReport>>>, player: &StreamingMp3Player) {
    record_teardown_step(report, TeardownStep::StoppedAcceptingAudio);
    record_teardown_step(report, TeardownStep::HttpRequestCancelled);
    stop_pipeline(report, player);
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
            // Must not be `Interrupted`: `Write::write_all` retries that kind
            // forever while the cancellation flag stays set.
            return Err(io::Error::other("session cancelled"));
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
            while Instant::now() < deadline
                && !self.ctx.cancel.is_cancelled()
                && !self.player.events().contains(&PlayerEvent::Started)
            {
                thread::sleep(Duration::from_millis(2));
            }
            if self.ctx.cancel.is_cancelled() {
                self.cancelled = true;
                // See the note in `write`: an Interrupted error would be retried
                // by `write_all` for as long as cancellation remains set.
                return Err(io::Error::other("session cancelled"));
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
    /// Invoked between pipeline construction and `start()`; lets tests drive
    /// cancellation at the most delicate point of initialisation.
    init_hook: Option<InitHook>,
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
            init_hook: None,
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

    /// Run `hook` after the audio pipeline is built but before it starts.
    /// Tests use this to cancel deterministically during initialisation.
    pub fn with_init_hook(
        mut self,
        hook: impl Fn(&SessionContext) + Send + Sync + 'static,
    ) -> Self {
        self.init_hook = Some(Arc::new(hook));
        self
    }
}

impl<S: SelectionSource + Send> SessionBody for ReadAloudSession<S> {
    fn run(&mut self, ctx: &SessionContext) -> Result<(), SessionError> {
        // Starting (set by the coordinator): read and validate the selection.
        let selected = match selection::read_selection_cancellable(
            &self.source,
            &self.selection_config,
            &ctx.cancel,
        ) {
            Ok(selected) => selected,
            Err(SelectionError::Cancelled) => {
                self.record_teardown(TeardownStep::SelectionProcessTerminated);
                publish(ctx, &self.report, SessionState::Stopping);
                return Ok(());
            }
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

        // Cancellation can arrive while Connecting is published or while the
        // pipeline is being constructed; either way the pipeline is torn down
        // through the same ordered path as a cancellation during streaming.
        let player = Arc::new(
            StreamingMp3Player::new(self.sink.clone(), self.stream_config)
                .map_err(SessionError::Audio)?,
        );
        player_opened(&self.report);

        if let Some(hook) = &self.init_hook {
            hook(ctx);
        }
        if ctx.cancel.is_cancelled() {
            record_cancel_teardown(&self.report, &player);
            publish(ctx, &self.report, SessionState::Stopping);
            return Ok(());
        }
        match player.start() {
            Ok(()) => {}
            Err(AudioError::Cancelled) => {
                record_cancel_teardown(&self.report, &player);
                publish(ctx, &self.report, SessionState::Stopping);
                return Ok(());
            }
            Err(err) => {
                stop_pipeline(&self.report, &player);
                publish(ctx, &self.report, SessionState::Stopping);
                return Err(SessionError::Audio(err));
            }
        }
        if ctx.cancel.is_cancelled() {
            record_cancel_teardown(&self.report, &player);
            publish(ctx, &self.report, SessionState::Stopping);
            return Ok(());
        }

        // Only now, with the pipeline fully started, does the watchdog begin
        // watching for cancellation: its job is to unblock a `push()` stalled
        // on back-pressure, not to race `start()`'s state change.
        let mut watchdog = PipelineWatchdog::spawn(Arc::clone(&player), ctx.cancel.clone());

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
        let stream_result = client.speak_to_cancellable(selected.text(), &mut writer, &ctx.cancel);

        // Cancellation wins over follow-on errors: once Stop arrives the HTTP
        // request has been aborted and the watchdog has stopped the pipeline,
        // so an `Interrupted` write or `Cancelled` push is not an error.
        let cancelled = ctx.cancel.is_cancelled()
            || writer.cancelled
            || matches!(stream_result, Err(TtsError::Cancelled));
        if cancelled {
            watchdog.disarm();
            record_cancel_teardown(&self.report, &player);
            publish(ctx, &self.report, SessionState::Stopping);
            return Ok(());
        }
        if let Some(err) = writer.audio_error.take() {
            watchdog.disarm();
            stop_pipeline(&self.report, &player);
            publish(ctx, &self.report, SessionState::Stopping);
            return Err(SessionError::Audio(err));
        }
        if let Err(err) = stream_result {
            watchdog.disarm();
            stop_pipeline(&self.report, &player);
            publish(ctx, &self.report, SessionState::Stopping);
            return Err(SessionError::Tts(err));
        }

        // A very short stream can finish before the probe marks Started.
        if !writer.playing {
            publish(ctx, &self.report, SessionState::Playing);
        }

        let stats = match player.finish() {
            Ok(stats) => stats,
            Err(AudioError::Cancelled) => {
                watchdog.disarm();
                record_cancel_teardown(&self.report, &player);
                publish(ctx, &self.report, SessionState::Stopping);
                return Ok(());
            }
            Err(err) => {
                watchdog.disarm();
                stop_pipeline(&self.report, &player);
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
        // coordinator releases the control socket and lock on our return.
        watchdog.disarm();
        record_teardown_step(&self.report, TeardownStep::AudioPipelineStopped);
        player_closed(&self.report, &player);
        publish(ctx, &self.report, SessionState::Stopping);
        Ok(())
    }

    fn record_teardown(&self, step: TeardownStep) {
        record_teardown_step(&self.report, step);
    }
}
