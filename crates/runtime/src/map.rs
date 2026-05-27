//! Runtime `Map<K,V>`: open-addressing hash table (handle + slot buffer,
//! both GC-managed; `str` or integer keys). Split from `lib.rs`.

use crate::gc;
use crate::list::*;
use crate::strings::{str_bytes, LangStr};

// --- Map<K, V> -------------------------------------------------------------
//
// An open-addressing hash table with linear probing, represented by two managed
// objects so the collector can trace and reclaim it (`docs/16`):
//
//   handle (kind=MAP, 40 B): [len][cap][buf: *managed][key_is_ptr][val_is_ptr]
//   buf    (kind=PLAIN leaf): `cap` slots of [state: u64][key: i64][val: i64]
//
// `state` is 0 = empty, 1 = occupied, 2 = tombstone. Keys are either integers
// (compared by value) or `str` pointers (compared by content); the `key_is_ptr`
// flag selects the strategy and also lets the collector trace pointer keys.
// Values are widened to `i64` by the code generator, exactly like `List<T>`.

const M_LEN: usize = 0;
const M_CAP: usize = 8;
const M_BUF: usize = 16;
const M_KEYPTR: usize = 24;
const M_VALPTR: usize = 32;
/// Bytes per slot: `[state][key][val]`.
const SLOT: usize = 24;

#[inline]
unsafe fn slot_ptr(buf: *mut u8, i: usize) -> *mut u8 {
    unsafe { buf.add(i * SLOT) }
}
#[inline]
unsafe fn slot_state(buf: *mut u8, i: usize) -> u64 {
    unsafe { (slot_ptr(buf, i) as *const u64).read() }
}

/// Hash a key. Integer keys are mixed directly; `str` keys hash their bytes
/// (FNV-1a), so structurally-equal strings collide into the same bucket.
unsafe fn map_hash(key_is_ptr: bool, key: i64) -> u64 {
    if key_is_ptr {
        let bytes = unsafe { str_bytes(key as *const LangStr) };
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    } else {
        // Mix the integer bits (splitmix64 finalizer) to avoid clustering.
        let mut z = (key as u64).wrapping_add(0x9e3779b97f4a7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

unsafe fn map_key_eq(key_is_ptr: bool, a: i64, b: i64) -> bool {
    if key_is_ptr {
        let (x, y) = unsafe { (str_bytes(a as *const LangStr), str_bytes(b as *const LangStr)) };
        x == y
    } else {
        a == b
    }
}

/// Locate `key`'s slot index. Returns `(index, found)`: when `found`, `index`
/// is the occupied slot; otherwise `index` is the first free slot to insert at
/// (reusing the earliest tombstone seen). Assumes a non-empty buffer.
unsafe fn map_probe(h: *mut u8, key: i64) -> (usize, bool) {
    let cap = unsafe { lfield(h, M_CAP) } as usize;
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    let key_is_ptr = unsafe { lfield(h, M_KEYPTR) } != 0;
    let mask = cap - 1; // cap is always a power of two
    let mut i = (unsafe { map_hash(key_is_ptr, key) } as usize) & mask;
    let mut first_tomb: Option<usize> = None;
    for _ in 0..cap {
        match unsafe { slot_state(buf, i) } {
            0 => return (first_tomb.unwrap_or(i), false),
            2 => {
                if first_tomb.is_none() {
                    first_tomb = Some(i);
                }
            }
            _ => {
                let k = unsafe { (slot_ptr(buf, i).add(8) as *const i64).read() };
                if unsafe { map_key_eq(key_is_ptr, k, key) } {
                    return (i, true);
                }
            }
        }
        i = (i + 1) & mask;
    }
    (first_tomb.unwrap_or(0), false)
}

/// Grow (or initially allocate) the slot buffer to `new_cap` and rehash every
/// occupied entry. `new_cap` must be a power of two. A single managed alloc is
/// performed while the handle (rooted by the caller) keeps the old buffer live.
unsafe fn map_resize(h: *mut u8, new_cap: usize) {
    let old_cap = unsafe { lfield(h, M_CAP) } as usize;
    let old_buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    let new_buf = unsafe { gc::alloc_var(gc::map_buf_desc(), new_cap * SLOT) };
    unsafe {
        lset(h, M_BUF, new_buf as u64);
        lset(h, M_CAP, new_cap as u64);
    }
    if old_buf.is_null() {
        return;
    }
    for i in 0..old_cap {
        if unsafe { slot_state(old_buf, i) } != 1 {
            continue;
        }
        let sp = unsafe { slot_ptr(old_buf, i) };
        let k = unsafe { (sp.add(8) as *const i64).read() };
        let v = unsafe { (sp.add(16) as *const i64).read() };
        // Reinsert: re-probe in the fresh buffer (no tombstones present).
        let (j, _) = unsafe { map_probe(h, k) };
        let dp = unsafe { slot_ptr(new_buf, j) };
        unsafe {
            (dp as *mut u64).write(1);
            (dp.add(8) as *mut i64).write(k);
            (dp.add(16) as *mut i64).write(v);
        }
    }
}

/// Create an empty map. `key_is_ptr`/`val_is_ptr` are 1 if the key/value type is
/// a managed pointer (so the collector traces them), else 0.
///
/// # Safety
/// Safe to call; uses the global managed heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_new(key_is_ptr: i64, val_is_ptr: i64) -> *mut u8 {
    let h = unsafe { gc::alloc(gc::map_handle_desc()) }; // zeroed
    unsafe {
        lset(h, M_KEYPTR, key_is_ptr as u64);
        lset(h, M_VALPTR, val_is_ptr as u64);
    }
    h
}

/// Clone a map into a fresh handle + slot buffer (`docs/15` §8). Slots are
/// copied verbatim, preserving the hash layout; emitted only for immutable
/// key/value types, so sharing them is observationally a deep copy. The copied
/// `key_is_ptr`/`val_is_ptr` flags let the collector trace the new buffer.
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_clone(h: *mut u8) -> *mut u8 {
    gc::pause();
    let new = unsafe { gc::alloc(gc::map_handle_desc()) };
    let len = unsafe { lfield(h, M_LEN) };
    let cap = unsafe { lfield(h, M_CAP) } as usize;
    unsafe {
        lset(new, M_LEN, len);
        lset(new, M_CAP, cap as u64);
        lset(new, M_KEYPTR, lfield(h, M_KEYPTR));
        lset(new, M_VALPTR, lfield(h, M_VALPTR));
    }
    let old_buf = unsafe { lfield(h, M_BUF) } as *const u8;
    if cap > 0 && !old_buf.is_null() {
        let new_buf = unsafe { gc::alloc_var(gc::map_buf_desc(), cap * SLOT) };
        unsafe { std::ptr::copy_nonoverlapping(old_buf, new_buf, cap * SLOT) };
        unsafe { lset(new, M_BUF, new_buf as u64) };
    }
    gc::resume();
    new
}

/// Insert or replace `key -> val`.
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_set(h: *mut u8, key: i64, val: i64) {
    // `key`/`val` may be unrooted managed pointers (passed by value); pause
    // collection so the resize allocation cannot free them before they are
    // stored into the (rooted) map.
    gc::pause();
    let cap = unsafe { lfield(h, M_CAP) } as usize;
    let len = unsafe { lfield(h, M_LEN) } as usize;
    // Grow when load factor would exceed 3/4 (or on first insert).
    if cap == 0 || (len + 1) * 4 > cap * 3 {
        let new_cap = if cap == 0 { 8 } else { cap * 2 };
        unsafe { map_resize(h, new_cap) };
    }
    let (i, found) = unsafe { map_probe(h, key) };
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    let sp = unsafe { slot_ptr(buf, i) };
    unsafe {
        (sp as *mut u64).write(1);
        (sp.add(8) as *mut i64).write(key);
        (sp.add(16) as *mut i64).write(val);
    }
    if !found {
        unsafe { lset(h, M_LEN, (len + 1) as u64) };
    }
    gc::resume();
}

/// The value bound to `key`, or 0 if absent (callers gate on `lang_map_contains`).
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_get(h: *mut u8, key: i64) -> i64 {
    if unsafe { lfield(h, M_CAP) } == 0 {
        return 0;
    }
    let (i, found) = unsafe { map_probe(h, key) };
    if !found {
        return 0;
    }
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    unsafe { (slot_ptr(buf, i).add(16) as *const i64).read() }
}

/// Indexed read `map[key]`; panics if the key is absent (`docs/18` §6 — `[]`
/// panics, `.get` is the fallible form).
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_index(h: *mut u8, key: i64) -> i64 {
    if unsafe { lfield(h, M_CAP) } != 0 {
        let (i, found) = unsafe { map_probe(h, key) };
        if found {
            let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
            return unsafe { (slot_ptr(buf, i).add(16) as *const i64).read() };
        }
    }
    eprintln!("panic: map key not found");
    std::process::exit(101);
}

/// Whether `key` is present.
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_contains(h: *mut u8, key: i64) -> i64 {
    if unsafe { lfield(h, M_CAP) } == 0 {
        return 0;
    }
    let (_, found) = unsafe { map_probe(h, key) };
    found as i64
}

/// Remove `key` if present (leaving a tombstone). The value is read separately
/// by the caller before removal (`remove` returns `V | null`).
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_remove(h: *mut u8, key: i64) {
    if unsafe { lfield(h, M_CAP) } == 0 {
        return;
    }
    let (i, found) = unsafe { map_probe(h, key) };
    if !found {
        return;
    }
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    unsafe { (slot_ptr(buf, i) as *mut u64).write(2) }; // tombstone
    let len = unsafe { lfield(h, M_LEN) } as usize;
    unsafe { lset(h, M_LEN, (len - 1) as u64) };
}

/// The number of entries.
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_size(h: *mut u8) -> i64 {
    unsafe { lfield(h, M_LEN) as i64 }
}

/// Remove all entries (keeps the allocated buffer).
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_clear(h: *mut u8) {
    let cap = unsafe { lfield(h, M_CAP) } as usize;
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    for i in 0..cap {
        unsafe { (slot_ptr(buf, i) as *mut u64).write(0) };
    }
    unsafe { lset(h, M_LEN, 0) };
}

/// Snapshot the keys (`want_keys == 1`) or values into a fresh `List`, in probe
/// order. Collection is paused for the duration: the result list and its buffer
/// are unrooted intermediates until returned.
///
/// # Safety
/// `h` must be a valid map handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_entries(h: *mut u8, want_keys: i64) -> *mut u8 {
    gc::pause();
    let cap = unsafe { lfield(h, M_CAP) } as usize;
    let buf = unsafe { lfield(h, M_BUF) } as *mut u8;
    let elem_is_ptr = if want_keys != 0 {
        unsafe { lfield(h, M_KEYPTR) }
    } else {
        unsafe { lfield(h, M_VALPTR) }
    };
    let list = unsafe { lang_list_new(elem_is_ptr as i64) };
    for i in 0..cap {
        if unsafe { slot_state(buf, i) } != 1 {
            continue;
        }
        let off = if want_keys != 0 { 8 } else { 16 };
        let v = unsafe { (slot_ptr(buf, i).add(off) as *const i64).read() };
        unsafe { lang_list_push(list, v) };
    }
    gc::resume();
    list
}

/// Copy every entry of `src` into `dst` (used for map-literal `..spread`).
/// `dst` is rooted by the caller throughout, so no pause is needed.
///
/// # Safety
/// `dst` and `src` must be valid map handles with matching key/value kinds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_map_extend(dst: *mut u8, src: *mut u8) {
    let cap = unsafe { lfield(src, M_CAP) } as usize;
    let buf = unsafe { lfield(src, M_BUF) } as *mut u8;
    if buf.is_null() {
        return;
    }
    for i in 0..cap {
        if unsafe { slot_state(buf, i) } != 1 {
            continue;
        }
        let sp = unsafe { slot_ptr(buf, i) };
        let k = unsafe { (sp.add(8) as *const i64).read() };
        let v = unsafe { (sp.add(16) as *const i64).read() };
        unsafe { lang_map_set(dst, k, v) };
    }
}
