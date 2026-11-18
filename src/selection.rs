//! PRIMARY-selection reading, validation and privacy-safe reporting.
//!
//! Text is obtained through the [`SelectionSource`] abstraction so the logic
//! is unit-testable without a Wayland display. The production source is
//! [`WaylandPrimary`], which shells out to `wl-paste`. Selected text is never
//! logged: only its size is reported.

use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::cancellation::Cancellation;
use crate::config::SelectionConfig;
use crate::logging;

/// Problems encountered while reading or validating the PRIMARY selection.
///
/// Variants carry only metadata (counts, status codes, tool diagnostics) —
/// never the selected text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// The PRIMARY selection was empty.
    Empty,
    /// The selection exceeded the configured character limit.
    TooLong { max: usize, actual: usize },
    /// The provider produced more bytes than the read cap allows. Detected
    /// *during* the read so a runaway provider cannot grow memory without
    /// bound; the child is terminated and reaped before this is returned.
    OutputTooLarge { max_bytes: usize },
    /// The external provider process exited with a non-zero status.
    ProviderFailed { status: String, detail: String },
    /// The provider produced bytes that are not valid UTF-8.
    InvalidUtf8,
    /// The provider did not finish within the configured timeout. The domain
    /// boundary uses `Duration`; the TOML key (`timeout_ms`) stays in config.
    Timeout { after: Duration },
    /// The provider process could not be started.
    Spawn { detail: String },
    /// Reading the provider's output failed.
    Io { detail: String },
    /// The session was cancelled while the provider was running; the process
    /// was terminated and reaped.
    Cancelled,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "PRIMARY selection is empty"),
            Self::TooLong { max, actual } => write!(
                f,
                "PRIMARY selection is too long: {actual} characters (limit is {max})"
            ),
            Self::OutputTooLarge { max_bytes } => write!(
                f,
                "selection provider output exceeded the {max_bytes}-byte read cap"
            ),
            Self::ProviderFailed { status, detail } => {
                if detail.is_empty() {
                    write!(f, "selection provider failed ({status})")
                } else {
                    write!(f, "selection provider failed ({status}): {detail}")
                }
            }
            Self::InvalidUtf8 => write!(f, "selection provider returned invalid UTF-8 text"),
            Self::Timeout { after } => {
                write!(
                    f,
                    "selection provider timed out after {} ms",
                    after.as_millis()
                )
            }
            Self::Spawn { detail } => {
                write!(f, "could not start selection provider: {detail}")
            }
            Self::Io { detail } => write!(f, "error reading selection provider: {detail}"),
            Self::Cancelled => write!(f, "selection reading cancelled"),
        }
    }
}

impl Error for SelectionError {}

/// A source of PRIMARY-selection text.
///
/// This is the seam tests inject against: a fake returns canned text or an
/// error, letting validation and error handling be exercised without the real
/// `wl-paste` subprocess or a Wayland session.
pub trait SelectionSource {
    /// Read the raw text currently held in the PRIMARY selection.
    fn read_text(&self) -> Result<String, SelectionError>;

    /// Read while watching `cancel`. Sources with a subprocess override this
    /// so cancellation terminates the child; simple sources keep the default.
    fn read_text_cancellable(&self, cancel: &Cancellation) -> Result<String, SelectionError> {
        let _ = cancel;
        self.read_text()
    }
}

/// Short predefined text played by `test-voice` (spec §7). Unlike a PRIMARY
/// selection this is not user data, but it still travels through the same
/// validated text path and is never included in log messages.
pub const TEST_VOICE_SAMPLE: &str =
    "Hello from Murmduck. This is a short test of your configured voice.";

/// A source that always yields fixed text. Used by `test-voice` and as a fake
/// in tests, so no Wayland display is needed to drive a session.
#[derive(Debug, Clone)]
pub struct StaticSource {
    text: String,
}

impl StaticSource {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl SelectionSource for StaticSource {
    fn read_text(&self) -> Result<String, SelectionError> {
        Ok(self.text.clone())
    }
}

/// Validated text read from the PRIMARY selection.
///
/// `Debug` is implemented by hand to redact the contents so an accidental
/// `{:?}` in a log statement can never expose selected text.
#[derive(Clone)]
pub struct Selection {
    text: String,
}

impl Selection {
    /// Borrow the selected text for downstream use (e.g. a TTS request).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Number of Unicode characters in the selection.
    pub fn char_count(&self) -> usize {
        self.text.chars().count()
    }
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selection")
            .field("char_count", &self.char_count())
            .field("text", &"<redacted>")
            .finish()
    }
}

/// Validate raw provider text and wrap it in a [`Selection`].
///
/// Shared by both read paths so empty/oversized handling cannot drift. Only a
/// size summary is logged — never the text itself.
fn validate_selection(text: String, config: &SelectionConfig) -> Result<Selection, SelectionError> {
    if text.is_empty() {
        return Err(SelectionError::Empty);
    }

    let char_count = text.chars().count();
    if char_count > config.max_chars {
        return Err(SelectionError::TooLong {
            max: config.max_chars,
            actual: char_count,
        });
    }

    logging::log_selection_summary(&text);
    Ok(Selection { text })
}

/// Read and validate the PRIMARY selection through `source`.
///
/// Empty and oversized selections are rejected before any downstream work is
/// possible. On success a size-only summary is logged and the text is returned
/// inside [`Selection`]; the text itself is never written to logs.
pub fn read_selection(
    source: &dyn SelectionSource,
    config: &SelectionConfig,
) -> Result<Selection, SelectionError> {
    validate_selection(source.read_text()?, config)
}

/// Cancellation-aware counterpart to [`read_selection`]. A cancelled read
/// reports [`SelectionError::Cancelled`] and guarantees the provider process
/// has been terminated and reaped before returning.
pub fn read_selection_cancellable(
    source: &dyn SelectionSource,
    config: &SelectionConfig,
    cancel: &Cancellation,
) -> Result<Selection, SelectionError> {
    validate_selection(source.read_text_cancellable(cancel)?, config)
}

/// Build the human-readable, text-free report for a successful read.
///
/// Only the character count is included; the selected text is never part of
/// this string, so it is safe to print to stdout.
pub fn success_report(selection: &Selection) -> String {
    format!(
        "Read {} characters from the PRIMARY selection.",
        selection.char_count()
    )
}

/// Maximum number of characters of provider stderr kept in an error message.
/// Provider diagnostics are safe to include (they never contain the selection),
/// but bounding them keeps errors readable.
const STDERR_DETAIL_LIMIT: usize = 200;

/// UTF-8 uses at most four bytes per character. The stdout cap is the worst
/// case for one more character than the limit (so an oversized selection is
/// still detected) plus a small allowance for control bytes.
const BYTES_PER_CHAR: usize = 4;
const LIMIT_DETECTION_ALLOWANCE: usize = 64;

/// Byte cap used when a provider has no explicit configured limit. Matches the
/// documented default `selection.max_chars` so production is bounded even if a
/// caller builds a source without the configured limit.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 20_000 * BYTES_PER_CHAR + LIMIT_DETECTION_ALLOWANCE;

/// Result of a bounded pipe drain. `overflow` is reported as soon as it is
/// detected (before EOF) so the owner can terminate a runaway child promptly.
struct PipeRead {
    bytes: Vec<u8>,
    overflow: bool,
}

enum PipeResult {
    Read(PipeRead),
    Io(io::Error),
}

/// How often a cancellable read re-checks the signal while waiting on output.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// A [`SelectionSource`] backed by an external selection-provider subprocess.
///
/// In production this runs `wl-paste --primary --no-newline`. The command and
/// timeout are injectable so subprocess behaviour (timeouts, non-zero exits,
/// invalid UTF-8) can be tested without `wl-paste` or a Wayland display.
pub struct WaylandPrimary {
    program: String,
    args: Vec<String>,
    timeout: Duration,
    /// Hard cap on stdout bytes read from the provider, derived from
    /// `selection.max_chars` (UTF-8 worst case plus limit-detection room).
    max_output_bytes: usize,
}

impl WaylandPrimary {
    /// Build the standard Wayland provider: `wl-paste --primary --no-newline`.
    pub fn new(timeout: Duration) -> Self {
        Self {
            program: "wl-paste".to_owned(),
            args: vec!["--primary".to_owned(), "--no-newline".to_owned()],
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Build a provider that runs `program` with `args` under `timeout`.
    ///
    /// Primarily a testing seam: it lets the subprocess handling above be
    /// exercised with a stand-in command on hosts without `wl-paste`.
    pub fn with_command(program: impl Into<String>, args: &[&str], timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Bound stdout reads from the configured character limit. One extra
    /// four-byte character and a small allowance leave room to detect a
    /// too-long selection rather than truncating a valid one.
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_output_bytes = max_chars
            .saturating_add(1)
            .saturating_mul(BYTES_PER_CHAR)
            .saturating_add(LIMIT_DETECTION_ALLOWANCE);
        self
    }

    fn spawn(&self) -> io::Result<Child> {
        Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    fn timeout_error(&self) -> SelectionError {
        SelectionError::Timeout {
            after: self.timeout,
        }
    }
}

impl SelectionSource for WaylandPrimary {
    fn read_text(&self) -> Result<String, SelectionError> {
        self.read_text_inner(None)
    }

    fn read_text_cancellable(&self, cancel: &Cancellation) -> Result<String, SelectionError> {
        self.read_text_inner(Some(cancel))
    }
}

impl WaylandPrimary {
    fn read_text_inner(&self, cancel: Option<&Cancellation>) -> Result<String, SelectionError> {
        if cancel.is_some_and(|signal| signal.is_cancelled()) {
            return Err(SelectionError::Cancelled);
        }
        let mut child = self.spawn().map_err(|err| SelectionError::Spawn {
            detail: err.to_string(),
        })?;

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // Drain each pipe on its own bounded reader so a child that fills one
        // pipe cannot block while we wait on the other, and so neither pipe can
        // grow memory without limit. The main thread enforces the timeout and
        // kills the child if it overruns or stdout exceeds its byte cap.
        let stdout_rx = drain(stdout, self.max_output_bytes);
        let stderr_rx = drain(stderr, STDERR_DETAIL_LIMIT * BYTES_PER_CHAR);

        // `checked_add` guards against an extreme configured timeout
        // overflowing `Instant`; in that case cap the wait at one day.
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let stdout_bytes = loop {
            if cancel.is_some_and(|signal| signal.is_cancelled()) {
                kill_and_reap(&mut child);
                return Err(SelectionError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                kill_and_reap(&mut child);
                return Err(self.timeout_error());
            }
            match stdout_rx.recv_timeout(remaining.min(CANCEL_POLL)) {
                Ok(PipeResult::Read(PipeRead { bytes, overflow })) => {
                    if overflow {
                        kill_and_reap(&mut child);
                        return Err(SelectionError::OutputTooLarge {
                            max_bytes: self.max_output_bytes,
                        });
                    }
                    break bytes;
                }
                Ok(PipeResult::Io(err)) => {
                    kill_and_reap(&mut child);
                    return Err(SelectionError::Io {
                        detail: err.to_string(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    kill_and_reap(&mut child);
                    return Err(SelectionError::Io {
                        detail: "selection reader stopped unexpectedly".to_owned(),
                    });
                }
            }
        };

        // stdout has reached EOF; give the child the remaining budget to exit.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if cancel.is_some_and(|signal| signal.is_cancelled()) {
                        kill_and_reap(&mut child);
                        return Err(SelectionError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        kill_and_reap(&mut child);
                        return Err(self.timeout_error());
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => {
                    kill_and_reap(&mut child);
                    return Err(SelectionError::Io {
                        detail: err.to_string(),
                    });
                }
            }
        };

        let stderr_bytes = match stderr_rx.recv() {
            Ok(PipeResult::Read(read)) => read.bytes,
            _ => Vec::new(), // diagnostic truncation is not fatal
        };

        if !status.success() {
            let detail = String::from_utf8_lossy(&stderr_bytes)
                .trim()
                .chars()
                .take(STDERR_DETAIL_LIMIT)
                .collect();
            return Err(SelectionError::ProviderFailed {
                status: status.to_string(),
                detail,
            });
        }

        String::from_utf8(stdout_bytes).map_err(|_| SelectionError::InvalidUtf8)
    }
}

/// Read a pipe on a background thread, keeping at most `max_bytes` and
/// reporting overflow *before* EOF so the owner can terminate a runaway child.
/// The reader is still drained to EOF afterwards so the child never blocks on
/// a full pipe while the owner tears it down.
fn drain<R: Read + Send + 'static>(mut reader: R, max_bytes: usize) -> mpsc::Receiver<PipeResult> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut scratch = [0u8; 8 * 1024];
        let mut overflow_reported = false;
        loop {
            match reader.read(&mut scratch) {
                Ok(0) => break,
                Ok(n) => {
                    if overflow_reported {
                        continue; // keep draining, but stop storing bytes
                    }
                    if bytes.len() + n > max_bytes {
                        let keep = max_bytes.saturating_sub(bytes.len());
                        bytes.extend_from_slice(&scratch[..keep]);
                        let _ = tx.send(PipeResult::Read(PipeRead {
                            bytes: bytes.clone(),
                            overflow: true,
                        }));
                        overflow_reported = true;
                    } else {
                        bytes.extend_from_slice(&scratch[..n]);
                    }
                }
                Err(err) => {
                    let _ = tx.send(PipeResult::Io(err));
                    return;
                }
            }
        }
        if !overflow_reported {
            let _ = tx.send(PipeResult::Read(PipeRead {
                bytes,
                overflow: false,
            }));
        }
    });
    rx
}

/// Kill a subprocess and reap it, ignoring errors from an already-exited child.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
