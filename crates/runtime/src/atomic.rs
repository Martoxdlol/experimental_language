//! Runtime atomic primitives for `core:sync/atomic`.
//!
//! The public surface is authored in `stdlib_src/core/sync_atomic.otter`.
//! Handles own a boxed native atomic value and are released by the
//! `@RefCounted` wrapper's deterministic `Drop`.

use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering,
};

fn ordering(code: i64) -> Ordering {
    match code {
        0 => Ordering::Relaxed,
        1 => Ordering::Acquire,
        2 => Ordering::Release,
        3 => Ordering::AcqRel,
        4 => Ordering::SeqCst,
        _ => Ordering::SeqCst,
    }
}

fn bool_value(value: i64) -> bool {
    value != 0
}

fn bool_code(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

/// Allocate a native atomic i64 handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_i64_new(value: i64) -> *mut u8 {
    Box::into_raw(Box::new(AtomicI64::new(value))) as *mut u8
}

/// Free a native atomic i64 handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_i64_new`] that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicI64) });
    }
}

/// Load the atomic value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_load(ptr: *mut u8, ord: i64) -> i64 {
    unsafe { &*(ptr as *const AtomicI64) }.load(ordering(ord))
}

/// Store a new atomic value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_store(ptr: *mut u8, value: i64, ord: i64) {
    unsafe { &*(ptr as *const AtomicI64) }.store(value, ordering(ord));
}

/// Swap the atomic value, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_swap(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    unsafe { &*(ptr as *const AtomicI64) }.swap(value, ordering(ord))
}

/// Compare and exchange, returning the observed previous value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_compare_exchange(
    ptr: *mut u8,
    expected: i64,
    new: i64,
    success: i64,
    failure: i64,
) -> i64 {
    let atomic = unsafe { &*(ptr as *const AtomicI64) };
    match atomic.compare_exchange(expected, new, ordering(success), ordering(failure)) {
        Ok(old) | Err(old) => old,
    }
}

/// Add with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_fetch_add(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    unsafe { &*(ptr as *const AtomicI64) }.fetch_add(value, ordering(ord))
}

/// Subtract with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i64_fetch_sub(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    unsafe { &*(ptr as *const AtomicI64) }.fetch_sub(value, ordering(ord))
}

/// Allocate a native atomic i32 handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_i32_new(value: i32) -> *mut u8 {
    Box::into_raw(Box::new(AtomicI32::new(value))) as *mut u8
}

/// Free a native atomic i32 handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_i32_new`] that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicI32) });
    }
}

/// Load the atomic value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_load(ptr: *mut u8, ord: i64) -> i32 {
    unsafe { &*(ptr as *const AtomicI32) }.load(ordering(ord))
}

/// Store a new atomic value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_store(ptr: *mut u8, value: i32, ord: i64) {
    unsafe { &*(ptr as *const AtomicI32) }.store(value, ordering(ord));
}

/// Swap the atomic value, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_swap(ptr: *mut u8, value: i32, ord: i64) -> i32 {
    unsafe { &*(ptr as *const AtomicI32) }.swap(value, ordering(ord))
}

/// Compare and exchange, returning the observed previous value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_compare_exchange(
    ptr: *mut u8,
    expected: i32,
    new: i32,
    success: i64,
    failure: i64,
) -> i32 {
    let atomic = unsafe { &*(ptr as *const AtomicI32) };
    match atomic.compare_exchange(expected, new, ordering(success), ordering(failure)) {
        Ok(old) | Err(old) => old,
    }
}

/// Add with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_fetch_add(ptr: *mut u8, value: i32, ord: i64) -> i32 {
    unsafe { &*(ptr as *const AtomicI32) }.fetch_add(value, ordering(ord))
}

/// Subtract with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic i32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_i32_fetch_sub(ptr: *mut u8, value: i32, ord: i64) -> i32 {
    unsafe { &*(ptr as *const AtomicI32) }.fetch_sub(value, ordering(ord))
}

/// Allocate a native atomic u64 handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_u64_new(value: u64) -> *mut u8 {
    Box::into_raw(Box::new(AtomicU64::new(value))) as *mut u8
}

/// Free a native atomic u64 handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_u64_new`] that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicU64) });
    }
}

/// Load the atomic value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_load(ptr: *mut u8, ord: i64) -> u64 {
    unsafe { &*(ptr as *const AtomicU64) }.load(ordering(ord))
}

/// Store a new atomic value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_store(ptr: *mut u8, value: u64, ord: i64) {
    unsafe { &*(ptr as *const AtomicU64) }.store(value, ordering(ord));
}

/// Swap the atomic value, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_swap(ptr: *mut u8, value: u64, ord: i64) -> u64 {
    unsafe { &*(ptr as *const AtomicU64) }.swap(value, ordering(ord))
}

/// Compare and exchange, returning the observed previous value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_compare_exchange(
    ptr: *mut u8,
    expected: u64,
    new: u64,
    success: i64,
    failure: i64,
) -> u64 {
    let atomic = unsafe { &*(ptr as *const AtomicU64) };
    match atomic.compare_exchange(expected, new, ordering(success), ordering(failure)) {
        Ok(old) | Err(old) => old,
    }
}

/// Add with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_fetch_add(ptr: *mut u8, value: u64, ord: i64) -> u64 {
    unsafe { &*(ptr as *const AtomicU64) }.fetch_add(value, ordering(ord))
}

/// Subtract with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u64 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u64_fetch_sub(ptr: *mut u8, value: u64, ord: i64) -> u64 {
    unsafe { &*(ptr as *const AtomicU64) }.fetch_sub(value, ordering(ord))
}

/// Allocate a native atomic u32 handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_u32_new(value: u32) -> *mut u8 {
    Box::into_raw(Box::new(AtomicU32::new(value))) as *mut u8
}

/// Free a native atomic u32 handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_u32_new`] that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicU32) });
    }
}

/// Load the atomic value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_load(ptr: *mut u8, ord: i64) -> u32 {
    unsafe { &*(ptr as *const AtomicU32) }.load(ordering(ord))
}

/// Store a new atomic value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_store(ptr: *mut u8, value: u32, ord: i64) {
    unsafe { &*(ptr as *const AtomicU32) }.store(value, ordering(ord));
}

/// Swap the atomic value, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_swap(ptr: *mut u8, value: u32, ord: i64) -> u32 {
    unsafe { &*(ptr as *const AtomicU32) }.swap(value, ordering(ord))
}

/// Compare and exchange, returning the observed previous value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_compare_exchange(
    ptr: *mut u8,
    expected: u32,
    new: u32,
    success: i64,
    failure: i64,
) -> u32 {
    let atomic = unsafe { &*(ptr as *const AtomicU32) };
    match atomic.compare_exchange(expected, new, ordering(success), ordering(failure)) {
        Ok(old) | Err(old) => old,
    }
}

/// Add with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_fetch_add(ptr: *mut u8, value: u32, ord: i64) -> u32 {
    unsafe { &*(ptr as *const AtomicU32) }.fetch_add(value, ordering(ord))
}

/// Subtract with two's-complement wrapping semantics, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic u32 handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_u32_fetch_sub(ptr: *mut u8, value: u32, ord: i64) -> u32 {
    unsafe { &*(ptr as *const AtomicU32) }.fetch_sub(value, ordering(ord))
}

/// Allocate a native atomic pointer handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_ptr_new(value: *mut u8) -> *mut u8 {
    Box::into_raw(Box::new(AtomicPtr::<u8>::new(value))) as *mut u8
}

/// Free a native atomic pointer handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_ptr_new`] that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_ptr_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicPtr<u8>) });
    }
}

/// Load the atomic pointer value.
///
/// # Safety
/// `ptr` must be a live atomic pointer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_ptr_load(ptr: *mut u8, ord: i64) -> *mut u8 {
    unsafe { &*(ptr as *const AtomicPtr<u8>) }.load(ordering(ord))
}

/// Store a new atomic pointer value.
///
/// # Safety
/// `ptr` must be a live atomic pointer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_ptr_store(ptr: *mut u8, value: *mut u8, ord: i64) {
    unsafe { &*(ptr as *const AtomicPtr<u8>) }.store(value, ordering(ord));
}

/// Swap the atomic pointer value, returning the previous value.
///
/// # Safety
/// `ptr` must be a live atomic pointer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_ptr_swap(ptr: *mut u8, value: *mut u8, ord: i64) -> *mut u8 {
    unsafe { &*(ptr as *const AtomicPtr<u8>) }.swap(value, ordering(ord))
}

/// Compare and exchange, returning the observed previous pointer value.
///
/// # Safety
/// `ptr` must be a live atomic pointer handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_ptr_compare_exchange(
    ptr: *mut u8,
    expected: *mut u8,
    new: *mut u8,
    success: i64,
    failure: i64,
) -> *mut u8 {
    let atomic = unsafe { &*(ptr as *const AtomicPtr<u8>) };
    match atomic.compare_exchange(expected, new, ordering(success), ordering(failure)) {
        Ok(old) | Err(old) => old,
    }
}

/// Allocate a native atomic bool handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_atomic_bool_new(value: i64) -> *mut u8 {
    Box::into_raw(Box::new(AtomicBool::new(bool_value(value)))) as *mut u8
}

/// Free a native atomic bool handle.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`lang_atomic_bool_new`] that
/// has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_free(ptr: *mut u8) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr as *mut AtomicBool) });
    }
}

/// Load the atomic value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_load(ptr: *mut u8, ord: i64) -> i64 {
    bool_code(unsafe { &*(ptr as *const AtomicBool) }.load(ordering(ord)))
}

/// Store a new atomic bool value.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_store(ptr: *mut u8, value: i64, ord: i64) {
    unsafe { &*(ptr as *const AtomicBool) }.store(bool_value(value), ordering(ord));
}

/// Swap the atomic value, returning the previous value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_swap(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    bool_code(unsafe { &*(ptr as *const AtomicBool) }.swap(bool_value(value), ordering(ord)))
}

/// Compare and exchange, returning the observed previous value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_compare_exchange(
    ptr: *mut u8,
    expected: i64,
    new: i64,
    success: i64,
    failure: i64,
) -> i64 {
    let atomic = unsafe { &*(ptr as *const AtomicBool) };
    let observed = match atomic.compare_exchange(
        bool_value(expected),
        bool_value(new),
        ordering(success),
        ordering(failure),
    ) {
        Ok(old) | Err(old) => old,
    };
    bool_code(observed)
}

/// Bitwise-and the atomic value, returning the previous value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_fetch_and(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    bool_code(unsafe { &*(ptr as *const AtomicBool) }.fetch_and(bool_value(value), ordering(ord)))
}

/// Bitwise-or the atomic value, returning the previous value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_fetch_or(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    bool_code(unsafe { &*(ptr as *const AtomicBool) }.fetch_or(bool_value(value), ordering(ord)))
}

/// Bitwise-xor the atomic value, returning the previous value as `0`/`1`.
///
/// # Safety
/// `ptr` must be a live atomic bool handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_atomic_bool_fetch_xor(ptr: *mut u8, value: i64, ord: i64) -> i64 {
    bool_code(unsafe { &*(ptr as *const AtomicBool) }.fetch_xor(bool_value(value), ordering(ord)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_i64_operations_follow_native_contract() {
        let ptr = lang_atomic_i64_new(10);
        assert_eq!(unsafe { lang_atomic_i64_load(ptr, 4) }, 10);
        unsafe { lang_atomic_i64_store(ptr, 11, 4) };
        assert_eq!(unsafe { lang_atomic_i64_swap(ptr, 12, 4) }, 11);
        assert_eq!(unsafe { lang_atomic_i64_fetch_add(ptr, 5, 4) }, 12);
        assert_eq!(unsafe { lang_atomic_i64_fetch_sub(ptr, 2, 4) }, 17);
        assert_eq!(
            unsafe { lang_atomic_i64_compare_exchange(ptr, 15, 99, 4, 4) },
            15
        );
        assert_eq!(unsafe { lang_atomic_i64_load(ptr, 4) }, 99);
        assert_eq!(
            unsafe { lang_atomic_i64_compare_exchange(ptr, 15, 123, 4, 4) },
            99
        );
        assert_eq!(unsafe { lang_atomic_i64_load(ptr, 4) }, 99);
        unsafe { lang_atomic_i64_free(ptr) };
    }

    #[test]
    fn atomic_bool_operations_follow_native_contract() {
        let ptr = lang_atomic_bool_new(1);
        assert_eq!(unsafe { lang_atomic_bool_load(ptr, 4) }, 1);
        unsafe { lang_atomic_bool_store(ptr, 0, 4) };
        assert_eq!(unsafe { lang_atomic_bool_swap(ptr, 1, 4) }, 0);
        assert_eq!(
            unsafe { lang_atomic_bool_compare_exchange(ptr, 1, 0, 4, 4) },
            1
        );
        assert_eq!(unsafe { lang_atomic_bool_load(ptr, 4) }, 0);
        assert_eq!(
            unsafe { lang_atomic_bool_compare_exchange(ptr, 1, 1, 4, 4) },
            0
        );
        unsafe { lang_atomic_bool_store(ptr, 1, 4) };
        assert_eq!(unsafe { lang_atomic_bool_fetch_and(ptr, 0, 4) }, 1);
        assert_eq!(unsafe { lang_atomic_bool_load(ptr, 4) }, 0);
        assert_eq!(unsafe { lang_atomic_bool_fetch_or(ptr, 1, 4) }, 0);
        assert_eq!(unsafe { lang_atomic_bool_fetch_xor(ptr, 1, 4) }, 1);
        assert_eq!(unsafe { lang_atomic_bool_load(ptr, 4) }, 0);
        unsafe { lang_atomic_bool_free(ptr) };
    }

    #[test]
    fn atomic_i32_operations_follow_native_contract() {
        let ptr = lang_atomic_i32_new(-10);
        assert_eq!(unsafe { lang_atomic_i32_load(ptr, 4) }, -10);
        unsafe { lang_atomic_i32_store(ptr, 11, 4) };
        assert_eq!(unsafe { lang_atomic_i32_swap(ptr, 12, 4) }, 11);
        assert_eq!(unsafe { lang_atomic_i32_fetch_add(ptr, 5, 4) }, 12);
        assert_eq!(unsafe { lang_atomic_i32_fetch_sub(ptr, 2, 4) }, 17);
        assert_eq!(
            unsafe { lang_atomic_i32_compare_exchange(ptr, 15, -99, 4, 4) },
            15
        );
        assert_eq!(unsafe { lang_atomic_i32_load(ptr, 4) }, -99);
        assert_eq!(
            unsafe { lang_atomic_i32_compare_exchange(ptr, 15, 123, 4, 4) },
            -99
        );
        assert_eq!(unsafe { lang_atomic_i32_load(ptr, 4) }, -99);
        unsafe { lang_atomic_i32_free(ptr) };
    }

    #[test]
    fn atomic_u64_operations_follow_native_contract() {
        let ptr = lang_atomic_u64_new(10);
        assert_eq!(unsafe { lang_atomic_u64_load(ptr, 4) }, 10);
        unsafe { lang_atomic_u64_store(ptr, 11, 4) };
        assert_eq!(unsafe { lang_atomic_u64_swap(ptr, 12, 4) }, 11);
        assert_eq!(unsafe { lang_atomic_u64_fetch_add(ptr, 5, 4) }, 12);
        assert_eq!(unsafe { lang_atomic_u64_fetch_sub(ptr, 2, 4) }, 17);
        assert_eq!(
            unsafe { lang_atomic_u64_compare_exchange(ptr, 15, 99, 4, 4) },
            15
        );
        assert_eq!(unsafe { lang_atomic_u64_load(ptr, 4) }, 99);
        assert_eq!(
            unsafe { lang_atomic_u64_compare_exchange(ptr, 15, 123, 4, 4) },
            99
        );
        assert_eq!(unsafe { lang_atomic_u64_load(ptr, 4) }, 99);
        unsafe { lang_atomic_u64_free(ptr) };
    }

    #[test]
    fn atomic_u32_operations_follow_native_contract() {
        let ptr = lang_atomic_u32_new(10);
        assert_eq!(unsafe { lang_atomic_u32_load(ptr, 4) }, 10);
        unsafe { lang_atomic_u32_store(ptr, 11, 4) };
        assert_eq!(unsafe { lang_atomic_u32_swap(ptr, 12, 4) }, 11);
        assert_eq!(unsafe { lang_atomic_u32_fetch_add(ptr, 5, 4) }, 12);
        assert_eq!(unsafe { lang_atomic_u32_fetch_sub(ptr, 2, 4) }, 17);
        assert_eq!(
            unsafe { lang_atomic_u32_compare_exchange(ptr, 15, 99, 4, 4) },
            15
        );
        assert_eq!(unsafe { lang_atomic_u32_load(ptr, 4) }, 99);
        assert_eq!(
            unsafe { lang_atomic_u32_compare_exchange(ptr, 15, 123, 4, 4) },
            99
        );
        assert_eq!(unsafe { lang_atomic_u32_load(ptr, 4) }, 99);
        unsafe { lang_atomic_u32_free(ptr) };
    }

    #[test]
    fn atomic_ptr_operations_follow_native_contract() {
        let mut a = 10u8;
        let mut b = 20u8;
        let mut c = 30u8;
        let pa = &mut a as *mut u8;
        let pb = &mut b as *mut u8;
        let pc = &mut c as *mut u8;
        let ptr = lang_atomic_ptr_new(pa);
        assert_eq!(unsafe { lang_atomic_ptr_load(ptr, 4) }, pa);
        unsafe { lang_atomic_ptr_store(ptr, pb, 4) };
        assert_eq!(unsafe { lang_atomic_ptr_swap(ptr, pc, 4) }, pb);
        assert_eq!(
            unsafe { lang_atomic_ptr_compare_exchange(ptr, pc, pa, 4, 4) },
            pc
        );
        assert_eq!(unsafe { lang_atomic_ptr_load(ptr, 4) }, pa);
        assert_eq!(
            unsafe { lang_atomic_ptr_compare_exchange(ptr, pc, pb, 4, 4) },
            pa
        );
        assert_eq!(unsafe { lang_atomic_ptr_load(ptr, 4) }, pa);
        unsafe { lang_atomic_ptr_free(ptr) };
    }
}
