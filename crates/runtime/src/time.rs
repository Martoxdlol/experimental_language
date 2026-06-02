//! Runtime clock hooks for `std:time`.
//!
//! The public Otter Fusion API is authored in `stdlib_src/std/time.otter`; this
//! module supplies only the target-backed clock readings. Values cross the ABI
//! as signed nanoseconds so `std:time` remains a normal value module above the
//! provider boundary.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn clamp_nanos(nanos: i128) -> i64 {
    nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn duration_nanos(duration: std::time::Duration) -> i128 {
    duration.as_secs() as i128 * 1_000_000_000 + duration.subsec_nanos() as i128
}

/// Return nanoseconds elapsed from a process-local monotonic epoch.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_monotonic_nanos() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    clamp_nanos(duration_nanos(start.elapsed()))
}

/// Return nanoseconds since the Unix epoch according to the system clock.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_system_nanos() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => clamp_nanos(duration_nanos(duration)),
        Err(err) => -clamp_nanos(duration_nanos(err.duration())),
    }
}

/// Block the current host thread for `nanos` nanoseconds.
///
/// Non-positive durations are a no-op. The public stdlib wrapper takes a
/// `Duration`; the runtime hook remains a plain integer ABI boundary.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_sleep_nanos(nanos: i64) {
    if nanos <= 0 {
        return;
    }
    std::thread::sleep(std::time::Duration::from_nanos(nanos as u64));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_clock_is_non_decreasing() {
        let a = lang_time_monotonic_nanos();
        let b = lang_time_monotonic_nanos();
        assert!(b >= a, "monotonic clock moved backwards: {a} -> {b}");
    }

    #[test]
    fn system_clock_is_after_unix_epoch_on_supported_hosts() {
        assert!(lang_time_system_nanos() > 0);
    }

    #[test]
    fn nanosecond_clamp_bounds_are_stable() {
        assert_eq!(clamp_nanos(i64::MAX as i128 + 1), i64::MAX);
        assert_eq!(clamp_nanos(i64::MIN as i128 - 1), i64::MIN);
    }

    #[test]
    fn sleep_nanos_ignores_non_positive_durations() {
        lang_time_sleep_nanos(0);
        lang_time_sleep_nanos(-1);
    }
}
