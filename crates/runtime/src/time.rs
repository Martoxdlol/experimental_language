//! Runtime clock and local-offset hooks for `std:time`.
//!
//! The public Otter Fusion API is authored in `stdlib_src/std/time.otter`; this
//! module supplies private target-backed clock and local-offset helpers behind
//! async public futures. Values cross the ABI as encoded strings so `std:time`
//! remains a normal value module above the provider boundary. Public
//! `std:time.sleep(Duration)` does not expose a runtime sleep hook here; it
//! lowers to the reactor timer future in `async_rt`.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
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

fn local_offset_seconds_for_unix_nanos_native_wait(unix_nanos: i64) -> Option<i32> {
    crate::gc::native_wait(|| local_offset_seconds_for_unix_nanos(unix_nanos))
}

/// Return nanoseconds elapsed from a process-local monotonic epoch.
fn monotonic_nanos() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    clamp_nanos(duration_nanos(start.elapsed()))
}

/// Return nanoseconds since the Unix epoch according to the system clock.
fn system_nanos() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => clamp_nanos(duration_nanos(duration)),
        Err(err) => -clamp_nanos(duration_nanos(err.duration())),
    }
}

pub(crate) fn time_monotonic_nanos_encoded() -> String {
    format!("0{}", crate::gc::native_wait(monotonic_nanos))
}

pub(crate) fn time_system_nanos_encoded() -> String {
    format!("0{}", crate::gc::native_wait(system_nanos))
}

/// Return the selected provider's local UTC offset, in seconds, for a Unix
/// timestamp represented as nanoseconds. The payload is encoded as a success or
/// error string decoded by the Otter-authored `std:time` layer.
pub(crate) fn time_local_offset_seconds_encoded(unix_nanos: i64) -> String {
    match local_offset_seconds_for_unix_nanos_native_wait(unix_nanos) {
        Some(offset) => format!("0{offset}"),
        None => "1local timezone lookup is unavailable for this target or timestamp".to_string(),
    }
}

#[cfg(test)]
fn sleep_nanos_native_wait(nanos: i64) {
    if nanos <= 0 {
        return;
    }
    crate::gc::native_wait(|| std::thread::sleep(std::time::Duration::from_nanos(nanos as u64)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_clock_is_non_decreasing() {
        let a = monotonic_nanos();
        let b = monotonic_nanos();
        assert!(b >= a, "monotonic clock moved backwards: {a} -> {b}");
    }

    #[test]
    fn system_clock_is_after_unix_epoch_on_supported_hosts() {
        assert!(system_nanos() > 0);
    }

    #[test]
    fn encoded_clock_hooks_return_success_payloads() {
        assert!(time_monotonic_nanos_encoded().starts_with('0'));
        assert!(time_system_nanos_encoded().starts_with('0'));
    }

    #[test]
    fn nanosecond_clamp_bounds_are_stable() {
        assert_eq!(clamp_nanos(i64::MAX as i128 + 1), i64::MAX);
        assert_eq!(clamp_nanos(i64::MIN as i128 - 1), i64::MIN);
    }

    #[test]
    fn sleep_nanos_ignores_non_positive_durations() {
        sleep_nanos_native_wait(0);
        sleep_nanos_native_wait(-1);
    }

    #[test]
    fn sleep_nanos_accepts_positive_duration() {
        sleep_nanos_native_wait(1);
    }

    #[test]
    fn local_offset_hook_returns_reasonable_offset_or_error_sentinel() {
        let encoded = time_local_offset_seconds_encoded(system_nanos());
        if let Some(payload) = encoded.strip_prefix('0') {
            let offset = payload.parse::<i64>().expect("encoded offset is i64");
            assert!(
                (-86_400..=86_400).contains(&offset),
                "local UTC offset is out of the plausible one-day range: {offset}"
            );
        } else {
            assert!(encoded.starts_with('1'));
        }
    }

    #[test]
    fn unix_epoch_local_offset_matches_provider_shape() {
        let encoded = time_local_offset_seconds_encoded(0);
        if let Some(payload) = encoded.strip_prefix('0') {
            let offset = payload.parse::<i64>().expect("encoded offset is i64");
            assert!(
                (-86_400..=86_400).contains(&offset),
                "Unix epoch local UTC offset is out of range: {offset}"
            );
        } else {
            assert!(encoded.starts_with('1'));
        }
    }

    #[test]
    fn local_offset_provider_lookup_uses_native_state_marker() {
        let offset = local_offset_seconds_for_unix_nanos_native_wait(0);
        if let Some(offset) = offset {
            assert!(
                (-86_400..=86_400).contains(&i64::from(offset)),
                "Unix epoch local UTC offset is out of range: {offset}"
            );
        }
    }
}
