//! Runtime `List<T>`: a growable array of 8-byte slots (handle + buffer,
//! both GC-managed). Split from `lib.rs`.

use crate::gc;

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
pub(crate) unsafe fn lfield(h: *mut u8, off: usize) -> u64 {
    unsafe { (h.add(off) as *const u64).read() }
}
#[inline]
pub(crate) unsafe fn lset(h: *mut u8, off: usize, v: u64) {
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

