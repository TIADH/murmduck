//! Session-body seam: cancellation, shared lifecycle state and the running
//! body. This module is what a later milestone plugs the real TTS/audio
//! pipeline into; the owner's lock/IPC machinery does not change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::SelectionConfig;
use crate::selection::{self, SelectionSource};
use crate::session::{SessionError, SessionState};

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
