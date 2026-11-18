//! Deterministic concurrency tests for the session-level control state.
//!
//! These drive only [`SessionControl`]'s public API: a replacement must never
//! be acknowledged and then silently dropped when it races shutdown, and a
//! replacement accepted at the next-attempt handoff must survive it.

use std::sync::{Arc, Barrier};
use std::thread;

use murmduck::session::{ReadingKind, SessionControl};

/// A replacement and the shutdown transition share one serialized state
/// operation. Whatever the interleaving, an `Ok` acknowledgement implies the
/// owner received the reading; a refused replacement implies nothing was
/// queued.
#[test]
fn a_replacement_racing_shutdown_is_never_accepted_then_dropped() {
    for _ in 0..250 {
        let control = Arc::new(SessionControl::new());
        let barrier = Arc::new(Barrier::new(2));

        let replace_control = Arc::clone(&control);
        let replace_barrier = Arc::clone(&barrier);
        let replace = thread::spawn(move || {
            replace_barrier.wait();
            replace_control.request_replace(ReadingKind::TestVoice)
        });

        let shutdown_control = Arc::clone(&control);
        let shutdown_barrier = Arc::clone(&barrier);
        let shutdown = thread::spawn(move || {
            shutdown_barrier.wait();
            shutdown_control.close_for_shutdown()
        });

        let accepted = replace.join().expect("replacement thread joins");
        let taken = shutdown.join().expect("shutdown thread joins");
        assert!(
            !accepted || taken == Some(ReadingKind::TestVoice),
            "an accepted replacement was dropped: accepted={accepted}, taken={taken:?}"
        );
    }
}
