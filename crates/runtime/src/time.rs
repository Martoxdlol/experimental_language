//! Runtime clock hooks for `std:time`.
//!
//! The public Otter Fusion API is authored in `stdlib_src/std/time.otter`; this
//! module supplies only the target-backed clock readings. Values cross the ABI
//! as signed nanoseconds so `std:time` remains a normal value module above the
//! provider boundary.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const LOCAL_OFFSET_ERROR: i64 = i64::MIN;

fn clamp_nanos(nanos: i128) -> i64 {
    nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn duration_nanos(duration: std::time::Duration) -> i128 {
    duration.as_secs() as i128 * 1_000_000_000 + duration.subsec_nanos() as i128
}

fn floor_div_i64(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if r < 0 { q - 1 } else { q }
}

fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let mut y = year as i64;
    let m = month as i64;
    let d = day as i64;
    if m <= 2 {
        y -= 1;
    }
    let era = floor_div_i64(y, 400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn tm_as_unix_seconds(tm: &libc::tm) -> i64 {
    let year = tm.tm_year + 1900;
    let month = (tm.tm_mon + 1) as u8;
    let day = tm.tm_mday as u8;
    days_from_civil(year, month, day) * 86_400
        + tm.tm_hour as i64 * 3_600
        + tm.tm_min as i64 * 60
        + tm.tm_sec as i64
}

#[cfg(unix)]
fn local_offset_seconds_for_unix_nanos(unix_nanos: i64) -> Option<i32> {
    let seconds = floor_div_i64(unix_nanos, NANOS_PER_SECOND);
    let raw = seconds as libc::time_t;
    if raw as i128 != seconds as i128 {
        return None;
    }

    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    unsafe {
        if libc::localtime_r(&raw, local.as_mut_ptr()).is_null() {
            return None;
        }
        if libc::gmtime_r(&raw, utc.as_mut_ptr()).is_null() {
            return None;
        }
        let local = local.assume_init();
        let utc = utc.assume_init();
        let offset = tm_as_unix_seconds(&local) - tm_as_unix_seconds(&utc);
        i32::try_from(offset).ok()
    }
}

#[cfg(not(unix))]
fn local_offset_seconds_for_unix_nanos(_unix_nanos: i64) -> Option<i32> {
    None
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

/// Return the selected provider's local UTC offset, in seconds, for a Unix
/// timestamp represented as nanoseconds.
///
/// `i64::MIN` is an error sentinel decoded by the Otter-authored `std:time`
/// layer into `TimeError`.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_local_offset_seconds(unix_nanos: i64) -> i64 {
    local_offset_seconds_for_unix_nanos(unix_nanos)
        .map(i64::from)
        .unwrap_or(LOCAL_OFFSET_ERROR)
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

    #[test]
    fn local_offset_hook_returns_reasonable_offset_or_error_sentinel() {
        let offset = lang_time_local_offset_seconds(lang_time_system_nanos());
        if offset != LOCAL_OFFSET_ERROR {
            assert!(
                (-86_400..=86_400).contains(&offset),
                "local UTC offset is out of the plausible one-day range: {offset}"
            );
        }
    }

    #[test]
    fn unix_epoch_local_offset_matches_provider_shape() {
        let offset = lang_time_local_offset_seconds(0);
        if offset != LOCAL_OFFSET_ERROR {
            assert!(
                (-86_400..=86_400).contains(&offset),
                "Unix epoch local UTC offset is out of range: {offset}"
            );
        }
    }
}
