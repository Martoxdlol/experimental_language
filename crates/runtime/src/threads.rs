//! OS threads for `Thread.spawn`/`join` (`docs/20` §1).
//!
//! A spawned thread runs a lifted closure — a function `(env) -> R` whose first
//! env word is the function pointer (the closure ABI from `docs/09`). The
//! spawning thread hands the environment over; the child runs it on a fresh OS
//! thread. Results (and the environment) are pinned as global GC roots
//! ([`gc::add_extra_root`]) for the cross-thread handoff window, so a collection
//! on any thread keeps them alive even before they reach a scanned stack.
//!
//! `JoinHandle<R>` (the language type) carries only an integer id into the
//! registry below; `join` blocks (in native state, so the joining thread stays
//! scannable) until the worker finishes and returns its result.

use crate::gc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread::JoinHandle as OsJoin;

/// Shared state between a worker and its joiner.
struct ThreadCtl {
    /// Set true when the worker has finished and `result` is valid.
    done: Mutex<bool>,
    cv: Condvar,
    /// The worker's return value, widened to a machine word (`docs/18`: list/map
    /// slots use the same widening). Non-pointer results are read directly;
    /// pointer results are GC-pinned until [`lang_thread_join`] consumes them.
    result: Mutex<i64>,
    /// True if the worker panicked (panic isolation is a follow-up; currently a
    /// worker panic aborts the process, so this stays false).
    panicked: Mutex<bool>,
    /// The worker's panic message (`str` pointer), valid when `panicked`.
    message: Mutex<usize>,
    /// The OS thread handle, joined for cleanup.
    os: Mutex<Option<OsJoin<()>>>,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<ThreadCtl>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<ThreadCtl>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Spawn `() => R` on a new OS thread. `env` is the closure environment
/// (`[fn_ptr][captures…]`); the function pointer is its first word. Returns a
/// registry id for the resulting `JoinHandle<R>`.
///
/// # Safety
/// `env` must be a valid closure environment whose lifted function has the
/// signature `extern "C" fn(*mut u8) -> i64` (the code generator guarantees a
/// non-float result so the value is returned in the integer register).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_spawn(env: *mut u8) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = std::sync::Arc::new(ThreadCtl {
        done: Mutex::new(false),
        cv: Condvar::new(),
        result: Mutex::new(0),
        panicked: Mutex::new(false),
        message: Mutex::new(0),
        os: Mutex::new(None),
    });
    registry().lock().unwrap().insert(id, ctl.clone());

    // Pin the environment so it survives collection during the handoff (before
    // the child has it rooted on its own stack).
    gc::add_extra_root(env as usize);
    let env_addr = env as usize;

    let worker = ctl.clone();
    let os = std::thread::spawn(move || {
        let fn_ptr = unsafe { (env_addr as *const usize).read() };
        let f: extern "C" fn(*mut u8) -> i64 = unsafe { std::mem::transmute(fn_ptr) };
        let result = f(env_addr as *mut u8);
        // Pin the result for the handoff to the joiner, then publish it.
        gc::add_extra_root(result as usize);
        *worker.result.lock().unwrap() = result;
        // The environment is no longer needed by the child.
        gc::remove_extra_root(env_addr);
        *worker.done.lock().unwrap() = true;
        worker.cv.notify_all();
    });
    registry().lock().unwrap().get(&id).unwrap().os.lock().unwrap().replace(os);
    id
}

/// Spawn `fut` (a `Future<T>` interface-object box) onto a new OS worker that
/// drives it to completion via the executor, returning a registry id for the
/// resulting `JoinHandle<T>` (`docs/21` §6 — `spawn`). `pending_tid` is the
/// `Pending` type id the worker's `block_on` needs.
///
/// # Safety
/// `fut` must be a valid `Future<T>` box (vtable slot 0 = `poll`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_spawn(fut: *mut u8, pending_tid: i64) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = std::sync::Arc::new(ThreadCtl {
        done: Mutex::new(false),
        cv: Condvar::new(),
        result: Mutex::new(0),
        panicked: Mutex::new(false),
        message: Mutex::new(0),
        os: Mutex::new(None),
    });
    registry().lock().unwrap().insert(id, ctl.clone());

    // Pin the future across the handoff (before the worker roots it).
    gc::add_extra_root(fut as usize);
    let fut_addr = fut as usize;

    let worker = ctl.clone();
    let os = std::thread::spawn(move || {
        let result = unsafe { crate::async_rt::lang_block_on(fut_addr as *mut u8, pending_tid) };
        gc::add_extra_root(result as usize); // pin the result for the joiner
        *worker.result.lock().unwrap() = result;
        gc::remove_extra_root(fut_addr);
        *worker.done.lock().unwrap() = true;
        worker.cv.notify_all();
    });
    registry().lock().unwrap().get(&id).unwrap().os.lock().unwrap().replace(os);
    id
}

/// Block until worker `id` finishes; return its (widened) result. The joining
/// thread enters native state so it stays scannable — a collection triggered by
/// the worker can proceed without waiting for the (blocked) joiner.
///
/// # Safety
/// `id` must be a live `JoinHandle` id produced by [`lang_thread_spawn`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_join(id: u64) -> i64 {
    let ctl = registry().lock().unwrap().get(&id).cloned().expect("invalid JoinHandle");
    gc::enter_native();
    let mut done = ctl.done.lock().unwrap();
    while !*done {
        done = ctl.cv.wait(done).unwrap();
    }
    drop(done);
    let result = *ctl.result.lock().unwrap();
    if let Some(os) = ctl.os.lock().unwrap().take() {
        let _ = os.join();
    }
    gc::leave_native();
    // The joiner now holds the result on its (scanned) stack; unpin it.
    gc::remove_extra_root(result as usize);
    result
}

/// Whether worker `id` panicked (valid after [`lang_thread_join`]). Panic
/// isolation across `join` is a follow-up, so this is currently always 0.
///
/// # Safety
/// `id` must be a `JoinHandle` id that has been joined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_panicked(id: u64) -> i64 {
    match registry().lock().unwrap().get(&id) {
        Some(ctl) => *ctl.panicked.lock().unwrap() as i64,
        None => 0,
    }
}

/// The panic message (`str` pointer) for worker `id`, valid when it panicked.
///
/// # Safety
/// `id` must be a `JoinHandle` id that has been joined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_message(id: u64) -> usize {
    match registry().lock().unwrap().get(&id) {
        Some(ctl) => *ctl.message.lock().unwrap(),
        None => 0,
    }
}
