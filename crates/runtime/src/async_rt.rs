//! The async executor (`docs/21` §6): drives `Future` state machines.
//!
//! A `Future<Out>` value is the interface-object box the code generator emits
//! for any interface value (`docs/11` §5): `[vtable @0][data @8][type_id @16]`.
//! The `Future` interface has exactly one method, `poll`, so the vtable's first
//! (and only) slot is the poll function:
//!
//! ```text
//! poll: extern "C" fn(self: *mut u8 /*= the box's data ptr*/,
//!                     ctx:  *mut Context) -> *mut u8
//! ```
//!
//! `poll` returns a language `Ready<Out> | Pending` union box
//! (`[type_id @0][payload @8]`, `docs/03`). When the leading `type_id` equals
//! the `Pending` type id the task is not yet complete; otherwise the payload at
//! `+8` is the `Ready<Out>` struct, whose `value` field (offset 0) is the
//! result, widened to a machine word.
//!
//! [`lang_block_on`] polls a top-level future to completion on the current
//! thread, parking on a condvar between polls. The future arranges its own
//! re-poll by invoking `ctx.wake_fn(ctx.waker_data)` (typically from an I/O
//! callback or a timer thread); a future that returns `Pending` without ever
//! waking is a forever-hung task, exactly as the spec warns.

use crate::gc;
use std::sync::{Condvar, Mutex, OnceLock};

/// The waker context handed to `poll` (`docs/21` §2). Layout matches the
/// language `extern struct Context { waker_data: *u8, wake_fn: extern (*u8) =>
/// null }` so a C event loop can supply the callback natively.
#[repr(C)]
pub struct Context {
    waker_data: *mut u8,
    wake_fn: extern "C" fn(*mut u8),
}

/// A parked-thread waker: a flag the future sets (via [`wake_thunk`]) and a
/// condvar the blocked executor waits on.
struct ThreadWaker {
    woken: Mutex<bool>,
    cv: Condvar,
}

/// The `wake_fn` installed in the [`Context`] for [`lang_block_on`]. `data`
/// points at the executor's [`ThreadWaker`]; waking sets the flag and signals
/// the condvar so a parked `block_on` re-polls.
extern "C" fn wake_thunk(data: *mut u8) {
    let w = unsafe { &*(data as *const ThreadWaker) };
    *w.woken.lock().unwrap() = true;
    w.cv.notify_all();
}

/// Drive `fut` (a `Future<Out>` interface-object box) to completion on the
/// current thread and return its `Out`, widened to a machine word
/// (`docs/21` §6 — `block_on`). `pending_tid` is the `Pending` type id the code
/// generator passes so a `Pending` poll result can be distinguished from a
/// `Ready<Out>`.
///
/// Between `Pending` polls the thread parks on a condvar (in GC *native* state,
/// so it stays scannable) until the future's waker fires.
///
/// # Safety
/// `fut` must be a valid `Future<Out>` interface-object box whose vtable slot 0
/// is a `poll` function with the ABI documented at the module level.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_block_on(fut: *mut u8, pending_tid: i64) -> i64 {
    let waker = ThreadWaker {
        woken: Mutex::new(false),
        cv: Condvar::new(),
    };
    let waker_ptr = &waker as *const ThreadWaker as *mut u8;

    // `fut` lives only in this (native) frame, which the collector does not
    // scan; pin it (and thus its whole reachable graph, including the state
    // struct) so a collection triggered inside `poll` cannot free it.
    gc::add_extra_root(fut as usize);

    let out = loop {
        // Decode the interface-object box: vtable @0, data @8.
        let vtable = unsafe { (fut as *const usize).read() } as *const usize;
        let poll_addr = unsafe { vtable.read() };
        let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
            unsafe { std::mem::transmute(poll_addr) };
        let data = unsafe { ((fut as usize + 8) as *const usize).read() } as *mut u8;

        let mut ctx = Context {
            waker_data: waker_ptr,
            wake_fn: wake_thunk,
        };
        let result = poll(data, &mut ctx);

        // The poll result is a `Ready<Out> | Pending` union box: type_id @0.
        let tag = unsafe { (result as *const i64).read() };
        if tag != pending_tid {
            // Ready<Out>: payload @8 is the `Ready<Out>` struct; its `value`
            // field is at offset 0.
            let ready = unsafe { ((result as usize + 8) as *const usize).read() };
            break unsafe { (ready as *const i64).read() };
        }

        // Pending: park until the future wakes us. Check the flag first so a
        // wake that fired during `poll` is not missed.
        gc::enter_native();
        let mut woken = waker.woken.lock().unwrap();
        while !*woken {
            woken = waker.cv.wait(woken).unwrap();
        }
        *woken = false;
        drop(woken);
        gc::leave_native();
    };

    gc::remove_extra_root(fut as usize);
    out
}

// -- yield_now: a future that suspends exactly once --------------------------
//
// `yield_now()` (`docs/21`) returns a `Future<null>` that, the first time it is
// polled, schedules an immediate re-poll (via the waker) and returns `Pending`;
// the second time it returns `Ready`. It is the minimal genuinely-suspending
// future — it exercises the full state machine + executor park/resume loop
// without needing real I/O. The future and its state are GC-managed (the
// awaiting state machine stores the future in a traced slot).

/// Build (once) and return a leaked descriptor blob:
/// `[size][kind=plain][type_id=0][n_ptrs][offsets…][n_rc=0]`.
///
/// The trailing `n_rc` word is mandatory on every descriptor (see the
/// `gc` module descriptor doc): the collector reads it for every object it
/// reclaims. These async-runtime boxes own no `@RefCounted` fields, so `n_rc`
/// is `0`.
fn make_desc(size: u64, ptr_offsets: &[u32]) -> *const u8 {
    let mut bytes = Vec::with_capacity(36 + ptr_offsets.len() * 4);
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // kind = plain
    bytes.extend_from_slice(&0u64.to_le_bytes()); // type_id
    bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
    for o in ptr_offsets {
        bytes.extend_from_slice(&o.to_le_bytes());
    }
    bytes.extend_from_slice(&0u32.to_le_bytes()); // n_rc = 0 (no refcounted fields)
    Box::leak(bytes.into_boxed_slice()).as_ptr()
}

fn yield_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn yield_data_desc() -> *const u8 {
    // State: [polled][ready_tid][pending_tid] — no managed pointers.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[]) as usize) as *const u8
}
fn union_managed_desc() -> *const u8 {
    // Ready box: [type_id][payload @8 (managed)].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[8]) as usize) as *const u8
}
fn union_plain_desc() -> *const u8 {
    // Pending box: [type_id][payload @8 (null)].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[]) as usize) as *const u8
}
fn value_desc() -> *const u8 {
    // Ready<null>.value slot (8 bytes, null).
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[]) as usize) as *const u8
}
fn yield_vtable() -> *const u8 {
    // One slot: the poll function pointer.
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = yield_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

extern "C" fn yield_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    // data: [polled @0][ready_tid @8][pending_tid @16].
    let polled = unsafe { (data as *const i64).read() };
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    gc::pause(); // the result box + payload must not be collected mid-build
    let result = if polled != 0 {
        let payload = unsafe { gc::alloc(value_desc()) }; // Ready<null>.value = 0
        let bx = unsafe { gc::alloc(union_managed_desc()) };
        unsafe {
            (bx as *mut i64).write(ready_tid);
            ((bx as usize + 8) as *mut usize).write(payload as usize);
        }
        bx
    } else {
        unsafe { (data as *mut i64).write(1) }; // mark polled
        // Schedule an immediate re-poll, then report Pending.
        let c = unsafe { &*ctx };
        (c.wake_fn)(c.waker_data);
        let bx = unsafe { gc::alloc(union_plain_desc()) };
        unsafe { (bx as *mut i64).write(pending_tid) };
        bx
    };
    gc::resume();
    result
}

/// Build a `Ready<null>` union box (value = 0), tagged with `ready_tid`.
unsafe fn ready_null_box(ready_tid: i64) -> *mut u8 {
    let payload = unsafe { gc::alloc(value_desc()) };
    let bx = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (bx as *mut i64).write(ready_tid);
        ((bx as usize + 8) as *mut usize).write(payload as usize);
    }
    bx
}

/// Build a `Pending` union box tagged with `pending_tid`.
unsafe fn pending_box(pending_tid: i64) -> *mut u8 {
    let bx = unsafe { gc::alloc(union_plain_desc()) };
    unsafe { (bx as *mut i64).write(pending_tid) };
    bx
}

// -- sleep: a future that completes after a delay ----------------------------

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Per-`sleep` timer state, keyed by id in [`sleep_registry`].
struct SleepCell {
    ms: i64,
    started: AtomicBool,
    fired: AtomicBool,
}

fn sleep_registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<SleepCell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<SleepCell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
static SLEEP_NEXT: AtomicU64 = AtomicU64::new(1);

fn sleep_data_desc() -> *const u8 {
    // [ready_tid][pending_tid][id] — no managed pointers.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[]) as usize) as *const u8
}
fn sleep_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = sleep_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

extern "C" fn sleep_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let ready_tid = unsafe { (data as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let id = unsafe { ((data as usize + 16) as *const i64).read() } as u64;
    let cell = sleep_registry().lock().unwrap().get(&id).cloned();
    let Some(cell) = cell else {
        // Unknown id — treat as already elapsed.
        gc::pause();
        let r = unsafe { ready_null_box(ready_tid) };
        gc::resume();
        return r;
    };
    if cell.fired.load(Ordering::SeqCst) {
        gc::pause();
        let r = unsafe { ready_null_box(ready_tid) };
        gc::resume();
        return r;
    }
    if !cell.started.swap(true, Ordering::SeqCst) {
        // First poll: arm a timer thread that wakes us after `ms`.
        let c = unsafe { &*ctx };
        let waker_data = c.waker_data as usize;
        let wake_fn = c.wake_fn;
        let cell2 = cell.clone();
        let ms = cell.ms.max(0) as u64;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            cell2.fired.store(true, Ordering::SeqCst);
            wake_fn(waker_data as *mut u8);
        });
    }
    gc::pause();
    let r = unsafe { pending_box(pending_tid) };
    gc::resume();
    r
}

/// Construct a `sleep(ms)` future (`docs/21` §9 helper) — a `Future<null>` that
/// completes after `ms` milliseconds, waking the executor via a timer thread.
///
/// # Safety
/// Callable only from generated code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_sleep(ms: i64, ready_tid: i64, pending_tid: i64) -> *mut u8 {
    let id = SLEEP_NEXT.fetch_add(1, Ordering::Relaxed);
    sleep_registry().lock().unwrap().insert(
        id,
        std::sync::Arc::new(SleepCell {
            ms,
            started: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        }),
    );
    gc::pause();
    let data = unsafe { gc::alloc(sleep_data_desc()) };
    unsafe {
        (data as *mut i64).write(ready_tid);
        ((data as usize + 8) as *mut i64).write(pending_tid);
        ((data as usize + 16) as *mut i64).write(id as i64);
    }
    let bx = unsafe { gc::alloc(yield_box_desc()) };
    unsafe {
        (bx as *mut usize).write(sleep_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(0);
    }
    gc::resume();
    bx
}

// -- timeout: race a future against a deadline -------------------------------
//
// `timeout(fut, ms): Future<T | TimedOut>` polls `fut`; if it is ready the value
// is reboxed as the `T` variant of `T | TimedOut`, otherwise once `ms` elapses
// the future resolves to `TimedOut`. The code generator supplies `t_id` /
// `t_is_ptr` (so the runtime can build the `T`-variant box and the collector can
// trace its payload) plus the `TimedOut` / `Ready` / `Pending` type ids.

struct TimeoutCell {
    ms: i64,
    started: AtomicBool,
    fired: AtomicBool,
}
fn timeout_registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<TimeoutCell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<TimeoutCell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
static TIMEOUT_NEXT: AtomicU64 = AtomicU64::new(1);

fn timeout_data_desc() -> *const u8 {
    // [inner_fut @0 (managed)][ready_tid][pending_tid][t_id][t_is_ptr]
    // [timedout_tid][id] — only the inner future is a managed pointer.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(56, &[0]) as usize) as *const u8
}
fn timeout_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = timeout_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Wrap a `T | TimedOut` union box (`tbox`) in a `Ready<Out>` struct and then in
/// an outer `Ready<Out> | Pending` box tagged `ready_tid`. Caller holds a GC
/// pause around the whole sequence.
unsafe fn wrap_ready(tbox: *mut u8, ready_tid: i64) -> *mut u8 {
    let ready_out = unsafe { gc::alloc(value_desc_ptr()) }; // Ready<Out>.value @0 (managed)
    unsafe { (ready_out as *mut usize).write(tbox as usize) };
    let outer = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (outer as *mut i64).write(ready_tid);
        ((outer as usize + 8) as *mut usize).write(ready_out as usize);
    }
    outer
}
fn value_desc_ptr() -> *const u8 {
    // A `Ready<Out>` struct holding one managed pointer field.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}

extern "C" fn timeout_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let inner_fut = unsafe { (data as *const usize).read() } as *mut u8;
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let t_id = unsafe { ((data as usize + 24) as *const i64).read() };
    let t_is_ptr = unsafe { ((data as usize + 32) as *const i64).read() };
    let timedout_tid = unsafe { ((data as usize + 40) as *const i64).read() };
    let id = unsafe { ((data as usize + 48) as *const i64).read() } as u64;

    // Poll the inner future once.
    let vtable = unsafe { (inner_fut as *const usize).read() } as *const usize;
    let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
        unsafe { std::mem::transmute(vtable.read()) };
    let inner_data = unsafe { ((inner_fut as usize + 8) as *const usize).read() } as *mut u8;
    let r = poll(inner_data, ctx);
    let tag = unsafe { (r as *const i64).read() };

    if tag != pending_tid {
        // Inner ready: rebox its value as the `T` variant of `T | TimedOut`.
        let ready_struct = unsafe { ((r as usize + 8) as *const usize).read() };
        let value = unsafe { (ready_struct as *const i64).read() };
        gc::pause();
        let tbox = unsafe {
            gc::alloc(if t_is_ptr != 0 { union_managed_desc() } else { union_plain_desc() })
        };
        unsafe {
            (tbox as *mut i64).write(t_id);
            ((tbox as usize + 8) as *mut i64).write(value);
        }
        let out = unsafe { wrap_ready(tbox, ready_tid) };
        gc::resume();
        return out;
    }

    // Inner pending: arm the deadline timer on first poll, then report
    // `TimedOut` once it fires (the inner future's own waker covers the
    // ready case; either wake re-polls us).
    let cell = timeout_registry().lock().unwrap().get(&id).cloned();
    if let Some(cell) = cell {
        if cell.fired.load(Ordering::SeqCst) {
            gc::pause();
            let tbox = unsafe { gc::alloc(union_plain_desc()) };
            unsafe { (tbox as *mut i64).write(timedout_tid) };
            let out = unsafe { wrap_ready(tbox, ready_tid) };
            gc::resume();
            return out;
        }
        if !cell.started.swap(true, Ordering::SeqCst) {
            let c = unsafe { &*ctx };
            let waker_data = c.waker_data as usize;
            let wake_fn = c.wake_fn;
            let cell2 = cell.clone();
            let ms = cell.ms.max(0) as u64;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                cell2.fired.store(true, Ordering::SeqCst);
                wake_fn(waker_data as *mut u8);
            });
        }
    }
    gc::pause();
    let p = unsafe { pending_box(pending_tid) };
    gc::resume();
    p
}

/// Construct a `timeout(fut, ms)` future (`docs/21` §9).
///
/// # Safety
/// Callable only from generated code; `fut` is a managed `Future<T>` box.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_timeout(
    fut: *mut u8,
    ms: i64,
    t_id: i64,
    t_is_ptr: i64,
    timedout_tid: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let id = TIMEOUT_NEXT.fetch_add(1, Ordering::Relaxed);
    timeout_registry().lock().unwrap().insert(
        id,
        std::sync::Arc::new(TimeoutCell {
            ms,
            started: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        }),
    );
    // `fut` is unrooted while we build the timeout future — pin it across the
    // allocations so a collection cannot free it before it is stored.
    gc::add_extra_root(fut as usize);
    gc::pause();
    let data = unsafe { gc::alloc(timeout_data_desc()) };
    unsafe {
        (data as *mut usize).write(fut as usize);
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
        ((data as usize + 24) as *mut i64).write(t_id);
        ((data as usize + 32) as *mut i64).write(t_is_ptr);
        ((data as usize + 40) as *mut i64).write(timedout_tid);
        ((data as usize + 48) as *mut i64).write(id as i64);
    }
    let bx = unsafe { gc::alloc(yield_box_desc()) };
    unsafe {
        (bx as *mut usize).write(timeout_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(0);
    }
    gc::resume();
    gc::remove_extra_root(fut as usize);
    bx
}

/// Construct a `yield_now()` future (`docs/21`). `ready_tid` / `pending_tid` are
/// the code generator's `Ready<null>` and `Pending` type ids, so the future's
/// `poll` returns a `Ready<null> | Pending` union the awaiting state machine and
/// `block_on` understand.
///
/// # Safety
/// Callable only from generated code with the runtime initialised.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_yield(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    gc::pause();
    let data = unsafe { gc::alloc(yield_data_desc()) };
    unsafe {
        (data as *mut i64).write(0); // polled = false
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
    }
    let bx = unsafe { gc::alloc(yield_box_desc()) };
    unsafe {
        (bx as *mut usize).write(yield_vtable() as usize); // vtable @0
        ((bx as usize + 8) as *mut usize).write(data as usize); // data @8
        ((bx as usize + 16) as *mut i64).write(0); // type_id @16
    }
    gc::resume();
    bx
}

#[cfg(test)]
mod tests {
    use super::*;

    // A `poll` that completes immediately: returns a `Ready<Out>` union box
    // whose value is 42. Memory is leaked (test-only).
    extern "C" fn ready_poll(_self: *mut u8, _ctx: *mut Context) -> *mut u8 {
        let ready_struct: Box<[i64; 1]> = Box::new([42]); // Ready { value: 42 }
        let ready_ptr = Box::into_raw(ready_struct) as usize;
        // union box: [type_id = 7 (Ready)][payload = ready_struct ptr]
        let union_box: Box<[usize; 2]> = Box::new([7, ready_ptr]);
        Box::into_raw(union_box) as *mut u8
    }

    #[test]
    fn block_on_returns_immediately_ready() {
        // vtable: one slot = poll fn address.
        let vtable: Box<[usize; 1]> = Box::new([ready_poll as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data: Box<[i64; 1]> = Box::new([0]); // unused state
        let data_ptr = Box::into_raw(data) as usize;
        // interface-object box: [vtable][data][type_id].
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        let fut_ptr = Box::into_raw(fut) as *mut u8;
        // Pending type id is 9 here; the Ready box carries 7, so it is Ready.
        let out = unsafe { lang_block_on(fut_ptr, 9) };
        assert_eq!(out, 42);
    }

    // A `poll` that is Pending on the first call (and wakes itself for an
    // immediate re-poll) then Ready on the second — exercises the park/wake
    // path without a real I/O source.
    static POLLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    extern "C" fn yield_once_poll(_self: *mut u8, ctx: *mut Context) -> *mut u8 {
        let n = POLLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            // Arrange an immediate re-poll, then report Pending.
            let c = unsafe { &*ctx };
            (c.wake_fn)(c.waker_data);
            let pending_box: Box<[usize; 2]> = Box::new([9, 0]); // type_id = 9 (Pending)
            return Box::into_raw(pending_box) as *mut u8;
        }
        let ready_struct: Box<[i64; 1]> = Box::new([7]);
        let ready_ptr = Box::into_raw(ready_struct) as usize;
        let union_box: Box<[usize; 2]> = Box::new([7, ready_ptr]);
        Box::into_raw(union_box) as *mut u8
    }

    #[test]
    fn block_on_parks_then_resumes_on_wake() {
        POLLS.store(0, std::sync::atomic::Ordering::SeqCst);
        let vtable: Box<[usize; 1]> = Box::new([yield_once_poll as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data_ptr = Box::into_raw(Box::new([0i64; 1])) as usize;
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        let fut_ptr = Box::into_raw(fut) as *mut u8;
        let out = unsafe { lang_block_on(fut_ptr, 9) };
        assert_eq!(out, 7);
        assert_eq!(POLLS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
