//! Session-level control state shared by the owner loop and the control server.
//!
//! A session may run several readings in a row (replacement). The lock, the
//! control socket and this state live for the whole session; each reading gets
//! its own [`Cancellation`] so a replacement is never born cancelled:
//!
//! * `stop` ends the whole session (Stop command / final teardown).
//! * `replace_requested` asks the owner to start one more reading after the
//!   current one has stopped.
//! * `attempt` is the cancellation of the reading currently running, which a
//!   Stop or replacement command interrupts immediately.
//!
//! Selected text and credentials never pass through this state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::cancellation::Cancellation;

/// Which reading a replacement should start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingKind {
    /// Read the current PRIMARY selection (`read-selection`, `toggle`).
    Selection,
    /// Play the predefined sample (`test-voice`).
    TestVoice,
}

/// Shared control signals for one session owner.
#[derive(Debug, Clone)]
pub struct SessionControl {
    stop: Cancellation,
    replace_requested: Arc<AtomicBool>,
    replacement_kind: Arc<Mutex<ReadingKind>>,
    attempt: Arc<Mutex<Option<Cancellation>>>,
}

impl SessionControl {
    pub fn new() -> Self {
        Self {
            stop: Cancellation::new(),
            replace_requested: Arc::new(AtomicBool::new(false)),
            replacement_kind: Arc::new(Mutex::new(ReadingKind::Selection)),
            attempt: Arc::new(Mutex::new(None)),
        }
    }

    /// Adapt a bare cancellation (used by manual `ControlServer::spawn` calls
    /// that have no replacement loop). Stop and replacement both act on it.
    pub fn from_cancel(cancel: Cancellation) -> Self {
        Self {
            stop: cancel.clone(),
            replace_requested: Arc::new(AtomicBool::new(false)),
            replacement_kind: Arc::new(Mutex::new(ReadingKind::Selection)),
            attempt: Arc::new(Mutex::new(Some(cancel))),
        }
    }

    /// End the whole session: never start another reading after this.
    pub fn stop(&self) {
        self.stop.cancel();
        self.cancel_attempt();
    }

    /// Stop the current reading and ask the owner for one more of `kind`.
    pub fn request_replace(&self, kind: ReadingKind) {
        *self
            .replacement_kind
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = kind;
        self.replace_requested.store(true, Ordering::Release);
        self.cancel_attempt();
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub fn replace_requested(&self) -> bool {
        self.replace_requested.load(Ordering::Acquire)
    }

    pub fn replacement_kind(&self) -> ReadingKind {
        *self
            .replacement_kind
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// Replace the cancellation observed by the next reading.
    pub fn set_attempt(&self, cancel: Cancellation) {
        *self.attempt.lock().unwrap_or_else(|err| err.into_inner()) = Some(cancel);
    }

    /// Consume a pending replacement request before starting a reading, so a
    /// command arriving *during* that reading is seen on the next iteration.
    pub fn clear_replace(&self) {
        self.replace_requested.store(false, Ordering::Release);
    }

    /// The session-level cancellation, used by cleanup at final teardown.
    pub fn token(&self) -> &Cancellation {
        &self.stop
    }

    fn cancel_attempt(&self) {
        if let Some(cancel) = self
            .attempt
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
        {
            cancel.cancel();
        }
    }
}

impl Default for SessionControl {
    fn default() -> Self {
        Self::new()
    }
}
