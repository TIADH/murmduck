//! Shared cancellation signal.
//!
//! One value is cloned into the session context, the control server, the
//! selection provider and the TTS client. Setting it has two effects:
//!
//! * a synchronous flag that tight polling loops can read (selection process
//!   teardown, the streaming writer), and
//! * a [`CancellationToken`] that wakes async work blocked in the HTTP request
//!   so the request can be aborted promptly instead of waiting for a timeout.
//!
//! Waiters block on a condition variable rather than polling: `cancel()` wakes
//! them immediately, and `notify()` wakes a watcher that is being disarmed
//! without changing the cancellation state (used by the audio watchdog).
//!
//! `cancel()` is idempotent, so any number of control commands and cleanup
//! paths may signal it without coordination.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    token: CancellationToken,
    /// Notification channel for synchronous waiters. The mutex is held while
    /// changing the condition and notifying, so a wakeup cannot be lost between
    /// a waiter's predicate check and its `wait`.
    notify: Arc<(Mutex<()>, Condvar)>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            token: CancellationToken::new(),
            notify: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Request cancellation of the running session.
    pub fn cancel(&self) {
        let first = !self.flag.swap(true, Ordering::AcqRel);
        self.token.cancel();
        if first {
            let (lock, cv) = &*self.notify;
            let _guard = lock.lock().unwrap_or_else(|err| err.into_inner());
            cv.notify_all();
        }
    }

    /// Token used by async work to race an operation against cancellation.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Wake synchronous waiters without setting the cancellation flag. Used to
    /// disarm a watcher that would otherwise wait until cancellation.
    pub fn notify(&self) {
        let (lock, cv) = &*self.notify;
        let _guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        cv.notify_all();
    }

    /// Wait until cancelled or `timeout` elapses. Returns `true` if cancelled.
    ///
    /// `Duration::MAX` is handled without overflowing `Instant`.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let (lock, cv) = &*self.notify;
        let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        loop {
            if self.is_cancelled() {
                return true;
            }
            match deadline.map(|deadline| deadline.saturating_duration_since(Instant::now())) {
                Some(remaining) if remaining.is_zero() => return false,
                Some(remaining) => {
                    let (next, _) = cv
                        .wait_timeout(guard, remaining)
                        .unwrap_or_else(|err| err.into_inner());
                    guard = next;
                }
                None => guard = cv.wait(guard).unwrap_or_else(|err| err.into_inner()),
            }
        }
    }

    /// Block until cancelled or `predicate` becomes true. The predicate is
    /// evaluated while holding the notification lock, so a disarm that calls
    /// [`Self::notify`] cannot be lost in the gap between the check and wait.
    pub fn wait_until<F: Fn() -> bool>(&self, predicate: F) {
        let (lock, cv) = &*self.notify;
        let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        while !self.is_cancelled() && !predicate() {
            guard = cv.wait(guard).unwrap_or_else(|err| err.into_inner());
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    use super::*;

    #[test]
    fn wait_until_wakes_on_notify_without_setting_cancelled() {
        let signal = Cancellation::new();
        let done = Arc::new(AtomicBool::new(false));
        let waiter_done = Arc::clone(&done);
        let waiter_signal = signal.clone();
        let waiter = thread::spawn(move || {
            waiter_signal.wait_until(|| waiter_done.load(Ordering::Acquire));
            waiter_signal.is_cancelled()
        });

        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        done.store(true, Ordering::Release);
        signal.notify();
        let cancelled = waiter.join().expect("waiter joins");
        assert!(
            !cancelled,
            "a disarm notification must not cancel the session"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the waiter should wake immediately on notify"
        );
    }

    #[test]
    fn wait_until_wakes_promptly_on_cancel() {
        let signal = Cancellation::new();
        let waiter_signal = signal.clone();
        let waiter = thread::spawn(move || {
            waiter_signal.wait_until(|| false);
            waiter_signal.is_cancelled()
        });

        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        signal.cancel();
        assert!(waiter.join().expect("waiter joins"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the waiter should wake immediately on cancel"
        );
    }
}
