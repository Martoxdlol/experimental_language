//! Runtime `List<T>`: a growable array of 8-byte slots (handle + buffer,
//! both GC-managed). Split from `lib.rs`.

use crate::gc;

// --- List<T> ---------------------------------------------------------------
//
// `List<T>` is a growable array of 8-byte slots, represented by two managed
// objects so the collector can trace and reclaim it (`docs/16`):
//
//   handle (kind=LIST, 40 B):
//     [len: u64][cap: u64][buf: *managed][elem_is_ptr: u64][endpoint_kind: u64]
//   buf    (kind=PLAIN leaf, cap*8 B): the element slots
//
// The code generator widens each element to an `i64` on the way in and narrows
// it back on read, so one set of intrinsics serves every element type. The
// `elem_is_ptr` flag lets the collector trace pointer-typed elements via the
// handle (the buffer alone does not know its length). `endpoint_kind` is a
// runtime backstop for `List<Sender<T>>`/`List<Receiver<T>>`: live operations
// emit deterministic acquire/release calls in codegen, and GC release uses this
// metadata when a whole list becomes unreachable.

const L_LEN: usize = 0;
const L_CAP: usize = 8;
const L_BUF: usize = 16;
const L_ELEMPTR: usize = 24;
const L_ENDPOINT_KIND: usize = 32;

const ENDPOINT_NONE: u64 = 0;
const ENDPOINT_SENDER: u64 = 1;
const ENDPOINT_RECEIVER: u64 = 2;

#[inline]
pub(crate) unsafe fn lfield(h: *mut u8, off: usize) -> u64 {
    unsafe { (h.add(off) as *const u64).read() }
}
#[inline]
pub(crate) unsafe fn lset(h: *mut u8, off: usize, v: u64) {
    unsafe { (h.add(off) as *mut u64).write(v) }
}

/// Create an empty list. `elem_is_ptr` is 1 if the element type is a managed
/// pointer (so the collector traces the elements), else 0. `endpoint_kind` is
/// 0 for ordinary lists, 1 for `Sender<T>` elements, and 2 for `Receiver<T>`.
///
/// # Safety
/// Safe to call; uses the global managed heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_new(elem_is_ptr: i64, endpoint_kind: i64) -> *mut u8 {
    let h = unsafe { gc::alloc(gc::list_handle_desc()) }; // zeroed: len=cap=0, buf=null
    unsafe {
        lset(h, L_ELEMPTR, elem_is_ptr as u64);
        lset(h, L_ENDPOINT_KIND, endpoint_kind as u64);
    }
    h
}

#[inline]
unsafe fn endpoint_kind(h: *mut u8) -> u64 {
    match unsafe { lfield(h, L_ENDPOINT_KIND) } {
        ENDPOINT_SENDER => ENDPOINT_SENDER,
        ENDPOINT_RECEIVER => ENDPOINT_RECEIVER,
        _ => ENDPOINT_NONE,
    }
}

#[inline]
unsafe fn endpoint_channel_id(raw: i64) -> Option<u64> {
    if raw == 0 {
        None
    } else {
        Some(unsafe { (raw as *const u64).read() })
    }
}

unsafe fn acquire_endpoint(kind: u64, raw: i64) {
    if kind == ENDPOINT_NONE {
        return;
    }
    let Some(id) = (unsafe { endpoint_channel_id(raw) }) else {
        return;
    };
    match kind {
        ENDPOINT_SENDER => unsafe { crate::channels::lang_chan_sender_acquire(id) },
        ENDPOINT_RECEIVER => unsafe { crate::channels::lang_chan_receiver_acquire(id) },
        _ => {}
    }
}

/// Collect the endpoint releases owned by a list handle about to be reclaimed.
/// The GC calls the returned channel hooks after dropping the heap lock.
pub(crate) unsafe fn endpoint_releases_for_list(h: *mut u8) -> Vec<(u64, u64)> {
    let kind = unsafe { endpoint_kind(h) };
    if kind == ENDPOINT_NONE {
        return Vec::new();
    }
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let buf = unsafe { lfield(h, L_BUF) } as *const i64;
    if buf.is_null() || len == 0 {
        return Vec::new();
    }
    let mut releases = Vec::with_capacity(len);
    for i in 0..len {
        let raw = unsafe { buf.add(i).read() };
        if let Some(id) = unsafe { endpoint_channel_id(raw) } {
            releases.push((kind, id));
        }
    }
    releases
}

pub(crate) fn release_endpoint_refs(releases: Vec<(u64, u64)>) {
    for (kind, id) in releases {
        match kind {
            ENDPOINT_SENDER => unsafe { crate::channels::lang_chan_sender_release(id) },
            ENDPOINT_RECEIVER => unsafe { crate::channels::lang_chan_receiver_release(id) },
            _ => {}
        }
    }
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

/// `xs.clear()` (`docs/18`): drop all elements (length → 0; capacity kept).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_clear(h: *mut u8) {
    unsafe { lset(h, L_LEN, 0) };
}

/// Build a fresh `List<T>` from the half-open slice `[start, end)` of `h`
/// (clamped to the list's bounds). Used to bind the `..tail` of a list pattern.
/// Built under a GC pause — the new list is not yet rooted.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_slice(h: *mut u8, start: i64, end: i64) -> *mut u8 {
    let len = unsafe { lfield(h, L_LEN) } as i64;
    let lo = start.clamp(0, len);
    let hi = end.clamp(lo, len);
    let elem_is_ptr = unsafe { lfield(h, L_ELEMPTR) } as i64;
    let endpoint_kind = unsafe { lfield(h, L_ENDPOINT_KIND) } as i64;
    gc::pause();
    let out = unsafe { lang_list_new(elem_is_ptr, endpoint_kind) };
    let mut i = lo;
    while i < hi {
        let buf = unsafe { lfield(h, L_BUF) } as *const i64;
        let v = unsafe { buf.add(i as usize).read() };
        unsafe { acquire_endpoint(endpoint_kind as u64, v) };
        unsafe { lang_list_push(out, v) };
        i += 1;
    }
    gc::resume_with_return_root(out as usize);
    out
}

/// `xs.truncate(n)` (`docs/18`): shorten the list to at most `n` elements
/// (no-op if `n >= size`; capacity kept). `n < 0` is treated as `0`.
///
/// # Safety
/// `h` must be a valid list handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_truncate(h: *mut u8, n: i64) {
    let len = unsafe { lfield(h, L_LEN) } as i64;
    if n < 0 {
        unsafe { lset(h, L_LEN, 0) };
    } else if n < len {
        unsafe { lset(h, L_LEN, n as u64) };
    }
}

/// Remove the last element, decrement the length, and return its raw slot.
/// The caller (codegen) guards `len > 0` before calling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_pop(h: *mut u8) -> i64 {
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let buf = unsafe { lfield(h, L_BUF) } as *const i64;
    let v = unsafe { buf.add(len - 1).read() };
    unsafe { lset(h, L_LEN, (len - 1) as u64) };
    v
}

/// `xs.insert(i, v)` (`docs/18`): shift `[i..len]` right and insert `v` at `i`.
/// Panics if `i > len`. `v` may be an unrooted managed pointer, so pause GC
/// across the (possibly growing) shift.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_insert(h: *mut u8, i: i64, v: i64) {
    gc::pause();
    let len = unsafe { lfield(h, L_LEN) } as usize;
    if i < 0 || i as usize > len {
        eprintln!("panic: list insert index {i} out of range (len {len})");
        std::process::exit(101);
    }
    let cap = unsafe { lfield(h, L_CAP) } as usize;
    if len == cap {
        unsafe { list_grow(h) };
    }
    let buf = unsafe { lfield(h, L_BUF) } as *mut i64;
    let idx = i as usize;
    unsafe {
        // Shift the tail right by one (overlapping copy, moving backwards).
        std::ptr::copy(buf.add(idx), buf.add(idx + 1), len - idx);
        buf.add(idx).write(v);
        lset(h, L_LEN, (len + 1) as u64);
    }
    gc::resume();
}

/// Remove and return the element at `i`, shifting `[i+1..len]` left. The caller
/// (codegen) guards `0 <= i < len`. No allocation, so no GC pause is needed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_list_remove(h: *mut u8, i: i64) -> i64 {
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let buf = unsafe { lfield(h, L_BUF) } as *mut i64;
    let idx = i as usize;
    unsafe {
        let v = buf.add(idx).read();
        std::ptr::copy(buf.add(idx + 1), buf.add(idx), len - idx - 1);
        lset(h, L_LEN, (len - 1) as u64);
        v
    }
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
    let endpoint_kind = unsafe { lfield(h, L_ENDPOINT_KIND) } as i64;
    let len = unsafe { lfield(h, L_LEN) } as usize;
    let new = unsafe { lang_list_new(elem_is_ptr, endpoint_kind) };
    if len > 0 {
        let new_buf = unsafe { gc::alloc_var(gc::list_buf_desc(), len * 8) };
        let old_buf = unsafe { lfield(h, L_BUF) } as *const u8;
        unsafe { std::ptr::copy_nonoverlapping(old_buf, new_buf, len * 8) };
        if endpoint_kind != ENDPOINT_NONE as i64 {
            let old_slots = old_buf as *const i64;
            for i in 0..len {
                let raw = unsafe { old_slots.add(i).read() };
                unsafe { acquire_endpoint(endpoint_kind as u64, raw) };
            }
        }
        unsafe {
            lset(new, L_BUF, new_buf as u64);
            lset(new, L_CAP, len as u64);
            lset(new, L_LEN, len as u64);
        }
    }
    gc::resume_with_return_root(new as usize);
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENDPOINT_DESC: gc::StaticDesc = gc::StaticDesc {
        size: 8,
        kind: gc::KIND_PLAIN,
        type_id: 0,
        n_ptrs: 0,
        rc_trailer: 0,
    };

    unsafe fn sender_handle(id: u64) -> *mut u8 {
        let p = unsafe { gc::alloc(&ENDPOINT_DESC as *const gc::StaticDesc as *const u8) };
        unsafe { (p as *mut u64).write(id) };
        p
    }

    #[test]
    fn gc_releases_sender_endpoints_owned_by_dead_list() {
        let _guard = gc::TEST_LOCK.lock().unwrap();
        unsafe { gc::free_all() };

        let id = crate::channels::lang_channel_new();
        let sender = unsafe { sender_handle(id) };
        let list = unsafe { lang_list_new(1, 1) };

        unsafe { crate::channels::lang_chan_sender_acquire(id) };
        unsafe { lang_list_push(list, sender as i64) };
        unsafe { crate::channels::lang_chan_sender_release(id) };

        let freed = unsafe { gc::collect(&[]) };
        assert!(freed > 0, "unrooted list should be reclaimed");

        let mut got = -1;
        let _ = unsafe { crate::channels::chan_recv_native_wait_for_runtime_tests(id, &mut got) };
        assert_eq!(got, 0, "dead list released its last sender endpoint");

        unsafe { crate::channels::lang_chan_receiver_release(id) };
        unsafe { gc::free_all() };
    }
}
