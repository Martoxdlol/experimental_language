//! The language runtime: functions compiled programs call into.
//!
//! This is the provisional, pre-GC runtime. `str` is represented as a pointer
//! to a [`LangStr`] header (length + data pointer); the data and headers are
//! heap-allocated and **currently leaked** — there is no collector yet, so the
//! best-effort-drop contract of `docs/16` is trivially (if wastefully)
//! satisfied. When the tracing GC lands, these allocations move onto the
//! managed heap with the two-word object header and these functions become
//! thin shims over the collector's allocator.
//!
//! Every entry point uses the C ABI and a `lang_` prefix so the code generator
//! can reference them by a stable symbol name (JIT) or link against them
//! (object output).

use std::io::Write;

pub mod async_rt;
pub mod channels;
pub mod gc;
pub mod shared;
pub mod threads;

/// Allocate a managed object described by `desc`, returning a pointer to its
/// field block. The two-word object header (`docs/16` §3) sits at negative
/// offsets, so field offsets are unaffected. Managed allocation is infallible
/// (aborts on OOM, `docs/16` §11).
///
/// `desc` is an inline descriptor blob (see [`gc`]); the code generator emits
/// one per managed type.
///
/// # Safety
/// `desc` must point to a valid descriptor blob that outlives all its objects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_alloc(desc: *const u8) -> *mut u8 {
    unsafe { gc::alloc(desc) }
}

/// Terminate the current thread with a panic message (`docs/14`). Not
/// catchable; on the main thread this ends the program. Prints to stderr and
/// exits with code 101 (the conventional ICE/panic code).
///
/// # Safety
/// `msg` must be a valid `LangStr` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_panic(msg: *const LangStr) -> ! {
    let bytes = unsafe { str_bytes(msg) };
    eprintln!("panic: {}", String::from_utf8_lossy(bytes));
    std::process::exit(101);
}

/// Terminate the process with an explicit exit code (`docs/24`: `exit(code):
/// never`). Returns control to the OS; no `Drop` runs.
#[unsafe(no_mangle)]
pub extern "C" fn lang_exit(code: i32) -> ! {
    std::process::exit(code);
}

/// Abort the process immediately (`docs/24`: `abort(): never`). Skips unwinding
/// and finalizers — the hard stop.
#[unsafe(no_mangle)]
pub extern "C" fn lang_abort() -> ! {
    std::process::abort();
}

// --- List<T> ---------------------------------------------------------------
//
// `List<T>` is a growable array of 8-byte slots, represented by two managed
// objects so the collector can trace and reclaim it (`docs/16`):
//
//   handle (kind=LIST, 32 B): [len: u64][cap: u64][buf: *managed][elem_is_ptr: u64]
//   buf    (kind=PLAIN leaf, cap*8 B): the element slots
//
// The code generator widens each element to an `i64` on the way in and narrows
// it back on read, so one set of intrinsics serves every element type. The
// `elem_is_ptr` flag lets the collector trace pointer-typed elements via the
// handle (the buffer alone does not know its length).

const L_LEN: usize = 0;
const L_CAP: usize = 8;
const L_BUF: usize = 16;
const L_ELEMPTR: usize = 24;

#[inline]
unsafe fn lfield(h: *mut u8, off: usize) -> u64 {
    unsafe { (h.add(off) as *const u64).read() }
}
#[inline]
unsafe fn lset(h: *mut u8, off: usize, v: u64) {
    unsafe { (h.add(off) as *mut u64).write(v) }
}

/// Create an empty list. `elem_is_ptr` is 1 if the element type is a managed
/// pointer (so the collector traces the elements), else 0.
///
/// # Safety
/// Safe to call; uses the global managed heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_new(elem_is_ptr: i64) -> *mut u8 {
    let h = unsafe { gc::alloc(gc::list_handle_desc()) }; // zeroed: len=cap=0, buf=null
    unsafe { lset(h, L_ELEMPTR, elem_is_ptr as u64) };
    h
}

/// Grow `h`'s buffer to hold at least one more element.
unsafe fn list_grow(h: *mut u8) {
    let cap = unsafe { lfield(h, L_CAP) } as usize;
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let new_cap = if cap == 0 { 4 } else { cap * 2 };
    let new_buf = unsafe { gc::alloc_var(gc::list_buf_desc(), new_cap * 8) };
    let old_buf = unsafe { lfield(h, L_BUF) } as *const u8;
    if !old_buf.is_null() && len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(old_buf, new_buf, len * 8) };
    }
    unsafe {
        lset(h, L_BUF, new_buf as u64);
        lset(h, L_CAP, new_cap as u64);
    }
}

/// Append a slot.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_push(h: *mut u8, v: i64) {
    // `v` may be an unrooted managed pointer (the caller passed it by value);
    // pause collection so the buffer-growth allocation cannot free it before it
    // is stored into the (rooted) list.
    gc::pause();
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let cap = unsafe { lfield(h, L_CAP) } as usize;
    if len == cap {
        unsafe { list_grow(h) };
    }
    let buf = unsafe { lfield(h, L_BUF) } as *mut i64;
    unsafe {
        buf.add(len).write(v);
        lset(h, L_LEN, (len + 1) as u64);
    }
    gc::resume();
}

/// The number of elements.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_size(h: *mut u8) -> i64 {
    unsafe { lfield(h, L_LEN) as i64 }
}

/// Indexed read; panics out of range (`docs/18` — `[]` panics, `.get` is the
/// fallible form).
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_get(h: *mut u8, i: i64) -> i64 {
    let len = unsafe { lfield(h, L_LEN) } as i64;
    if i < 0 || i >= len {
        eprintln!("panic: list index {i} out of range (len {len})");
        std::process::exit(101);
    }
    let buf = unsafe { lfield(h, L_BUF) } as *const i64;
    unsafe { buf.add(i as usize).read() }
}

/// Indexed write; panics out of range.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_set(h: *mut u8, i: i64, v: i64) {
    let len = unsafe { lfield(h, L_LEN) } as i64;
    if i < 0 || i >= len {
        eprintln!("panic: list index {i} out of range (len {len})");
        std::process::exit(101);
    }
    let buf = unsafe { lfield(h, L_BUF) } as *mut i64;
    unsafe { buf.add(i as usize).write(v) };
}

/// Clone a list into a fresh handle + buffer (`docs/15` §8). The element slots
/// are copied verbatim; the code generator only emits this for immutable
/// element types, so sharing the (immutable) elements is observationally a deep
/// copy. The collector traces the new buffer via the copied `elem_is_ptr` flag.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_clone(h: *mut u8) -> *mut u8 {
    // The new handle/buffer are unrooted while we build them; pause collection.
    gc::pause();
    let elem_is_ptr = unsafe { lfield(h, L_ELEMPTR) } as i64;
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let new = unsafe { lang_list_new(elem_is_ptr) };
    if len > 0 {
        let new_buf = unsafe { gc::alloc_var(gc::list_buf_desc(), len * 8) };
        let old_buf = unsafe { lfield(h, L_BUF) } as *const u8;
        unsafe { std::ptr::copy_nonoverlapping(old_buf, new_buf, len * 8) };
        unsafe {
            lset(new, L_BUF, new_buf as u64);
            lset(new, L_CAP, len as u64);
            lset(new, L_LEN, len as u64);
        }
    }
    gc::resume();
    new
}

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
