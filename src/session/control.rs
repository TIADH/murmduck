//! Session-level control state shared by the owner loop and the control server.
//!
//! A session may run several readings in a row (replacement). The lock, the
//! control socket and this state live for the whole session; each reading gets
//! its own [`Cancellation`] so a replacement is never born cancelled:
//!
//! * `stop` ends the whole session (Stop command / final teardown).
//! * a pending replacement asks the owner to start one more reading after the
//!   current one has stopped.
//! * `attempt_cancel` is the cancellation of the reading currently running,
//!   which a Stop or replacement command interrupts immediately.
//!
//! Admission, pending-replacement handoff and shutdown all mutate one
//! mutex-guarded [`ControlState`], so none of these multi-step operations can
//! observe a half-updated state. Separate atomics were not enough: a
//! replacement could be acknowledged in the gap between "accepting" and
//! "enqueued", then dropped by shutdown (spec §6).
//!
//! Selected text and credentials never pass through this state.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::cancellation::Cancellation;

/// Which reading a replacement should start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingKind {
    /// Read the current PRIMARY selection (`read-selection`, `toggle`).
    Selection,
    /// Play the predefined sample (`test-voice`).
    TestVoice,
}

/// All mutable control state of one session, guarded by a single mutex.
#[derive(Debug)]
struct ControlState {
    /// False once the owner has begun final shutdown. A replacement arriving
    /// after that point is declined rather than acknowledged and dropped.
    accepting_replace: bool,
    /// The next reading requested by a control command. Last writer wins.
    pending_replace: Option<ReadingKind>,
    /// Cancellation of the reading currently running (not a token).
    attempt_cancel: Option<Cancellation>,
}

/// Shared control signals for one session owner.
#[derive(Debug, Clone)]
pub struct SessionControl {
    stop: Cancellation,
    state: Arc<Mutex<ControlState>>,
}

impl SessionControl {
    pub fn new() -> Self {
        Self {
            stop: Cancellation::new(),
            state: Arc::new(Mutex::new(ControlState {
                accepting_replace: true,
                pending_replace: None,
                attempt_cancel: None,
            })),
        }
    }

    /// Adapt a bare cancellation (used by manual `ControlServer::spawn` calls
    /// that have no replacement loop). Stop and replacement both act on it.
    pub fn from_cancel(cancel: Cancellation) -> Self {
        Self {
            stop: cancel.clone(),
            state: Arc::new(Mutex::new(ControlState {
                accepting_replace: true,
                pending_replace: None,
                attempt_cancel: Some(cancel),
            })),
        }
    }

    fn state(&self) -> MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// End the whole session: never start another reading after this.
    pub fn stop(&self) {
        self.stop.cancel();
        // One transition: decline future replacements, discard any pending one
        // (Stop wins) and cancel the running attempt.
        let mut state = self.state();
        state.accepting_replace = false;
        state.pending_replace = None;
        if let Some(cancel) = state.attempt_cancel.take() {
            cancel.cancel();
        }
    }

    /// Stop the current reading and ask the owner for one more of `kind`.
    ///
    /// Returns `false` when the owner has already begun shutting down: in that
    /// case the caller must acknowledge the command as declined, not accepted.
    pub fn request_replace(&self, kind: ReadingKind) -> bool {
        self.admit_replace(kind, || {})
    }

    /// Test-only injection point: `pause` runs at the most delicate moment of
    /// admission, after acceptance is decided and before the state is written.
    /// Production callers use [`Self::request_replace`] (a no-op pause).
    #[cfg(test)]
    pub fn request_replace_with_pause(&self, kind: ReadingKind, pause: impl Fn()) -> bool {
        self.admit_replace(kind, pause)
    }

    fn admit_replace(&self, kind: ReadingKind, pause: impl Fn()) -> bool {
        // Holding the lock across check + enqueue + cancellation is what closes
        // the documented shutdown race: no other transition can interleave.
        let mut state = self.state();
        if self.stop.is_cancelled() || !state.accepting_replace {
            return false;
        }
        pause();
        state.pending_replace = Some(kind); // last writer wins
        if let Some(cancel) = state.attempt_cancel.as_ref() {
            cancel.cancel();
        }
        true
    }

    /// Atomically stop accepting replacements and return one that arrived in
    /// the shutdown window. The owner loops once more for that reading; every
    /// later replacement is declined with an explicit acknowledgement.
    pub fn close_for_shutdown(&self) -> Option<ReadingKind> {
        let mut state = self.state();
        if self.stop.is_cancelled() || !state.accepting_replace {
            return None;
        }
        state.accepting_replace = false;
        state.pending_replace.take()
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub fn replace_requested(&self) -> bool {
        self.state().pending_replace.is_some()
    }

    pub fn replacement_kind(&self) -> ReadingKind {
        self.state()
            .pending_replace
            .unwrap_or(ReadingKind::Selection)
    }

    /// Atomically take a pending replacement (if any) and install the
    /// cancellation for the attempt about to start. A command arriving after
    /// this point cancels the *new* attempt instead of being lost at the
    /// handoff, and a replacement accepted meanwhile is kept for the next loop.
    pub fn begin_attempt(&self, fallback: ReadingKind) -> (ReadingKind, Cancellation) {
        let mut state = self.state();
        let kind = state.pending_replace.take().unwrap_or(fallback);
        let attempt = Cancellation::new();
        state.attempt_cancel = Some(attempt.clone());
        if self.stop.is_cancelled() {
            attempt.cancel();
        }
        (kind, attempt)
    }

    /// Set the cancellation observed by the next reading. Kept for callers that
    /// manage the attempt cancellation themselves; the owner loop uses
    /// [`Self::begin_attempt`] so take + install are one operation.
    pub fn set_attempt_cancel(&self, cancel: Cancellation) {
        self.state().attempt_cancel = Some(cancel);
    }

    /// The session-level cancellation, used by cleanup at final teardown.
    /// Named for what it returns: unlike `Cancellation::token()` this is a
    /// `&Cancellation`, not a `CancellationToken`.
    pub fn stop_cancellation(&self) -> &Cancellation {
        &self.stop
    }
}

impl Default for SessionControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    /// Force the exact interleaving the reviewer found: admission has decided
    /// to accept and is paused; shutdown runs; then admission commits. The
    /// invariant must hold whichever thread got there first — in the fixed
    /// implementation shutdown cannot observe the half-committed state.
    #[test]
    fn shutdown_cannot_steal_the_window_between_admission_and_commit() {
        let control = Arc::new(SessionControl::new());
        let (paused_tx, paused_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();

        let admit_control = Arc::clone(&control);
        let replace = thread::spawn(move || {
            admit_control.request_replace_with_pause(ReadingKind::TestVoice, || {
                paused_tx.send(()).expect("announce pause");
                go_rx.recv().expect("resume admission");
            })
        });

        paused_rx.recv().expect("admission reached its pause");
        // Shutdown runs in its own thread so the admission thread can always be
        // released, including when the fixed implementation serializes them.
        let shutdown_control = Arc::clone(&control);
        let shutdown = thread::spawn(move || shutdown_control.close_for_shutdown());
        go_tx.send(()).expect("let admission commit");

        let accepted = replace.join().expect("replacement thread joins");
        let taken = shutdown.join().expect("shutdown thread joins");
        assert!(
            !accepted || taken == Some(ReadingKind::TestVoice),
            "an accepted replacement was dropped: accepted={accepted}, taken={taken:?}"
        );
    }

    /// The next-attempt handoff must consume a pending replacement atomically
    /// and install the new cancellation under the same synchronization.
    #[test]
    fn begin_attempt_takes_a_pending_replace_and_installs_a_fresh_cancellation() {
        let control = SessionControl::new();
        let (first_kind, first_attempt) = control.begin_attempt(ReadingKind::Selection);
        assert_eq!(first_kind, ReadingKind::Selection);
        assert!(!first_attempt.is_cancelled());

        assert!(control.request_replace(ReadingKind::TestVoice));
        assert!(
            first_attempt.is_cancelled(),
            "a replacement must interrupt the reading currently running"
        );

        // A second replacement accepted at the handoff overwrites the first
        // (last-wins) and is never silently discarded.
        assert!(control.request_replace(ReadingKind::Selection));
        let (next_kind, next_attempt) = control.begin_attempt(ReadingKind::TestVoice);
        assert_eq!(
            next_kind,
            ReadingKind::Selection,
            "the second replacement must survive the handoff"
        );
        assert!(
            !next_attempt.is_cancelled(),
            "a replacement is not born cancelled"
        );
        assert!(
            !control.replace_requested(),
            "the taken request must not remain pending"
        );

        // A replacement arriving after begin_attempt cancels the new attempt,
        // not the stale one.
        assert!(control.request_replace(ReadingKind::TestVoice));
        assert!(
            next_attempt.is_cancelled(),
            "a late replacement must cancel the new attempt"
        );
    }
}
