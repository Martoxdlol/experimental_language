//! Runtime `str` (`LangStr`): UTF-8 byte strings, primitive→str conversions,
//! comparisons, substring/case ops, and `print`/`println`. Split from `lib.rs`.

use crate::gc;
use std::io::Write;

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
pub unsafe extern "C" fn lang_str_substring(s: *const LangStr, start: i64, end: i64) -> *const LangStr {
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

fn write_str(s: *const LangStr, newline: bool) {
    let bytes = unsafe { str_bytes(s) };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    if newline {
        let _ = lock.write_all(b"\n");
    }
    let _ = lock.flush();
}

/// Write a `str` to stdout with no trailing newline.
///
/// # Safety
/// `s` must be a valid `LangStr` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_print(s: *const LangStr) {
    write_str(s, false);
}

/// Write a `str` to stdout followed by a newline.
///
/// # Safety
/// `s` must be a valid `LangStr` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_println(s: *const LangStr) {
    write_str(s, true);
}
