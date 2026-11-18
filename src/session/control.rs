//! Session-level control state shared by the owner loop and the control server.
//!
//! A session may run several readings in a row (replacement). The lock, the
//! control socket and this state live for the whole session; each reading gets
//! its own [`Cancellation`] so a replacement is never born cancelled:
//!
//! * `stop` ends the whole session (Stop command / final teardown).
//! * `replace_requested` asks the owner to start one more reading after the
//!   current one has stopped.
//! * `attempt_cancel` is the cancellation of the reading currently running,
//!   which a Stop or replacement command interrupts immediately.
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
    /// Cancellation of the reading currently running (not a token).
    attempt_cancel: Arc<Mutex<Option<Cancellation>>>,
    /// False once the owner has begun final shutdown. A replacement arriving
    /// after that point is declined rather than acknowledged and dropped (§6).
    accepting_replace: Arc<AtomicBool>,
}

impl SessionControl {
    pub fn new() -> Self {
        Self {
            stop: Cancellation::new(),
            replace_requested: Arc::new(AtomicBool::new(false)),
            replacement_kind: Arc::new(Mutex::new(ReadingKind::Selection)),
            attempt_cancel: Arc::new(Mutex::new(None)),
            accepting_replace: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Adapt a bare cancellation (used by manual `ControlServer::spawn` calls
    /// that have no replacement loop). Stop and replacement both act on it.
    pub fn from_cancel(cancel: Cancellation) -> Self {
        Self {
            stop: cancel.clone(),
            replace_requested: Arc::new(AtomicBool::new(false)),
            replacement_kind: Arc::new(Mutex::new(ReadingKind::Selection)),
            attempt_cancel: Arc::new(Mutex::new(Some(cancel))),
            accepting_replace: Arc::new(AtomicBool::new(true)),
        }
    }

    /// End the whole session: never start another reading after this.
    pub fn stop(&self) {
        self.stop.cancel();
        self.cancel_attempt();
    }

    /// Stop the current reading and ask the owner for one more of `kind`.
    ///
    /// Returns `false` when the owner has already begun shutting down: in that
    /// case the caller must acknowledge the command as declined, not accepted.
    pub fn request_replace(&self, kind: ReadingKind) -> bool {
        if self.stop_requested() || !self.accepting_replace.load(Ordering::Acquire) {
            return false;
        }
        *self
            .replacement_kind
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = kind;
        self.replace_requested.store(true, Ordering::Release);
        self.cancel_attempt();
        true
    }

    /// Atomically stop accepting replacements and return one that arrived in
    /// the shutdown window. The owner loops once more for that reading; every
    /// later replacement is declined with an explicit acknowledgement.
    pub fn close_for_shutdown(&self) -> Option<ReadingKind> {
        if self.stop_requested() || !self.accepting_replace.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.replace_requested
            .swap(false, Ordering::AcqRel)
            .then(|| self.replacement_kind())
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

    /// Set the cancellation observed by the next reading.
    pub fn set_attempt_cancel(&self, cancel: Cancellation) {
        *self
            .attempt_cancel
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(cancel);
    }

    /// Consume a pending replacement request before starting a reading, so a
    /// command arriving *during* that reading is seen on the next iteration.
    pub fn clear_replace(&self) {
        self.replace_requested.store(false, Ordering::Release);
    }

    /// The session-level cancellation, used by cleanup at final teardown.
    /// Named for what it returns: unlike `Cancellation::token()` this is a
    /// `&Cancellation`, not a `CancellationToken`.
    pub fn stop_cancellation(&self) -> &Cancellation {
        &self.stop
    }

    fn cancel_attempt(&self) {
        if let Some(cancel) = self
            .attempt_cancel
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
