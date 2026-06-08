//! Runtime `str` (`LangStr`): UTF-8 byte strings, primitive→str conversions,
//! comparisons, substring/case ops, and `print`/`println`. Split from `lib.rs`.

use crate::gc;
use std::io::{Read, Write};

/// The runtime representation of a `str`: a managed object whose field block is
/// `[len: u64][UTF-8 bytes …]` (bytes inline). A `str` value is a pointer to
/// that field block; the GC header sits 16 bytes before it.
#[repr(C)]
pub struct LangStr {
    pub len: u64,
    // followed by `len` bytes, inline.
}

/// Allocate a managed `str` from `bytes` (copied inline).
fn make_str(bytes: &[u8]) -> *const LangStr {
    let len = bytes.len();
    let fb = unsafe { gc::alloc_var(gc::str_desc(), 8 + len) };
    unsafe {
        (fb as *mut u64).write(len as u64);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), fb.add(8), len);
    }
    fb as *const LangStr
}

/// View a `str` object's UTF-8 bytes.
///
/// # Safety
/// `s` must be a valid `str` field-block pointer.
pub unsafe fn str_bytes<'a>(s: *const LangStr) -> &'a [u8] {
    let len = unsafe { (s as *const u64).read() } as usize;
    unsafe { std::slice::from_raw_parts((s as *const u8).add(8), len) }
}

/// Construct a `str` from a static UTF-8 buffer (a string literal).
///
/// # Safety
/// `ptr` must point to at least `len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_from_utf8(ptr: *const u8, len: usize) -> *const LangStr {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    make_str(bytes)
}

/// Concatenate two `str`s into a new one (the `+` operator and interpolation).
///
/// # Safety
/// `a` and `b` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_concat(a: *const LangStr, b: *const LangStr) -> *const LangStr {
    let (a, b) = unsafe { (str_bytes(a), str_bytes(b)) };
    let mut bytes = Vec::with_capacity(a.len() + b.len());
    bytes.extend_from_slice(a);
    bytes.extend_from_slice(b);
    make_str(&bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_int_to_str(v: i64) -> *const LangStr {
    make_str(v.to_string().as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_uint_to_str(v: u64) -> *const LangStr {
    make_str(v.to_string().as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_float_to_str(v: f64) -> *const LangStr {
    make_str(v.to_string().as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_bool_to_str(v: i8) -> *const LangStr {
    make_str(if v != 0 { b"true" } else { b"false" })
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_char_to_str(v: u32) -> *const LangStr {
    let c = char::from_u32(v).unwrap_or('\u{FFFD}');
    make_str(c.to_string().as_bytes())
}

fn push_debug_escape(out: &mut String, c: char, quote: char) {
    match c {
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        '"' if quote == '"' => out.push_str("\\\""),
        '\'' if quote == '\'' => out.push_str("\\'"),
        c if c.is_control() => {
            use std::fmt::Write;
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
        c => out.push(c),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_debug_str(s: *const LangStr) -> *const LangStr {
    let mut out = String::from("\"");
    for c in unsafe { s_str(s) }.chars() {
        push_debug_escape(&mut out, c, '"');
    }
    out.push('"');
    make_str(out.as_bytes())
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_debug_char(v: u32) -> *const LangStr {
    let c = char::from_u32(v).unwrap_or('\u{FFFD}');
    let mut out = String::from("'");
    push_debug_escape(&mut out, c, '\'');
    out.push('\'');
    make_str(out.as_bytes())
}

/// Borrow a `str` object's contents as `&str` (lossily for invalid UTF-8).
unsafe fn s_str<'a>(s: *const LangStr) -> std::borrow::Cow<'a, str> {
    String::from_utf8_lossy(unsafe { str_bytes(s) })
}

/// `str.size()` — Unicode scalar count.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_size(s: *const LangStr) -> i64 {
    unsafe { s_str(s) }.chars().count() as i64
}

/// `str.byte_size()` — UTF-8 byte count.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_byte_size(s: *const LangStr) -> i64 {
    unsafe { str_bytes(s) }.len() as i64
}

/// Byte-wise `str` equality (`==`), 1 if equal.
///
/// # Safety
/// `a`/`b` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_eq(a: *const LangStr, b: *const LangStr) -> i8 {
    i8::from(unsafe { str_bytes(a) == str_bytes(b) })
}

/// Lexicographic `str` comparison by Unicode scalar (−1/0/1).
///
/// # Safety
/// `a`/`b` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_cmp(a: *const LangStr, b: *const LangStr) -> i64 {
    use std::cmp::Ordering::*;
    match unsafe { str_bytes(a).cmp(str_bytes(b)) } {
        Less => -1,
        Equal => 0,
        Greater => 1,
    }
}

/// `str.contains(sub)`.
///
/// # Safety
/// `s`/`sub` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_contains(s: *const LangStr, sub: *const LangStr) -> i8 {
    i8::from(unsafe { s_str(s) }.contains(unsafe { s_str(sub) }.as_ref()))
}

/// `str.starts_with(p)`.
///
/// # Safety
/// `s`/`p` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_starts_with(s: *const LangStr, p: *const LangStr) -> i8 {
    i8::from(unsafe { s_str(s) }.starts_with(unsafe { s_str(p) }.as_ref()))
}

/// `str.ends_with(p)`.
///
/// # Safety
/// `s`/`p` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_ends_with(s: *const LangStr, p: *const LangStr) -> i8 {
    i8::from(unsafe { s_str(s) }.ends_with(unsafe { s_str(p) }.as_ref()))
}

/// `str.substring(start, end)` over scalar (char) indices, half-open. Panics
/// out of range (`docs/18` §4).
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_substring(
    s: *const LangStr,
    start: i64,
    end: i64,
) -> *const LangStr {
    let chars: Vec<char> = unsafe { s_str(s) }.chars().collect();
    let n = chars.len();
    if start < 0 || end < 0 || start > end || end as usize > n {
        eprintln!("panic: substring({start}, {end}) out of range (len {n})");
        std::process::exit(101);
    }
    let sub: String = chars[start as usize..end as usize].iter().collect();
    make_str(sub.as_bytes())
}

/// `str.to_upper()`.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_to_upper(s: *const LangStr) -> *const LangStr {
    make_str(unsafe { s_str(s) }.to_uppercase().as_bytes())
}

/// `str.to_lower()`.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_to_lower(s: *const LangStr) -> *const LangStr {
    make_str(unsafe { s_str(s) }.to_lowercase().as_bytes())
}

/// `str.trim()`.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_trim(s: *const LangStr) -> *const LangStr {
    make_str(unsafe { s_str(s) }.trim().as_bytes())
}

/// `s.index_of(needle)` (`docs/18`): the byte index of the first occurrence of
/// `needle`, or `-1` if absent (codegen turns `-1` into the `null` variant).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_index_of(s: *const LangStr, needle: *const LangStr) -> i64 {
    let hay = unsafe { s_str(s) };
    let n = unsafe { s_str(needle) };
    match hay.find(&*n) {
        Some(i) => i as i64,
        None => -1,
    }
}

/// `s.repeat(n)` (`docs/18`): the string repeated `n` times (`n <= 0` → empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_repeat(s: *const LangStr, n: i64) -> *const LangStr {
    let count = if n < 0 { 0 } else { n as usize };
    make_str(unsafe { s_str(s) }.repeat(count).as_bytes())
}

/// `s.replace(old, new)` (`docs/18`): every non-overlapping occurrence of `old`
/// replaced with `new`. The result String is built before allocating, so the
/// managed inputs need not survive `make_str`'s safepoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_replace(
    s: *const LangStr,
    old: *const LangStr,
    new: *const LangStr,
) -> *const LangStr {
    let base = unsafe { s_str(s) };
    let from = unsafe { s_str(old) };
    let to = unsafe { s_str(new) };
    let out = base.replace(&*from, &*to);
    make_str(out.as_bytes())
}

/// `s.split(sep)` (`docs/18`): a `List<str>` of the substrings between each
/// occurrence of `sep`. An empty separator splits into individual characters
/// (matching Rust's behaviour minus the empty edge fragments). The result list
/// and its element strings are built under a GC pause: they are freshly
/// allocated and not yet reachable from any stack root, so a collection mid-way
/// must not reclaim them.
///
/// # Safety
/// `s` and `sep` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_split(s: *const LangStr, sep: *const LangStr) -> *mut u8 {
    let hay = unsafe { s_str(s) }.into_owned();
    let pat = unsafe { s_str(sep) }.into_owned();
    let parts: Vec<String> = if pat.is_empty() {
        hay.chars().map(|c| c.to_string()).collect()
    } else {
        hay.split(pat.as_str()).map(|p| p.to_string()).collect()
    };
    gc::pause();
    let list = unsafe { crate::list::lang_list_new(1, 0) }; // elements are managed `str`s
    for p in &parts {
        let item = make_str(p.as_bytes()) as i64;
        unsafe { crate::list::lang_list_push(list, item) };
    }
    gc::resume_with_return_root(list as usize);
    list
}

/// `s.chars()` (`docs/18` §4): a `List<char>` snapshot of the string's Unicode
/// scalars (codepoints stored in the `i64` element slots). Built under a GC
/// pause — the fresh list is not yet reachable from a stack root. The codegen
/// wraps this list in a prelude `StrChars` iterator struct.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_to_chars(s: *const LangStr) -> *mut u8 {
    let chars: Vec<char> = unsafe { s_str(s) }.chars().collect();
    gc::pause();
    let list = unsafe { crate::list::lang_list_new(0, 0) }; // codepoints are plain values
    for c in &chars {
        unsafe { crate::list::lang_list_push(list, *c as i64) };
    }
    gc::resume_with_return_root(list as usize);
    list
}

/// `s.bytes()` (`docs/18` §4): a `List<u8>` snapshot of the string's UTF-8
/// bytes (each byte stored in an `i64` element slot). Built under a GC pause.
/// The codegen wraps this list in a prelude `StrBytes` iterator struct.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_to_bytes(s: *const LangStr) -> *mut u8 {
    let bytes: Vec<u8> = unsafe { str_bytes(s) }.to_vec();
    gc::pause();
    let list = unsafe { crate::list::lang_list_new(0, 0) }; // bytes are plain values
    for b in &bytes {
        unsafe { crate::list::lang_list_push(list, i64::from(*b)) };
    }
    gc::resume_with_return_root(list as usize);
    list
}

/// `s.get(i)` (`docs/18`): the `i`-th Unicode scalar's codepoint, or `-1` if `i`
/// is out of range (codegen turns `-1` into the `char | null` `null` variant).
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_str_char_at(s: *const LangStr, i: i64) -> i64 {
    if i < 0 {
        return -1;
    }
    match unsafe { s_str(s) }.chars().nth(i as usize) {
        Some(c) => c as i64,
        None => -1,
    }
}

fn encode_io_success_string(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    out
}

fn encode_io_error_string(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    out
}

fn encode_io_bytes_hex_string(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut payload = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        payload.push(HEX[(byte >> 4) as usize] as char);
        payload.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encode_io_success_string(&payload)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_io_hex_bytes(hex: &str) -> std::io::Result<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "odd-length io byte payload",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = decode_hex_digit(bytes[i]).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid io byte payload")
        })?;
        let lo = decode_hex_digit(bytes[i + 1]).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid io byte payload")
        })?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

pub(crate) fn io_write_stream_bytes_encoded(hex: &str, stderr: bool) -> String {
    let bytes = match decode_io_hex_bytes(&hex) {
        Ok(bytes) => bytes,
        Err(error) => return encode_io_error_string(error),
    };
    let result = gc::native_wait(|| {
        if stderr {
            let stream = std::io::stderr();
            let mut lock = stream.lock();
            lock.write_all(&bytes)
        } else {
            let stream = std::io::stdout();
            let mut lock = stream.lock();
            lock.write_all(&bytes)
        }
    });
    match result {
        Ok(()) => encode_io_success_string(&bytes.len().to_string()),
        Err(error) => encode_io_error_string(error),
    }
}

pub(crate) fn io_flush_stream_encoded(stderr: bool) -> String {
    let result = gc::native_wait(|| {
        if stderr {
            std::io::stderr().lock().flush()
        } else {
            std::io::stdout().lock().flush()
        }
    });
    match result {
        Ok(()) => encode_io_success_string(""),
        Err(error) => encode_io_error_string(error),
    }
}

pub(crate) fn io_read_stdin_count_encoded(count: i64) -> String {
    if count < 0 {
        return encode_io_error_string("negative stdin read size");
    }
    let mut buf = vec![0u8; count as usize];
    let result = gc::native_wait(|| std::io::stdin().lock().read(&mut buf));
    match result {
        Ok(n) => {
            buf.truncate(n);
            encode_io_bytes_hex_string(&buf)
        }
        Err(error) => encode_io_error_string(error),
    }
}

pub(crate) fn io_read_stdin_to_end_encoded() -> String {
    let mut buf = Vec::new();
    match gc::native_wait(|| std::io::stdin().lock().read_to_end(&mut buf)) {
        Ok(_) => encode_io_bytes_hex_string(&buf),
        Err(error) => encode_io_error_string(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(s: &str) -> *const LangStr {
        unsafe { lang_str_from_utf8(s.as_ptr(), s.len()) }
    }

    fn decode(s: *const LangStr) -> String {
        String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
    }

    #[test]
    fn raw_stream_hooks_write_flush_and_report_bad_hex() {
        assert_eq!(io_write_stream_bytes_encoded("4f4b", false), "02");
        assert_eq!(io_write_stream_bytes_encoded("455252", true), "03");
        assert_eq!(io_flush_stream_encoded(false), "0");
        assert_eq!(io_flush_stream_encoded(true), "0");
        assert!(io_write_stream_bytes_encoded("0x", false).starts_with('1'));
    }

    #[test]
    fn concrete_stdio_hooks_use_native_state_marker() {
        let source = include_str!("strings.rs");
        assert!(
            source.contains("pub(crate) fn io_write_stream_bytes_encoded(hex: &str, stderr: bool)")
                && source.contains("let result = gc::native_wait(|| {\n        if stderr {"),
            "stdout/stderr write helpers must mark host stream writes with gc::native_wait(...)"
        );
        assert!(
            source.contains("pub(crate) fn io_flush_stream_encoded(stderr: bool)")
                && source.contains("let result = gc::native_wait(|| {\n        if stderr {"),
            "stdout/stderr flush helpers must mark host stream flushes with gc::native_wait(...)"
        );
        assert!(
            source.contains("pub(crate) fn io_read_stdin_count_encoded(count: i64)")
                && source.contains(
                    "let result = gc::native_wait(|| std::io::stdin().lock().read(&mut buf));",
                ),
            "stdin read helper must mark host stream reads with gc::native_wait(...)"
        );
        assert!(
            source.contains("pub(crate) fn io_read_stdin_to_end_encoded()")
                && source.contains(
                    "match gc::native_wait(|| std::io::stdin().lock().read_to_end(&mut buf))",
                ),
            "stdin read_to_end helper must mark host stream reads with gc::native_wait(...)"
        );
    }

    #[test]
    fn debug_renderers_quote_and_escape_strings_and_chars() {
        assert_eq!(
            decode(unsafe { lang_debug_str(lang("hi\n\"otter")) }),
            "\"hi\\n\\\"otter\""
        );
        assert_eq!(decode(lang_debug_char('\n' as u32)), "'\\n'");
        assert_eq!(decode(lang_debug_char('\'' as u32)), "'\\''");
    }
}
