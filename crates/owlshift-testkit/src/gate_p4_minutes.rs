//! A run's duration in minutes, as the build plan's run records write it.

/// Renders `ms` milliseconds as minutes with one decimal, rounded half up
/// ("25.6 min"). A duration under a minute stays in minutes: 24,000 ms is
/// "0.4 min".
pub fn as_minutes(ms: u64) -> String {
    // Tenths of a minute, rounded half up: ms / 6,000 + 1/2, in integers.
    let tenths = (u128::from(ms) * 2 + 6_000) / 12_000;
    format!("{}.{} min", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::as_minutes;

    #[test]
    fn zero_is_zero_minutes() {
        assert_eq!(as_minutes(0), "0.0 min");
    }

    #[test]
    fn under_a_minute_stays_in_minutes() {
        assert_eq!(as_minutes(24_000), "0.4 min");
    }

    #[test]
    fn rounds_up_to_a_full_minute() {
        assert_eq!(as_minutes(59_999), "1.0 min");
    }

    #[test]
    fn a_minute_is_one_minute() {
        assert_eq!(as_minutes(60_000), "1.0 min");
    }

    #[test]
    fn renders_a_run_record() {
        assert_eq!(as_minutes(1_536_000), "25.6 min");
    }

    #[test]
    fn rounds_half_up() {
        assert_eq!(as_minutes(2_999), "0.0 min");
        assert_eq!(as_minutes(3_000), "0.1 min");
    }

    #[test]
    fn the_largest_duration_does_not_overflow() {
        assert_eq!(as_minutes(u64::MAX), "307445734561825.9 min");
    }
}
