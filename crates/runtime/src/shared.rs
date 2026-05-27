//! `Shared<T>` — an explicit mutex for genuinely shared mutable state
//! (`docs/20` §4). The language-level `Shared<T>` struct carries only an id into
//! the registry below; all clones of a handle share the same cell and therefore
//! the same lock.
//!
//! Locking is a logical flag guarded by a short-held `Mutex` + `Condvar`, not a
//! held `MutexGuard` (the lock is acquired in one runtime call and released in
//! another, around a call back into generated code). The protected value is a
//! machine word — for a managed (struct) `T` it is a pointer the body mutates in
//! place; it is GC-pinned for the cell's lifetime so it survives collection even
//! though no thread stack references it.

use crate::gc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

struct State {
    locked: bool,
    value: i64,
}

struct Cell {
    state: Mutex<State>,
    cv: Condvar,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<Cell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Cell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Create a `Shared` cell holding `value`; returns its id.
///
/// # Safety
/// If `value` is a managed pointer it is pinned for the cell's lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_new(value: i64) -> u64 {
    gc::add_extra_root(value as usize); // the cell holds the only reference
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cell = std::sync::Arc::new(Cell {
        state: Mutex::new(State { locked: false, value }),
        cv: Condvar::new(),
    });
    registry().lock().unwrap().insert(id, cell);
    id
}

fn cell(id: u64) -> std::sync::Arc<Cell> {
    registry().lock().unwrap().get(&id).cloned().expect("invalid Shared id")
}

/// Acquire the lock (blocking, in native GC state) and return the protected
/// value. Pair with [`lang_shared_unlock`].
///
/// # Safety
/// `id` must be a live `Shared` id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_lock(id: u64) -> i64 {
    let cell = cell(id);
    gc::enter_native();
    let mut st = cell.state.lock().unwrap();
    while st.locked {
        st = cell.cv.wait(st).unwrap();
    }
    st.locked = true;
    let v = st.value;
    drop(st);
    gc::leave_native();
    v
}

/// Release the lock taken by [`lang_shared_lock`].
///
/// # Safety
/// `id` must be a live `Shared` id currently locked by this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_unlock(id: u64) {
    let cell = cell(id);
    let mut st = cell.state.lock().unwrap();
    st.locked = false;
    cell.cv.notify_one();
}

/// Try to acquire the lock without blocking. Writes `1` to `*got` and returns
/// the value on success; writes `0` and returns `0` if the lock was busy.
///
/// # Safety
/// `id` must be a live `Shared` id; `got` must point to a writable `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_try_lock(id: u64, got: *mut i64) -> i64 {
    let cell = cell(id);
    let mut st = cell.state.lock().unwrap();
    if st.locked {
        unsafe { got.write(0) };
        0
    } else {
        st.locked = true;
        unsafe { got.write(1) };
        st.value
    }
}
