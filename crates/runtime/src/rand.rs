//! Runtime entropy hooks for `std:rand`.
//!
//! The public RNG contracts live in Otter Fusion source. This module exposes
//! only encoded target-backed bytes from the platform entropy provider. The
//! Otter-visible stdlib calls this through async runtime futures, so OS entropy
//! reaches user code only through explicit `Future`-returning public helpers.

fn fill_os_random(out: &mut [u8]) -> Result<(), getrandom::Error> {
    getrandom::getrandom(out)
}

fn encode_success_string(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    out
}

fn encode_error_string(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    out
}

fn bytes_hex_payload(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut payload = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        payload.push(HEX[(byte >> 4) as usize] as char);
        payload.push(HEX[(byte & 0x0f) as usize] as char);
    }
    payload
}

pub(crate) fn rand_os_bytes_encoded(count: i64) -> String {
    if count <= 0 {
        return encode_success_string("");
    }
    let len = match usize::try_from(count) {
        Ok(len) => len,
        Err(err) => return encode_error_string(err),
    };
    let mut bytes = Vec::new();
    if let Err(err) = bytes.try_reserve_exact(len) {
        return encode_error_string(err);
    }
    bytes.resize(len, 0);
    match crate::gc::native_wait(|| fill_os_random(&mut bytes)) {
        Ok(()) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_random_bytes_reports_encoded_success_or_failure() {
        let encoded = rand_os_bytes_encoded(4);
        assert!(encoded.starts_with('0') || encoded.starts_with('1'));
        if let Some(payload) = encoded.strip_prefix('0') {
            assert_eq!(payload.len(), 8);
        }
    }

    #[test]
    fn non_positive_os_random_request_is_empty_success() {
        assert_eq!(rand_os_bytes_encoded(0), "0");
        assert_eq!(rand_os_bytes_encoded(-1), "0");
    }
}
