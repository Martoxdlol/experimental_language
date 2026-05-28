//! Foreign-heap allocation (`docs/19` §5). The foreign heap is opaque to the
//! tracing GC: these are thin wrappers over the system allocator (`libc`
//! `malloc`/`calloc`/`free`) with explicit, manual lifetimes. A returned
//! pointer is a raw `*T` (or `null` on failure) — the language types it as
//! `*T | null` (NPO), never as a managed value.

use crate::strings::{lang_str_from_utf8, str_bytes, LangStr};
use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};

/// The allocation-header size: we stash the `Layout` size just before the
/// returned block so `lang_foreign_free` can reconstruct it (Rust's allocator
/// requires the size at `dealloc`). Kept pointer-aligned.
const HEADER: usize = 16;

/// Allocate `size` bytes on the foreign heap, returning a raw pointer (or null
/// on failure / zero size). The block is 16-byte aligned (fits any C scalar /
/// extern struct laid out so far).
#[unsafe(no_mangle)]
pub extern "C" fn lang_foreign_alloc(size: u64) -> *mut u8 {
    foreign_alloc(size as usize, false)
}

/// As [`lang_foreign_alloc`], but the returned block is zeroed.
#[unsafe(no_mangle)]
pub extern "C" fn lang_foreign_alloc_zeroed(size: u64) -> *mut u8 {
    foreign_alloc(size as usize, true)
}

fn foreign_alloc(size: usize, zeroed: bool) -> *mut u8 {
    if size == 0 {
        return std::ptr::null_mut();
    }
    let total = size + HEADER;
    let layout = match Layout::from_size_align(total, 16) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    // SAFETY: `layout` has a non-zero size.
    let base = unsafe {
        if zeroed { alloc_zeroed(layout) } else { alloc(layout) }
    };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // Stash the total size in the header so `free` can rebuild the layout.
    unsafe {
        (base as *mut u64).write(total as u64);
        base.add(HEADER)
    }
}

/// Resize a foreign allocation to `new_size` bytes (`docs/19` §5), preserving
/// the leading `min(old, new)` bytes. A null pointer behaves like a fresh
/// `alloc`; a zero `new_size` frees and returns null. Returns null on OOM (the
/// original block is left intact).
#[unsafe(no_mangle)]
pub extern "C" fn lang_foreign_realloc(ptr: *mut u8, new_size: u64) -> *mut u8 {
    if ptr.is_null() {
        return foreign_alloc(new_size as usize, false);
    }
    if new_size == 0 {
        lang_foreign_free(ptr);
        return std::ptr::null_mut();
    }
    // SAFETY: `ptr` came from `foreign_alloc`, so its header holds the total size.
    unsafe {
        let base = ptr.sub(HEADER);
        let old_total = (base as *const u64).read() as usize;
        let old_data = old_total - HEADER;
        let nb = foreign_alloc(new_size as usize, false);
        if nb.is_null() {
            return std::ptr::null_mut(); // OOM — original left intact
        }
        let copy = old_data.min(new_size as usize);
        std::ptr::copy_nonoverlapping(ptr as *const u8, nb, copy);
        lang_foreign_free(ptr);
        nb
    }
}

/// Marshal a language `str` into a fresh, NUL-terminated C string on the
/// foreign heap (`docs/19` §6). The returned `*u8` is owned by the caller and
/// must be released with `Foreign.free`. Interior NUL bytes are copied verbatim
/// (C will see a truncated string — the caller's responsibility).
///
/// # Safety
/// `s` must be a valid managed `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_cstring_from_str(s: *const LangStr) -> *mut u8 {
    let bytes = unsafe { str_bytes(s) };
    let buf = foreign_alloc(bytes.len() + 1, false);
    if buf.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
        buf.add(bytes.len()).write(0); // NUL terminator
    }
    buf
}

/// Copy a NUL-terminated C string into a managed language `str` (`docs/19` §6).
/// The bytes are assumed to be valid UTF-8 (the source vouches). A null pointer
/// yields the empty string.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_cstr_to_str(p: *const u8) -> *const LangStr {
    if p.is_null() {
        return unsafe { lang_str_from_utf8(p, 0) };
    }
    // C `strlen`.
    let mut len = 0usize;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
        lang_str_from_utf8(p, len)
    }
}

/// Free a block returned by [`lang_foreign_alloc`] / `_zeroed`. A null pointer
/// is a no-op (matching C `free(NULL)`).
#[unsafe(no_mangle)]
pub extern "C" fn lang_foreign_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` was produced by `foreign_alloc`, so the header sits `HEADER`
    // bytes before it and holds the original total size.
    unsafe {
        let base = ptr.sub(HEADER);
        let total = (base as *const u64).read() as usize;
        if let Ok(layout) = Layout::from_size_align(total, 16) {
            dealloc(base, layout);
        }
    }
}
