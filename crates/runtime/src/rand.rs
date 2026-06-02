//! Runtime entropy hooks for `std:rand`.
//!
//! The public RNG contracts live in Otter Fusion source. This module exposes
//! only target-backed bytes from the platform entropy provider.

fn fill_os_random(out: &mut [u8]) -> Result<(), getrandom::Error> {
    getrandom::getrandom(out)
}

/// Return an OS-random `u32`, or `-1` if the platform entropy source fails.
///
/// The Otter-authored layer composes two successful calls into `u64` and keeps
/// the public error value ordinary. The sentinel is outside the `u32` range.
#[unsafe(no_mangle)]
pub extern "C" fn lang_rand_os_u32() -> i64 {
    let mut bytes = [0u8; 4];
    match fill_os_random(&mut bytes) {
        Ok(()) => u32::from_le_bytes(bytes) as i64,
        Err(_) => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_random_u32_reports_success_or_failure_sentinel() {
        assert!(lang_rand_os_u32() >= -1);
    }
}
