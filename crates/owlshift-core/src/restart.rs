//! When the background service starts a served project's watch again
//! (OWL-435, architecture section 5, "The background service", "Restarts"):
//! a watch that ends without being asked is started again after a delay
//! that doubles from a first one up to a ceiling, the count of ends in a row
//! starting over once a watch has run long enough, and the operator is told
//! once when a run of ends reaches a count.
//!
//! The times are given, never read: how long the watch that ended ran.

use std::time::Duration;

/// The rule a supervisor restarts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// The delay after the first end of a run of ends.
    pub first: Duration,
    /// The longest delay: each end of a run doubles the one before, up to it.
    pub max: Duration,
    /// A watch that ran at least this long before it ended starts a new run
    /// of ends.
    pub reset_after: Duration,
    /// The end of a run at which the operator is told, once.
    pub notify_after: u32,
}

impl RestartPolicy {
    /// The service's rule: 10 seconds doubling up to an hour, a run of ends
    /// starting over after a watch ran 10 minutes, the operator told at the
    /// third end in a row.
    pub const SERVICE: Self = Self {
        first: Duration::from_secs(10),
        max: Duration::from_secs(60 * 60),
        reset_after: Duration::from_secs(10 * 60),
        notify_after: 3,
    };
}

/// The ends in a row of one project's watch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restarts {
    in_a_row: u32,
}

/// What follows an end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restart {
    /// How long to wait before starting the watch again.
    pub delay: Duration,
    /// The ends in a row, this one included.
    pub in_a_row: u32,
    /// Whether this end is the one the operator is told of.
    pub notify: bool,
}

impl Restarts {
    /// Counts an end of a watch that ran `ran_for`, by `policy`: what to do
    /// next.
    pub fn ended(&mut self, policy: &RestartPolicy, ran_for: Duration) -> Restart {
        if ran_for >= policy.reset_after {
            self.in_a_row = 0;
        }
        self.in_a_row = self.in_a_row.saturating_add(1);
        let doublings = self.in_a_row - 1;
        let delay = 2u32
            .checked_pow(doublings)
            .and_then(|factor| policy.first.checked_mul(factor))
            .map_or(policy.max, |delay| delay.min(policy.max));
        Restart {
            delay,
            in_a_row: self.in_a_row,
            notify: self.in_a_row == policy.notify_after,
        }
    }

    /// The ends in a row counted so far.
    pub fn in_a_row(&self) -> u32 {
        self.in_a_row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_secs(1);

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn the_delay_doubles_from_ten_seconds_up_to_an_hour() {
        let policy = RestartPolicy::SERVICE;
        let mut restarts = Restarts::default();
        let delays: Vec<u64> = (0..12)
            .map(|_| restarts.ended(&policy, QUICK).delay.as_secs())
            .collect();
        assert_eq!(
            delays,
            [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600, 3600, 3600]
        );
        // Far past the ceiling, the arithmetic never overflows.
        for _ in 0..100 {
            assert_eq!(restarts.ended(&policy, QUICK).delay, secs(3600));
        }
    }

    #[test]
    fn the_operator_is_told_once_per_run_of_ends() {
        let policy = RestartPolicy::SERVICE;
        let mut restarts = Restarts::default();
        let told: Vec<bool> = (0..5)
            .map(|_| restarts.ended(&policy, QUICK).notify)
            .collect();
        assert_eq!(told, [false, false, true, false, false]);
        // A watch that ran ten minutes starts a new run: 10 s again, and
        // told again at its third end.
        let after = restarts.ended(&policy, secs(600));
        assert_eq!(
            (after.delay, after.in_a_row, after.notify),
            (secs(10), 1, false)
        );
        assert!(!restarts.ended(&policy, secs(599)).notify);
        assert!(restarts.ended(&policy, QUICK).notify);
        assert_eq!(restarts.in_a_row(), 3);
    }
}
