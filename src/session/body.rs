//! Session-body seam: cancellation, shared lifecycle state and the running
//! body. The owner's lock/IPC machinery does not depend on which body runs.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::{AudioError, AudioSink, StreamConfig, StreamingMp3Player};
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

/// Lightweight demo/test body: read the PRIMARY selection, report `Playing`,
/// then stay alive until cancelled (or an optional deadline).
///
/// It deliberately does no TTS or audio work. The real pipeline is
/// [`ReadAloudSession`]; this body exists so the owner/IPC machinery can be
/// exercised without a TTS server or GStreamer.
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

fn record_teardown_step(report: &Option<Arc<Mutex<SessionReport>>>, step: TeardownStep) {
    if let Some(report) = report
        && let Ok(mut guard) = report.lock()
    {
        guard.teardown.push(step);
    }
}

/// The observer bundle shared by the body and its streaming writer.
///
/// Before this existed, `ctx`, `report` and sometimes `player` were passed
/// separately through every helper. Bundling the seam keeps state publishing
/// and teardown recording in one place.
#[derive(Clone, Copy)]
struct SessionObserver<'a> {
    ctx: &'a SessionContext,
    report: &'a Option<Arc<Mutex<SessionReport>>>,
}

impl SessionObserver<'_> {
    /// Publish a state both to `status` clients and to the optional report.
    fn publish(&self, state: SessionState) {
        self.ctx.set_state(state);
        if let Some(report) = self.report
            && let Ok(mut guard) = report.lock()
        {
            guard.states.push(state);
        }
    }

    fn record(&self, step: TeardownStep) {
        record_teardown_step(self.report, step);
    }

    fn player_opened(&self) {
        if let Some(report) = self.report
            && let Ok(mut guard) = report.lock()
        {
            guard.active_players += 1;
            guard.peak_active_players = guard.peak_active_players.max(guard.active_players);
        }
    }

    fn player_closed(&self, player: &StreamingMp3Player) {
        if let Some(report) = self.report
            && let Ok(mut guard) = report.lock()
        {
            guard.active_players -= 1;
            guard.audio_torn_down = player.is_torn_down();
        }
    }

    /// Non-cancellation teardown (errors, normal completion): release the
    /// pipeline and record that step. Cancellation uses [`OrderedTeardown`]
    /// instead so all steps happen in the spec's order.
    fn stop_pipeline(&self, player: &StreamingMp3Player) {
        player.cancel();
        self.record(TeardownStep::AudioPipelineStopped);
        self.player_closed(player);
    }
}

/// A cancellable teardown's actual effects. The trait lets the ordering logic
/// be tested with a recording target instead of a real GStreamer pipeline.
trait TeardownTarget: Send + 'static {
    fn stop_accepting(&self);
    fn cancel_http(&self);
    fn stop_playback(&self);
    fn playback_closed(&self, report: &Option<Arc<Mutex<SessionReport>>>);
}

/// Production teardown target: the writer's acceptance flag, the HTTP cancel
/// and the audio pipeline.
#[derive(Clone)]
struct PipelineTeardownTarget {
    accepting: Arc<AtomicBool>,
    http_cancel: Cancellation,
    player: Arc<StreamingMp3Player>,
}

impl TeardownTarget for PipelineTeardownTarget {
    fn stop_accepting(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    fn cancel_http(&self) {
        self.http_cancel.cancel();
    }

    fn stop_playback(&self) {
        self.player.cancel();
    }

    fn playback_closed(&self, report: &Option<Arc<Mutex<SessionReport>>>) {
        if let Some(report) = report
            && let Ok(mut guard) = report.lock()
        {
            guard.active_players -= 1;
            guard.audio_torn_down = self.player.is_torn_down();
        }
    }
}

/// Performs the §10 teardown *in order*, exactly once, under the control of
/// one idempotent gate. Both the owner thread and the watchdog call
/// [`Self::perform`]; the actions—not just their log entries—are ordered.
#[derive(Clone)]
struct OrderedTeardown<T: TeardownTarget> {
    target: T,
    report: Option<Arc<Mutex<SessionReport>>>,
    done: Arc<AtomicBool>,
}

impl<T: TeardownTarget> OrderedTeardown<T> {
    fn new(target: T, report: Option<Arc<Mutex<SessionReport>>>) -> Self {
        Self {
            target,
            report,
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn perform(&self) {
        // Only the first caller performs the teardown; later callers (the
        // owner thread after the watchdog, or repeated commands) are no-ops.
        if self.done.swap(true, Ordering::AcqRel) {
            return;
        }
        // Spec §10: (1) stop accepting audio, (2) cancel HTTP,
        // (3) stop playback, then (4) release decoder/pipeline resources.
        self.target.stop_accepting();
        record_teardown_step(&self.report, TeardownStep::StoppedAcceptingAudio);
        self.target.cancel_http();
        record_teardown_step(&self.report, TeardownStep::HttpRequestCancelled);
        self.target.stop_playback();
        record_teardown_step(&self.report, TeardownStep::AudioPipelineStopped);
        self.target.playback_closed(&self.report);
    }
}

/// Watches cancellation from a separate thread and runs the ordered teardown
/// so a `push()` blocked on GStreamer back-pressure unblocks immediately.
struct PipelineWatchdog {
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PipelineWatchdog {
    fn spawn(teardown: OrderedTeardown<PipelineTeardownTarget>, cancel: Cancellation) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let guard_done = Arc::clone(&done);
        let thread = thread::spawn(move || {
            while !guard_done.load(Ordering::Acquire) {
                if cancel.is_cancelled() {
                    teardown.perform();
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

/// Bridges the TTS client's streaming `Write` sink to the audio player.
///
/// Each HTTP chunk is pushed straight into `appsrc`; the first decoded buffer
/// flips the published state to `Playing`. Cancellation and decoder failures
/// are latched so the caller can tear the pipeline down deterministically.
struct AudioSinkWriter<'a> {
    player: &'a StreamingMp3Player,
    observer: SessionObserver<'a>,
    /// Cleared by teardown step 1 before the HTTP request is cancelled, so no
    /// chunk accepted after cancellation can reach the pipeline.
    accepting: Arc<AtomicBool>,
    playing: bool,
    /// Bounded window in which to await the asynchronous `Started` probe.
    start_deadline: Option<Instant>,
    cancelled: bool,
    audio_error: Option<AudioError>,
}

impl Write for AudioSinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.accepting.load(Ordering::Acquire) || self.observer.ctx.cancel.is_cancelled() {
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
                && self.accepting.load(Ordering::Acquire)
                && !self.observer.ctx.cancel.is_cancelled()
                && !self.player.has_started()
            {
                thread::sleep(Duration::from_millis(2));
            }
            if !self.accepting.load(Ordering::Acquire) || self.observer.ctx.cancel.is_cancelled() {
                self.cancelled = true;
                // See the note in `write`: an Interrupted error would be retried
                // by `write_all` for as long as cancellation remains set.
                return Err(io::Error::other("session cancelled"));
            }
            if self.player.has_started() {
                self.playing = true;
                self.observer.publish(SessionState::Playing);
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
/// and teardown follows the order required by spec §10.
pub struct ReadAloudSession<S> {
    source: S,
    selection_config: SelectionConfig,
    tts_config: TtsConfig,
    api_key: Option<String>,
    sink: AudioSink,
    stream_config: StreamConfig,
    report: Option<Arc<Mutex<SessionReport>>>,
    /// Invoked between pipeline construction and `start()`; lets tests drive
    /// cancellation at the most delicate point of initialisation. Kept as a
    /// deterministic injection seam, not a feature flag.
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

    /// Override audio tuning (volume, back-pressure cap, decoded capture) for tests.
    pub fn with_stream_config(mut self, stream_config: StreamConfig) -> Self {
        self.stream_config = stream_config;
        self
    }

    /// The audio tuning this body will use, including the configured volume.
    pub fn stream_config(&self) -> StreamConfig {
        self.stream_config
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
        let observer = SessionObserver {
            ctx,
            report: &self.report,
        };

        // Starting (set by the coordinator): read and validate the selection.
        let selected = match selection::read_selection_cancellable(
            &self.source,
            &self.selection_config,
            &ctx.cancel,
        ) {
            Ok(selected) => selected,
            Err(SelectionError::Cancelled) => {
                self.record_teardown(TeardownStep::SelectionProcessTerminated);
                observer.publish(SessionState::Stopping);
                return Ok(());
            }
            Err(err) => {
                observer.publish(SessionState::Stopping);
                return Err(SessionError::Selection(err));
            }
        };

        if ctx.cancel.is_cancelled() {
            observer.publish(SessionState::Stopping);
            return Ok(());
        }

        let client = match TtsClient::new(&self.tts_config, self.api_key.clone()) {
            Ok(client) => client,
            Err(err) => {
                observer.publish(SessionState::Stopping);
                return Err(SessionError::Tts(err));
            }
        };

        // Connecting: send the TTS request and begin receiving audio.
        observer.publish(SessionState::Connecting);

        // Cancellation can arrive while Connecting is published or while the
        // pipeline is being constructed; either way the pipeline is torn down
        // through the same ordered path as a cancellation during streaming.
        let player = Arc::new(
            StreamingMp3Player::new(self.sink.clone(), self.stream_config)
                .map_err(SessionError::Audio)?,
        );
        observer.player_opened();

        let accepting = Arc::new(AtomicBool::new(true));
        let http_cancel = Cancellation::new();
        let teardown = OrderedTeardown::new(
            PipelineTeardownTarget {
                accepting: Arc::clone(&accepting),
                http_cancel: http_cancel.clone(),
                player: Arc::clone(&player),
            },
            self.report.clone(),
        );

        if let Some(hook) = &self.init_hook {
            hook(ctx);
        }
        if ctx.cancel.is_cancelled() {
            teardown.perform();
            observer.publish(SessionState::Stopping);
            return Ok(());
        }
        match player.start() {
            Ok(()) => {}
            Err(AudioError::Cancelled) => {
                teardown.perform();
                observer.publish(SessionState::Stopping);
                return Ok(());
            }
            Err(err) => {
                observer.stop_pipeline(&player);
                observer.publish(SessionState::Stopping);
                return Err(SessionError::Audio(err));
            }
        }
        if ctx.cancel.is_cancelled() {
            teardown.perform();
            observer.publish(SessionState::Stopping);
            return Ok(());
        }

        // Only now, with the pipeline fully started, does the watchdog begin
        // watching for cancellation: its job is to unblock a `push()` stalled
        // on back-pressure, not to race `start()`'s state change.
        let mut watchdog = PipelineWatchdog::spawn(teardown.clone(), ctx.cancel.clone());

        // Buffering: audio is arriving but has not reached the sink yet.
        observer.publish(SessionState::Buffering);

        let mut writer = AudioSinkWriter {
            player: &player,
            observer,
            accepting: Arc::clone(&accepting),
            playing: false,
            start_deadline: None,
            cancelled: false,
            audio_error: None,
        };
        // The HTTP request is cancelled by teardown step 2, not directly by
        // `ctx.cancel`, so step 1 (stop accepting audio) really happens first.
        let stream_result = client.speak_to_cancellable(selected.text(), &mut writer, &http_cancel);

        // Cancellation wins over follow-on errors: once Stop arrives the HTTP
        // request has been aborted and the watchdog has stopped the pipeline,
        // so an `Interrupted` write or `Cancelled` push is not an error.
        let cancelled = ctx.cancel.is_cancelled()
            || !accepting.load(Ordering::Acquire)
            || writer.cancelled
            || matches!(stream_result, Err(TtsError::Cancelled));
        if cancelled {
            watchdog.disarm();
            teardown.perform();
            observer.publish(SessionState::Stopping);
            return Ok(());
        }
        if let Some(err) = writer.audio_error.take() {
            watchdog.disarm();
            observer.stop_pipeline(&player);
            observer.publish(SessionState::Stopping);
            return Err(SessionError::Audio(err));
        }
        if let Err(err) = stream_result {
            watchdog.disarm();
            observer.stop_pipeline(&player);
            observer.publish(SessionState::Stopping);
            return Err(SessionError::Tts(err));
        }

        // A very short stream can finish before the probe marks Started.
        if !writer.playing {
            observer.publish(SessionState::Playing);
        }

        let stats = match player.finish() {
            Ok(stats) => stats,
            Err(AudioError::Cancelled) => {
                watchdog.disarm();
                teardown.perform();
                observer.publish(SessionState::Stopping);
                return Ok(());
            }
            Err(err) => {
                watchdog.disarm();
                observer.stop_pipeline(&player);
                observer.publish(SessionState::Stopping);
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
        observer.record(TeardownStep::AudioPipelineStopped);
        observer.player_closed(&player);
        observer.publish(SessionState::Stopping);
        Ok(())
    }

    fn record_teardown(&self, step: TeardownStep) {
        record_teardown_step(&self.report, step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the *actual* teardown actions, not just their log entries, so a
    /// reordering or a repeated teardown fails the test (spec §6/§10).
    #[derive(Clone, Default)]
    struct RecordingTarget {
        actions: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RecordingTarget {
        fn push(&self, action: &'static str) {
            self.actions.lock().expect("actions lock").push(action);
        }
    }

    impl TeardownTarget for RecordingTarget {
        fn stop_accepting(&self) {
            self.push("stop_accepting_audio");
        }
        fn cancel_http(&self) {
            self.push("cancel_http");
        }
        fn stop_playback(&self) {
            self.push("stop_playback");
        }
        fn playback_closed(&self, _: &Option<Arc<Mutex<SessionReport>>>) {
            self.push("release_resources");
        }
    }

    #[test]
    fn ordered_teardown_performs_actions_once_in_the_required_order() {
        let target = RecordingTarget::default();
        let actions = Arc::clone(&target.actions);
        let report = Arc::new(Mutex::new(SessionReport::default()));
        let teardown = OrderedTeardown::new(target, Some(Arc::clone(&report)));

        teardown.perform();
        teardown.perform(); // the watchdog and the owner thread may both call it

        assert_eq!(
            *actions.lock().expect("actions lock"),
            vec![
                "stop_accepting_audio",
                "cancel_http",
                "stop_playback",
                "release_resources",
            ],
            "teardown actions must happen in §10 order and only once"
        );
        let snapshot = report.lock().expect("report lock").clone();
        assert_eq!(
            snapshot.teardown,
            vec![
                TeardownStep::StoppedAcceptingAudio,
                TeardownStep::HttpRequestCancelled,
                TeardownStep::AudioPipelineStopped,
            ]
        );
    }
}
