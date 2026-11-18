//! Shared cancellation signal.
//!
//! One value is cloned into the session context, the control server, the
//! selection provider and the TTS client. Setting it has two effects:
//!
//! * a synchronous flag that tight polling loops can read (selection process
//!   teardown, audio-pipeline watchdog, the streaming writer), and
//! * a [`CancellationToken`] that wakes async work blocked in the HTTP request
//!   so the request can be aborted promptly instead of waiting for a timeout.
//!
//! `cancel()` is idempotent, so any number of control commands and cleanup
//! paths may signal it without coordination.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// How long a synchronous waiter sleeps before re-checking the flag.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Debug, Clone)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
    token: CancellationToken,
}

impl Cancellation {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            token: CancellationToken::new(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Request cancellation of the running session.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.token.cancel();
    }

    /// Token used by async work to race an operation against cancellation.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
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
