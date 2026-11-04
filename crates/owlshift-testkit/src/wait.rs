//! How a test waits for a state (OWL-493): for as long as load could make it
//! take, so that a busy machine never turns a run red, and for less only
//! where the design sets a bound. See "Waits in tests" under "The test
//! bench" in `docs/design/build-plan.md`.
//!
//! The standard library only: the CLI's, the platform's and the adapters'
//! tests include this file with `#[path]`, as they do `test_proc.rs`.

use std::time::{Duration, Instant};

/// How long a test waits for a state it only needs reached: far past what
/// load makes it take. A wait returns as soon as its state holds, so a
/// passing run pays nothing for it.
pub const REACHED: Duration = Duration::from_secs(120);

/// How long a stand-in the test means to stop, such as a fake's `sleep`,
/// lives on its own: twice [`REACHED`], so that no wait sees it end by
/// itself and passes by mistake.
pub const OUTLASTS: Duration = Duration::from_secs(2 * REACHED.as_secs());

/// Whether `done` holds within `within`, tried every 20 ms; true at once
/// when it already holds.
///
/// ```
/// use std::time::Duration;
/// use owlshift_testkit::wait;
///
/// assert!(wait::reached(Duration::ZERO, || true));
/// assert!(!wait::reached(Duration::from_millis(50), || false));
/// ```
pub fn reached(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until `done` holds, [`REACHED`] at most; panics naming `what`
/// otherwise.
#[track_caller]
pub fn until(what: &str, done: impl FnMut() -> bool) {
    until_with(what, String::new, done);
}

/// [`until`], with `context`, read only on a failure, such as a log, in the
/// panic.
#[track_caller]
pub fn until_with(what: &str, context: impl FnOnce() -> String, done: impl FnMut() -> bool) {
    within(REACHED, what, context, done);
}

/// Waits until `done` holds within `bound`, which only a bound the design
/// sets justifies: the caller cites it. Panics naming `what`, with
/// `context`, otherwise.
#[track_caller]
pub fn within(
    bound: Duration,
    what: &str,
    context: impl FnOnce() -> String,
    done: impl FnMut() -> bool,
) {
    if !reached(bound, done) {
        panic!("still waiting for {what} after {:?}\n{}", bound, context());
    }
}
