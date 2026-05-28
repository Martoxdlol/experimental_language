//! OS threads for `Thread.spawn` / `JoinHandle.join` (`docs/20` §1) — **async
//! and non-blocking on the joiner side**.
//!
//! A spawned thread runs a lifted closure — a function `(env) -> R` whose first
//! env word is the function pointer (the closure ABI from `docs/09`). The
//! spawning thread hands the environment over; the child runs it on a fresh OS
//! thread. Results (and the environment) are pinned as global GC roots
//! ([`gc::add_extra_root`]) for the cross-thread handoff window, so a
//! collection on any thread keeps them alive even before they reach a scanned
//! stack.
//!
//! `JoinHandle<R>` (the language type) carries only an integer id into the
//! registry below; `join()` is **async** — it returns a
//! `Future<Joined<R> | Panicked>` (`docs/21`). Polling that future checks
//! whether the worker has finished: if so, it builds the result union and
//! reports `Ready`; otherwise it registers the executor's waker (from the poll
//! [`Context`]) and reports `Pending`, so the *task* suspends while the OS
//! thread is free to do other work. When the worker eventually finishes, it
//! invokes the stored waker(s), which re-poll the awaiting task.

use crate::gc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle as OsJoin;

/// A waker captured from a poll [`Context`]: the `(waker_data, wake_fn)` pair a
/// completing worker invokes to re-poll a suspended joiner.
type Waker = (usize, extern "C" fn(*mut u8));

/// The waker context handed to a future's `poll` (`docs/21` §2). Layout matches
/// the language `extern struct Context`; see `async_rt` for the canonical copy.
#[repr(C)]
struct Context {
    waker_data: *mut u8,
    wake_fn: extern "C" fn(*mut u8),
}

/// Per-worker state, guarded by a single mutex so a joiner's "not done →
/// register waker" check and the worker's "publish result → wake" are atomic
/// with respect to each other (no lost wakeups).
struct ThreadInner {
    /// Set true when the worker has finished and `result` is valid.
    done: bool,
    /// Whether the first `Ready` poll has consumed the result (used to drop
    /// the OS handle and unpin the result exactly once).
    taken: bool,
    /// The worker's return value, widened to a machine word (`docs/18`).
    result: i64,
    /// True if the worker panicked. Panic isolation is a follow-up; currently a
    /// worker panic aborts the process, so this stays false.
    panicked: bool,
    /// The worker's panic message (`str` pointer), valid when `panicked`.
    message: usize,
    /// Wakers from suspended `join()` futures.
    waiters: Vec<Waker>,
    /// The OS thread handle, taken on the first `Ready` poll for cleanup.
    os: Option<OsJoin<()>>,
}

struct ThreadCtl {
    inner: Mutex<ThreadInner>,
}

fn registry() -> &'static Mutex<HashMap<u64, Arc<ThreadCtl>>> {
    static R: OnceLock<Mutex<HashMap<u64, Arc<ThreadCtl>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn new_ctl() -> Arc<ThreadCtl> {
    Arc::new(ThreadCtl {
        inner: Mutex::new(ThreadInner {
            done: false,
            taken: false,
            result: 0,
            panicked: false,
            message: 0,
            waiters: Vec::new(),
            os: None,
        }),
    })
}

/// Publish a worker's result and wake every joiner suspended on it.
fn publish_done(ctl: &ThreadCtl, result: i64) {
    let wakers = {
        let mut g = ctl.inner.lock().unwrap();
        g.result = result;
        g.done = true;
        std::mem::take(&mut g.waiters)
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

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
    let ctl = new_ctl();
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
        // Pin the result for the cross-thread handoff to the joiner.
        gc::add_extra_root(result as usize);
        // The environment is no longer needed by the child.
        gc::remove_extra_root(env_addr);
        publish_done(&worker, result);
    });
    ctl.inner.lock().unwrap().os = Some(os);
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
    let ctl = new_ctl();
    registry().lock().unwrap().insert(id, ctl.clone());

    // Pin the future across the handoff (before the worker roots it).
    gc::add_extra_root(fut as usize);
    let fut_addr = fut as usize;

    let worker = ctl.clone();
    let os = std::thread::spawn(move || {
        let result = unsafe { crate::async_rt::lang_block_on(fut_addr as *mut u8, pending_tid) };
        gc::add_extra_root(result as usize); // pin the result for the joiner
        gc::remove_extra_root(fut_addr);
        publish_done(&worker, result);
    });
    ctl.inner.lock().unwrap().os = Some(os);
    id
}

// -- async join: a Future<Joined<R> | Panicked> ------------------------------
//
// The shape mirrors `channels::lang_chan_recv_future`. `join()` returns a
// `Future<Joined<R> | Panicked>` interface-object box
// (`[vtable @0][data @8][type_id @16]`, vtable slot 0 = `thread_join_poll`).
// Polling the future:
//   - reads the worker's `ThreadInner` once under its mutex
//   - if not yet `done`, pushes the executor's waker onto `inner.waiters` and
//     returns a `Pending` box, so the awaiting task suspends
//   - otherwise builds the `Ready<Joined<R> | Panicked>` box from the worker's
//     result (or panic message), joins the OS thread for cleanup on the first
//     `Ready` poll, and unpins the (now traced) value.

/// Build (once) and leak a descriptor blob:
/// `[size][kind=plain][type_id=0][n_ptrs][offsets…]` (`docs/16`).
fn make_desc(size: u64, ptr_offsets: &[u32]) -> *const u8 {
    let mut bytes = Vec::with_capacity(32 + ptr_offsets.len() * 4);
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // kind = plain
    bytes.extend_from_slice(&0u64.to_le_bytes()); // type_id
    bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
    for o in ptr_offsets {
        bytes.extend_from_slice(&o.to_le_bytes());
    }
    Box::leak(bytes.into_boxed_slice()).as_ptr()
}

fn future_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn join_data_desc() -> *const u8 {
    // State: [id][ready_tid][pending_tid][joined_tid][panicked_tid][value_is_ptr]
    // — no managed pointers.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(48, &[]) as usize) as *const u8
}
fn union_managed_desc() -> *const u8 {
    // Union box with a managed payload: [type_id @0][payload @8 (managed)].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[8]) as usize) as *const u8
}
fn union_plain_desc() -> *const u8 {
    // Union box with no payload (Pending): [type_id @0][payload @8 (null)].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[]) as usize) as *const u8
}
fn ready_payload_desc() -> *const u8 {
    // `Ready<Joined<R> | Panicked>` struct: { value: union-box (managed) }.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn joined_value_managed_desc() -> *const u8 {
    // `Joined<R> { value }` when R is managed.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn joined_value_plain_desc() -> *const u8 {
    // `Joined<R> { value }` when R is a scalar.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[]) as usize) as *const u8
}
fn panicked_struct_desc() -> *const u8 {
    // `Panicked { message: str }` — single managed field.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn join_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = thread_join_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Build a `Pending` union box tagged with `pending_tid`.
unsafe fn pending_box(pending_tid: i64) -> *mut u8 {
    let bx = unsafe { gc::alloc(union_plain_desc()) };
    unsafe { (bx as *mut i64).write(pending_tid) };
    bx
}

/// Build the `Ready<Joined<R> | Panicked>` outer-union box.
unsafe fn ready_join_box(
    ready_tid: i64,
    joined_tid: i64,
    panicked_tid: i64,
    result: i64,
    panicked: bool,
    message: usize,
    value_is_ptr: bool,
) -> *mut u8 {
    // Inner `Joined<R> | Panicked` union box: [type_id @0][payload @8].
    let (variant_tid, payload_struct) = if panicked {
        let payload = unsafe { gc::alloc(panicked_struct_desc()) };
        unsafe { (payload as *mut usize).write(message) };
        (panicked_tid, payload)
    } else {
        let desc = if value_is_ptr { joined_value_managed_desc() } else { joined_value_plain_desc() };
        let payload = unsafe { gc::alloc(desc) };
        unsafe { (payload as *mut i64).write(result) };
        (joined_tid, payload)
    };
    let inner = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (inner as *mut i64).write(variant_tid);
        ((inner as usize + 8) as *mut usize).write(payload_struct as usize);
    }
    // `Ready<Joined<R> | Panicked>` struct: a single managed `value` field.
    let ready_payload = unsafe { gc::alloc(ready_payload_desc()) };
    unsafe { (ready_payload as *mut usize).write(inner as usize) };
    // Outer `Ready<Out> | Pending` union: [ready_tid @0][ready_payload @8].
    let bx = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (bx as *mut i64).write(ready_tid);
        ((bx as usize + 8) as *mut usize).write(ready_payload as usize);
    }
    bx
}

extern "C" fn thread_join_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    // data: [id @0][ready_tid @8][pending_tid @16][joined_tid @24]
    //       [panicked_tid @32][value_is_ptr @40].
    let id = unsafe { (data as *const u64).read() };
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let joined_tid = unsafe { ((data as usize + 24) as *const i64).read() };
    let panicked_tid = unsafe { ((data as usize + 32) as *const i64).read() };
    let value_is_ptr = unsafe { ((data as usize + 40) as *const i64).read() } != 0;
    let ctl = registry().lock().unwrap().get(&id).cloned().expect("invalid JoinHandle");

    let (result, panicked, message, os_handle, was_taken) = {
        let mut g = ctl.inner.lock().unwrap();
        if !g.done {
            // Register the executor's waker (under the same lock the worker
            // takes), then report Pending so the task suspends.
            let c = unsafe { &*ctx };
            g.waiters.push((c.waker_data as usize, c.wake_fn));
            drop(g);
            gc::pause();
            let r = unsafe { pending_box(pending_tid) };
            gc::resume();
            return r;
        }
        let was_taken = g.taken;
        g.taken = true;
        let os = if was_taken { None } else { g.os.take() };
        (g.result, g.panicked, g.message, os, was_taken)
    };

    // Worker already returned; this `join()` won't block.
    if let Some(os) = os_handle {
        let _ = os.join();
    }

    gc::pause();
    let r = unsafe {
        ready_join_box(ready_tid, joined_tid, panicked_tid, result, panicked, message, value_is_ptr)
    };
    gc::resume();

    // The value now lives in the (traced) Ready slot; unpin the cross-thread
    // root on the first Ready poll.
    if !was_taken {
        gc::remove_extra_root(result as usize);
    }

    r
}

// -- async spawn-as-Future: spawn EXPR returns Future<T> --------------------
//
// `spawn EXPR` (`docs/21` §6) starts the inner future on a worker and yields a
// new `Future<T>` whose own `poll` returns `Ready<T>` when the worker has
// finished and `Pending` until then. Panics in the worker propagate at the
// awaiter as a language panic (matching the JS/Dart "promise rejection" style).
// We reuse the `ThreadCtl` registry the join-future runs on top of: spawn
// publishes the worker's `i64`-widened result through it; the spawn future's
// poll reads it the same way `thread_join_poll` does, but the result it builds
// is `Ready<T>{ value }` directly, not `Ready<Joined<T>|Panicked>`.

/// Per-spawn-future state — five words, no managed pointers (the registry id
/// resolves the worker's pinned result on completion).
fn spawn_data_desc() -> *const u8 {
    // [id @0][ready_tid @8][pending_tid @16][value_is_ptr @24] — 32B.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(32, &[]) as usize) as *const u8
}
fn ready_t_managed_desc() -> *const u8 {
    // `Ready<T> { value }` when T is managed.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn ready_t_plain_desc() -> *const u8 {
    // `Ready<T> { value }` when T is a scalar.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[]) as usize) as *const u8
}
fn spawn_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = spawn_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Build a `Ready<T> { value: result }` boxed in a `Ready<T> | Pending` union.
unsafe fn ready_value_box(ready_tid: i64, result: i64, value_is_ptr: bool) -> *mut u8 {
    let desc = if value_is_ptr { ready_t_managed_desc() } else { ready_t_plain_desc() };
    let payload = unsafe { gc::alloc(desc) };
    unsafe { (payload as *mut i64).write(result) };
    let bx = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (bx as *mut i64).write(ready_tid);
        ((bx as usize + 8) as *mut usize).write(payload as usize);
    }
    bx
}

extern "C" fn spawn_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    // data: [id @0][ready_tid @8][pending_tid @16][value_is_ptr @24].
    let id = unsafe { (data as *const u64).read() };
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let value_is_ptr = unsafe { ((data as usize + 24) as *const i64).read() } != 0;
    let ctl = registry().lock().unwrap().get(&id).cloned().expect("invalid spawn id");

    let (result, panicked, message, os_handle, was_taken) = {
        let mut g = ctl.inner.lock().unwrap();
        if !g.done {
            let c = unsafe { &*ctx };
            g.waiters.push((c.waker_data as usize, c.wake_fn));
            drop(g);
            gc::pause();
            let r = unsafe { pending_box(pending_tid) };
            gc::resume();
            return r;
        }
        let was_taken = g.taken;
        g.taken = true;
        let os = if was_taken { None } else { g.os.take() };
        (g.result, g.panicked, g.message, os, was_taken)
    };

    if let Some(os) = os_handle {
        let _ = os.join();
    }

    if panicked {
        // Propagate the spawned task's panic at the awaiter (`docs/21` §11).
        unsafe { crate::lang_panic(message as *const crate::strings::LangStr) };
    }

    gc::pause();
    let r = unsafe { ready_value_box(ready_tid, result, value_is_ptr) };
    gc::resume();

    if !was_taken {
        gc::remove_extra_root(result as usize);
    }
    r
}

/// `spawn EXPR` (`docs/21` §6): schedule `fut` on a worker and return a fresh
/// `Future<T>` interface-object box whose poll resolves to `T` when the worker
/// finishes. The returned future is awaitable just like any other.
///
/// # Safety
/// `fut` must be a valid `Future<T>` box (vtable slot 0 = `poll`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_spawn_future(
    fut: *mut u8,
    ready_tid: i64,
    pending_tid: i64,
    value_is_ptr: i64,
) -> *mut u8 {
    let id = unsafe { lang_async_spawn(fut, pending_tid) };
    gc::pause();
    let data = unsafe { gc::alloc(spawn_data_desc()) };
    unsafe {
        (data as *mut u64).write(id);
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
        ((data as usize + 24) as *mut i64).write(value_is_ptr);
    }
    let bx = unsafe { gc::alloc(future_box_desc()) };
    unsafe {
        (bx as *mut usize).write(spawn_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(0);
    }
    gc::resume();
    bx
}

/// Construct a `JoinHandle<R>.join()` future (`docs/20` §1): a
/// `Future<Joined<R> | Panicked>` that resolves once the worker finishes.
///
/// `ready_tid` / `pending_tid` are the code generator's `Ready<Out>` and
/// `Pending` type ids for the outer poll-result union; `joined_tid` /
/// `panicked_tid` tag the inner `Joined<R> | Panicked` variants; `value_is_ptr`
/// is non-zero when `R` is a managed (heap) type so the `Joined.value` slot is
/// GC-traced.
///
/// # Safety
/// Callable only from generated code with the runtime initialised; `id` must be
/// a live `JoinHandle` id produced by [`lang_thread_spawn`] (or
/// [`lang_async_spawn`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_join_future(
    id: u64,
    ready_tid: i64,
    pending_tid: i64,
    joined_tid: i64,
    panicked_tid: i64,
    value_is_ptr: i64,
) -> *mut u8 {
    gc::pause();
    let data = unsafe { gc::alloc(join_data_desc()) };
    unsafe {
        (data as *mut u64).write(id);
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
        ((data as usize + 24) as *mut i64).write(joined_tid);
        ((data as usize + 32) as *mut i64).write(panicked_tid);
        ((data as usize + 40) as *mut i64).write(value_is_ptr);
    }
    let bx = unsafe { gc::alloc(future_box_desc()) };
    unsafe {
        (bx as *mut usize).write(join_vtable() as usize); // vtable @0
        ((bx as usize + 8) as *mut usize).write(data as usize); // data @8
        ((bx as usize + 16) as *mut i64).write(0); // type_id @16
    }
    gc::resume();
    bx
}
