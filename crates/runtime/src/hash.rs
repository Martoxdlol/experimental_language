//! Runtime hashing for primitives and `str` (`docs/15` §7). Generated code
//! routes `.hash()` on a primitive/`str` receiver to one of these entry points;
//! `Map<K, V>` (Phase B) calls the same helpers when its key type is a builtin.
//!
//! Two algorithms cover every builtin key:
//! * 64-bit integer-shaped values (`i*`/`u*`/`bool`/`char`/float bits) →
//!   splitmix64 finalizer — a fast, well-mixed reversible bijection.
//! * `str` → FNV-1a over the UTF-8 bytes, so structurally-equal strings hash
//!   to the same value (the `Eq` ⇒ `Hash` contract for `str`).

use crate::strings::{str_bytes, LangStr};

/// SplitMix64 finalizer over a 64-bit value. Good avalanche, no allocation.
#[inline]
pub fn hash_u64(v: u64) -> u64 {
    let mut z = v.wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// FNV-1a over the bytes of `s`. Structurally-equal `str`s hash equally.
///
/// # Safety
/// `s` must be a valid `str` field-block pointer.
#[inline]
pub unsafe fn hash_str_bytes(s: *const LangStr) -> u64 {
    let bytes = unsafe { str_bytes(s) };
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `.hash()` on a 64-bit integer-shaped value (`i64`/`u64`/widened narrower
/// integers / `bool` / `char` / `f64` bits). The compiler widens narrower
/// primitives to `i64` before this call (see backend `gen_method_call`).
#[unsafe(no_mangle)]
pub extern "C" fn lang_hash_i64(v: i64) -> u64 {
    hash_u64(v as u64)
}

/// `.hash()` on a `str` value.
///
/// # Safety
/// `s` must be a valid `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_hash_str(s: *const LangStr) -> u64 {
    unsafe { hash_str_bytes(s) }
}

/// `.hash()` on an `f64`/`f32` value. Floats are mixed by their *bit pattern*
/// after normalizing `-0.0` to `+0.0` so the `Eq` ⇒ `Hash` contract holds for
/// `0.0 == -0.0`. NaN is intentionally *not* normalized — the spec leaves the
/// `NaN == NaN` exception to the caller (it already violates `Eq` reflexivity).
#[unsafe(no_mangle)]
pub extern "C" fn lang_hash_f64(v: f64) -> u64 {
    let bits = if v == 0.0 { 0u64 } else { v.to_bits() };
    hash_u64(bits)
}

/// `.eq()` on two 64-bit integer-shaped values. Used by `Map<K, V>` for
/// builtin integer/bool/char keys (function-pointer parity with user `eq`).
#[unsafe(no_mangle)]
pub extern "C" fn lang_eq_i64(a: i64, b: i64) -> i64 {
    (a == b) as i64
}

/// `.eq()` on two `str` values — byte-wise content equality.
///
/// # Safety
/// `a` and `b` must be valid `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_eq_str(a: *const LangStr, b: *const LangStr) -> i64 {
    let (x, y) = unsafe { (str_bytes(a), str_bytes(b)) };
    (x == y) as i64
}
