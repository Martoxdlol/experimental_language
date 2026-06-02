//! `Shared<T>` — an explicit ASYNC mutex for genuinely shared mutable state
//! (`docs/20` §4). The language-level `Shared<T>` struct carries only an id into
//! the registry below; all clones of a handle share the same cell and therefore
//! the same lock.
//!
//! The lock is **always async** and **task-aware** (tokio::Mutex-style): acquiring
//! a contended lock SUSPENDS the awaiting task — it never parks the OS thread.
//! Acquisition is fair: waiters are queued FIFO by ticket and served in order
//! (no barging), so the lock is starvation-free. The cell holds a logical
//! `locked` flag plus a queue of executor wakers `(waker_data, wake_fn)`; a
//! contended [`acquire`](lang_shared_acquire) returns `Pending` after recording
//! the caller's waker, and [`release`](lang_shared_release) wakes the live FIFO
//! head so it re-polls and takes the lock.
//!
//! The protected value is a machine word — for a managed (struct) `T` it is a
//! pointer the body mutates in place; it is GC-pinned for the cell's lifetime so
//! it survives collection even though no thread stack references it.
//!
//! Release on cancel/panic: a per-thread *held-lock set* records every lock this
//! thread currently holds; the worker panic boundary and `Future.cancel` drain it
//! via [`lang_shared_release_all`], so a body that unwinds or is cancelled never
//! leaks the lock (the async equivalent of RAII guard-drop). Executor tasks carry
//! their own held-lock set across worker threads; legacy dedicated thread paths
//! use a thread-local fallback.

use crate::async_rt::Context;
use crate::gc;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};

/// A queued waiter: the executor waker to invoke when it is this waiter's turn,
/// plus its FIFO ticket. The raw `data` pointer is opaque (it points into the
/// executor that is currently parking the task); the executor guarantees it stays
/// valid until the task is woken, so the cell may carry it across threads.
#[derive(Clone, Copy)]
struct Waker {
    data: *mut u8,
    wake: extern "C" fn(*mut u8),
    ticket: u64,
}
// SAFETY: `data`/`wake` form an opaque executor callback. Waking from another
// thread (the releaser) is exactly the intended cross-thread hand-off.
unsafe impl Send for Waker {}

struct State {
    locked: bool,
    value: i64,
    queue: VecDeque<Waker>,
    next_ticket: u64,
}

struct Cell {
    state: Mutex<State>,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<Cell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Cell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.try_lock() {
        Ok(guard) => return guard,
        Err(TryLockError::Poisoned(err)) => return err.into_inner(),
        Err(TryLockError::WouldBlock) => {}
    }
    gc::enter_runtime_native_no_roots();
    let guard = mutex.lock().unwrap_or_else(|err| err.into_inner());
    gc::leave_native();
    guard
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Fallback locks held by a thread running the legacy one-task-per-thread
    /// paths. Executor tasks install `TASK_HELD` while polling, so held locks
    /// move with the task instead of with whichever worker thread last polled it.
    static HELD: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    static TASK_HELD: RefCell<Option<Arc<Mutex<Vec<u64>>>>> = const { RefCell::new(None) };
}

pub fn enter_task_held(held: Arc<Mutex<Vec<u64>>>) {
    TASK_HELD.with(|h| *h.borrow_mut() = Some(held));
}

pub fn exit_task_held() {
    TASK_HELD.with(|h| *h.borrow_mut() = None);
}

fn held_push(id: u64) {
    if TASK_HELD.with(|h| {
        if let Some(task) = h.borrow().as_ref() {
            runtime_lock(task).push(id);
            true
        } else {
            false
        }
    }) {
        return;
    }
    HELD.with(|h| h.borrow_mut().push(id));
}
fn held_pop(id: u64) {
    if TASK_HELD.with(|h| {
        if let Some(task) = h.borrow().as_ref() {
            let mut v = runtime_lock(task);
            if let Some(pos) = v.iter().rposition(|&x| x == id) {
                v.remove(pos);
            }
            true
        } else {
            false
        }
    }) {
        return;
    }
    HELD.with(|h| {
        let mut v = h.borrow_mut();
        if let Some(pos) = v.iter().rposition(|&x| x == id) {
            v.remove(pos);
        }
    });
}

fn cell(id: u64) -> std::sync::Arc<Cell> {
    runtime_lock(registry())
        .get(&id)
        .cloned()
        .expect("invalid Shared id")
}

// -- descriptor + result-box helpers (mirror `async_rt`) ---------------------

/// Build (once) a leaked descriptor blob
/// `[size][kind=plain][type_id=0][n_ptrs][offsets…][n_rc=0]` (`docs/16`).
fn make_desc(size: u64, ptr_offsets: &[u32]) -> *const u8 {
    let mut bytes = Vec::with_capacity(36 + ptr_offsets.len() * 4);
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // kind = plain
    bytes.extend_from_slice(&0u64.to_le_bytes()); // type_id
    bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
    for o in ptr_offsets {
        bytes.extend_from_slice(&o.to_le_bytes());
    }
    bytes.extend_from_slice(&0u32.to_le_bytes()); // n_rc = 0
    Box::leak(bytes.into_boxed_slice()).as_ptr()
}

fn lock_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn lock_data_desc() -> *const u8 {
    // Lock-future state (see [`lock_poll`]). Managed slots: `env` @16 and the
    // suspended inner body future @24.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(LOCK_DATA_SIZE, &[16, 24]) as usize) as *const u8
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
fn value_plain_desc() -> *const u8 {
    // `Ready<R>.value` slot when `R` is not a pointer (8 bytes, untraced).
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[]) as usize) as *const u8
}
fn value_ptr_desc() -> *const u8 {
    // `Ready<R>.value` slot when `R` is a managed pointer (traced @0).
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn lock_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = lock_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Build a `Ready<R>` union box carrying `value` (widened to a word), tagged with
/// `ready_tid`. `r_is_ptr` selects the traced/untraced value-slot descriptor so
/// the collector traces a managed `R`. Caller holds a GC pause.
unsafe fn ready_value_box(ready_tid: i64, value: i64, r_is_ptr: bool) -> *mut u8 {
    let vdesc = if r_is_ptr {
        value_ptr_desc()
    } else {
        value_plain_desc()
    };
    let payload = unsafe { gc::alloc(vdesc) };
    unsafe { (payload as *mut i64).write(value) };
    let bx = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (bx as *mut i64).write(ready_tid);
        ((bx as usize + 8) as *mut usize).write(payload as usize);
    }
    bx
}

/// Build a `Pending` union box tagged with `pending_tid`. Caller holds a GC pause.
unsafe fn pending_box(pending_tid: i64) -> *mut u8 {
    let bx = unsafe { gc::alloc(union_plain_desc()) };
    unsafe { (bx as *mut i64).write(pending_tid) };
    bx
}

// -- public runtime ABI ------------------------------------------------------

/// Create a `Shared` cell holding `value`; returns its id.
///
/// # Safety
/// If `value` is a managed pointer it is pinned for the cell's lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_new(value: i64) -> u64 {
    gc::add_extra_root(value as usize); // the cell holds the only reference
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cell = std::sync::Arc::new(Cell {
        state: Mutex::new(State {
            locked: false,
            value,
            queue: VecDeque::new(),
            next_ticket: 1,
        }),
    });
    runtime_lock(registry()).insert(id, cell);
    id
}

const ST_INIT: i64 = 0;
const ST_QUEUED: i64 = 1;
const ST_ACQUIRED: i64 = 2;

/// Pure FIFO decision for one acquire poll, operating on the locked `State`.
/// Given the current resume `st` (and `ticket` when already queued) plus the
/// caller's waker, it either takes the lock (returns `acquired = true`) or
/// enqueues / stays queued (returns `false`), and reports the next `(st, ticket)`
/// to persist. Non-barging: a fresh `INIT` poll may take the lock only when it is
/// free AND the queue is empty; a `QUEUED` poll only when it is free AND at the
/// FIFO head. Factored out of [`acquire_poll`] so it is unit-testable without GC.
fn advance(
    s: &mut State,
    st: i64,
    ticket: u64,
    wdata: *mut u8,
    wfn: extern "C" fn(*mut u8),
) -> (bool, i64, u64) {
    prune_dead_front(s);
    if st == ST_ACQUIRED {
        return (true, ST_ACQUIRED, ticket); // idempotent re-poll
    }
    if st == ST_INIT {
        if !s.locked && s.queue.is_empty() {
            s.locked = true;
            return (true, ST_ACQUIRED, ticket);
        }
        remove_duplicate_task_waiters(s, wdata, wfn, None);
        if !s.locked && s.queue.is_empty() {
            s.locked = true;
            return (true, ST_ACQUIRED, ticket);
        }
        let t = s.next_ticket;
        s.next_ticket += 1;
        s.queue.push_back(Waker {
            data: wdata,
            wake: wfn,
            ticket: t,
        });
        return (false, ST_QUEUED, t);
    }
    // ST_QUEUED: refresh our (possibly changed) waker, then take the lock iff it
    // is free and we are at the head of the FIFO queue. A stale duplicate wake
    // can re-poll a future after its previous queue entry was already consumed;
    // in that case, never park on an unwakeable missing ticket. Re-enter the
    // FIFO protocol with a fresh ticket (or take the lock if it is truly free
    // and uncontended).
    if let Some(w) = s.queue.iter_mut().find(|w| w.ticket == ticket) {
        w.data = wdata;
        w.wake = wfn;
    } else {
        if !s.locked {
            s.locked = true;
            return (true, ST_ACQUIRED, ticket);
        }
        remove_duplicate_task_waiters(s, wdata, wfn, None);
        let t = s.next_ticket;
        s.next_ticket += 1;
        s.queue.push_back(Waker {
            data: wdata,
            wake: wfn,
            ticket: t,
        });
        return (false, ST_QUEUED, t);
    }
    if !s.locked && s.queue.front().map(|w| w.ticket) == Some(ticket) {
        s.queue.pop_front();
        s.locked = true;
        return (true, ST_ACQUIRED, ticket);
    }
    (false, ST_QUEUED, ticket)
}

fn remove_duplicate_task_waiters(
    s: &mut State,
    data: *mut u8,
    wake: extern "C" fn(*mut u8),
    keep_ticket: Option<u64>,
) {
    let Some((true, task_id)) = crate::threads::executor_waker_task(data, wake) else {
        return;
    };
    s.queue.retain(|w| {
        if keep_ticket == Some(w.ticket) {
            return true;
        }
        crate::threads::executor_waker_task_id(w.data, w.wake) != Some(task_id)
    });
}

fn prune_dead_front(s: &mut State) {
    while let Some(w) = s.queue.front().copied() {
        match crate::threads::executor_waker_is_live(w.data, w.wake) {
            Some(false) => {
                s.queue.pop_front();
            }
            _ => break,
        }
    }
}

// Lock-future state layout (`lock_poll`):
//   [0]  state: 0 = acquire-init, 1 = acquire-queued, 2 = running body
//   [8]  id
//   [16] env       (managed) — the body closure environment `[fn_ptr][caps…]`
//   [24] inner_fut (managed) — the suspended async body future (0 until running)
//   [32] clone_fn  — `extern "C" fn(i64)->i64` cloning `R` (0 = no clone needed)
//   [40] body_is_async (0/1)
//   [48] r_is_ptr      (0/1) — whether `R` is a managed pointer (for tracing)
//   [56] ready_tid
//   [64] pending_tid
//   [72] ticket — FIFO ticket once queued
//   [80] is_try   (0/1) — `try_lock`: non-suspending acquire, `R | LockBusy` out
//   [88] r_tid    — type id of the `R` variant (try_lock only)
//   [96] busy_tid — type id of `LockBusy`  (try_lock only)
const LOCK_DATA_SIZE: u64 = 104;
const LST_ACQ_INIT: i64 = 0;
const LST_ACQ_QUEUED: i64 = 1;
const LST_RUNNING: i64 = 2;

/// Build a `[type_id @0][payload @8]` union box (`docs/03`). `payload_is_ptr`
/// selects the traced descriptor so a managed payload survives collection.
/// Caller holds a GC pause.
unsafe fn union_box(tid: i64, payload: i64, payload_is_ptr: bool) -> *mut u8 {
    let desc = if payload_is_ptr {
        union_managed_desc()
    } else {
        union_plain_desc()
    };
    let bx = unsafe { gc::alloc(desc) };
    unsafe {
        (bx as *mut i64).write(tid);
        ((bx as usize + 8) as *mut i64).write(payload);
    }
    bx
}

/// Release the lock, clone `R` out *while still held*, and build the `Ready<…>`
/// result. `result_bits` is the body's value (possibly an alias into the cell);
/// `clone_fn` (if non-zero) detaches it before release so the returned value is
/// no longer aliased into the cell (`docs/20` §4 return-boundary clone-out). For
/// `try_lock` the value is first wrapped as the `R` variant of `R | LockBusy`.
unsafe fn finish_locked(data: *mut u8, id: u64, result_bits: i64, ready_tid: i64) -> *mut u8 {
    let clone_fn = unsafe { ((data as usize + 32) as *const i64).read() } as usize;
    let r_is_ptr = unsafe { ((data as usize + 48) as *const i64).read() } != 0;
    let is_try = unsafe { ((data as usize + 80) as *const i64).read() } != 0;
    let r_tid = unsafe { ((data as usize + 88) as *const i64).read() };

    // Clone-out under the lock. Pin the (possibly unrooted) source across the
    // clone, then pin the result across the release + box construction.
    if r_is_ptr {
        gc::add_extra_root(result_bits as usize);
    }
    let result = if clone_fn != 0 {
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(clone_fn) };
        f(result_bits)
    } else {
        result_bits
    };
    if r_is_ptr {
        gc::remove_extra_root(result_bits as usize);
        gc::add_extra_root(result as usize);
    }
    held_pop(id);
    release_inner(id);
    gc::pause();
    let bx = if is_try {
        // The Ready value is the `R | LockBusy` union (tagged as the `R` variant).
        let u = unsafe { union_box(r_tid, result, r_is_ptr) };
        unsafe { ready_value_box(ready_tid, u as i64, true) }
    } else {
        unsafe { ready_value_box(ready_tid, result, r_is_ptr) }
    };
    gc::resume_with_return_root(bx as usize);
    if r_is_ptr {
        gc::remove_extra_root(result as usize);
    }
    bx
}

/// `try_lock` lost the race: build `Ready<LockBusy>` (the `LockBusy` variant of
/// `R | LockBusy`). No lock is held, so nothing is released.
unsafe fn finish_busy(data: *mut u8, ready_tid: i64) -> *mut u8 {
    let busy_tid = unsafe { ((data as usize + 96) as *const i64).read() };
    gc::pause();
    let u = unsafe { union_box(busy_tid, 0, false) };
    let bx = unsafe { ready_value_box(ready_tid, u as i64, true) };
    gc::resume_with_return_root(bx as usize);
    bx
}

/// Run the body closure under the (just-acquired) lock and finish, driving an
/// async body's future to completion first. Shared by the `lock` and `try_lock`
/// acquisition paths.
unsafe fn run_body(data: *mut u8, id: u64, cell: &Cell, ready_tid: i64) -> *mut u8 {
    let value_bits = runtime_lock(&cell.state).value;
    let env = unsafe { ((data as usize + 16) as *const usize).read() } as *mut u8;
    let fn_ptr = unsafe { (env as *const usize).read() };
    let body: extern "C" fn(*mut u8, i64) -> i64 = unsafe { std::mem::transmute(fn_ptr) };
    let ret = body(env, value_bits);
    if unsafe { ((data as usize + 40) as *const i64).read() } == 0 {
        // Synchronous body: `ret` is `R`.
        return unsafe { finish_locked(data, id, ret, ready_tid) };
    }
    // Async body: store its future and mark running; the caller drives phase 2.
    unsafe {
        ((data as usize + 24) as *mut usize).write(ret as usize);
        ((data as usize + 0) as *mut i64).write(LST_RUNNING);
    }
    std::ptr::null_mut() // sentinel: "now in running phase"
}

/// `poll` for the lock future (`docs/20` §4). Phase 1 acquires the lock (`lock`:
/// FIFO, non-barging, suspending the task — never the thread — while contended;
/// `try_lock`: a single non-suspending attempt, resolving to `LockBusy` on
/// failure). On acquisition it runs the body closure under the lock. A
/// synchronous body finishes immediately; an `async` body's future is driven to
/// completion in phase 2 *with the lock still held* across its suspension points.
/// The lock is released only once the body's value is ready and cloned out.
extern "C" fn lock_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let state = unsafe { (data as *const i64).read() };
    let id = unsafe { ((data as usize + 8) as *const i64).read() } as u64;
    let ready_tid = unsafe { ((data as usize + 56) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 64) as *const i64).read() };
    let is_try = unsafe { ((data as usize + 80) as *const i64).read() } != 0;
    let cell = cell(id);

    if state == LST_ACQ_INIT || state == LST_ACQ_QUEUED {
        if is_try {
            // Single non-suspending attempt; never queue (no barging).
            let acquired = {
                let mut s = runtime_lock(&cell.state);
                if !s.locked && s.queue.is_empty() {
                    s.locked = true;
                    true
                } else {
                    false
                }
            };
            if !acquired {
                return unsafe { finish_busy(data, ready_tid) };
            }
            held_push(id);
            let r = unsafe { run_body(data, id, &cell, ready_tid) };
            if !r.is_null() {
                return r;
            }
            // else: fell through to running phase below
        } else {
            let ticket = unsafe { ((data as usize + 72) as *const i64).read() } as u64;
            let acq_st = if state == LST_ACQ_INIT {
                ST_INIT
            } else {
                ST_QUEUED
            };
            let c = unsafe { &*ctx };
            let (acquired, new_acq_st, new_ticket) = {
                let mut s = runtime_lock(&cell.state);
                advance(&mut s, acq_st, ticket, c.waker_data(), c.wake_fn())
            };
            unsafe { ((data as usize + 72) as *mut i64).write(new_ticket as i64) };
            if !acquired {
                let st = if new_acq_st == ST_QUEUED {
                    LST_ACQ_QUEUED
                } else {
                    LST_ACQ_INIT
                };
                unsafe { ((data as usize + 0) as *mut i64).write(st) };
                gc::pause();
                let p = unsafe { pending_box(pending_tid) };
                gc::resume_with_return_root(p as usize);
                return p;
            }
            held_push(id);
            let r = unsafe { run_body(data, id, &cell, ready_tid) };
            if !r.is_null() {
                return r;
            }
            // else: fell through to running phase below
        }
    }

    // Phase 2: drive the async body future while holding the lock.
    let inner = unsafe { ((data as usize + 24) as *const usize).read() } as *mut u8;
    let vtable = unsafe { (inner as *const usize).read() } as *const usize;
    let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
        unsafe { std::mem::transmute(vtable.read()) };
    let inner_data = unsafe { ((inner as usize + 8) as *const usize).read() } as *mut u8;
    let r = poll(inner_data, ctx);
    let tag = unsafe { (r as *const i64).read() };
    if tag == pending_tid {
        return r; // body suspended — lock remains held across the suspension
    }
    let ready_struct = unsafe { ((r as usize + 8) as *const usize).read() };
    let rv = unsafe { (ready_struct as *const i64).read() };
    unsafe { finish_locked(data, id, rv, ready_tid) }
}

/// Construct the lock future for `Shared<T>.lock(body)` / `.try_lock(body)`
/// (`docs/20` §4). The returned future acquires the lock (suspending the task
/// while contended for `lock`; resolving to `LockBusy` on a failed `try_lock`),
/// runs `env` (the body closure, called as `fn(env, value) -> R`) under the lock
/// — driving it to completion if `body_is_async` — clones `R` out via `clone_fn`
/// (0 if `R` needs no clone), releases, and resolves to `R` (or `R | LockBusy`).
///
/// # Safety
/// Callable only from generated code with the runtime initialised; `id` must be
/// a live `Shared` id and `env` a valid closure environment.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn lang_shared_lock_future(
    id: u64,
    env: *mut u8,
    clone_fn: usize,
    body_is_async: i64,
    r_is_ptr: i64,
    is_try: i64,
    r_tid: i64,
    busy_tid: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    // Pin `env` across the allocations below (it is unrooted until stored).
    gc::add_extra_root(env as usize);
    gc::pause();
    let data = unsafe { gc::alloc(lock_data_desc()) };
    unsafe {
        ((data as usize + 0) as *mut i64).write(LST_ACQ_INIT);
        ((data as usize + 8) as *mut i64).write(id as i64);
        ((data as usize + 16) as *mut usize).write(env as usize);
        ((data as usize + 24) as *mut usize).write(0); // inner_fut
        ((data as usize + 32) as *mut i64).write(clone_fn as i64);
        ((data as usize + 40) as *mut i64).write(body_is_async);
        ((data as usize + 48) as *mut i64).write(r_is_ptr);
        ((data as usize + 56) as *mut i64).write(ready_tid);
        ((data as usize + 64) as *mut i64).write(pending_tid);
        ((data as usize + 72) as *mut i64).write(0); // ticket
        ((data as usize + 80) as *mut i64).write(is_try);
        ((data as usize + 88) as *mut i64).write(r_tid);
        ((data as usize + 96) as *mut i64).write(busy_tid);
    }
    let bx = unsafe { gc::alloc(lock_box_desc()) };
    unsafe {
        (bx as *mut usize).write(lock_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(0);
    }
    gc::resume_with_return_root(bx as usize);
    gc::remove_extra_root(env as usize);
    bx
}

/// Try to acquire the lock without suspending (`try_lock`). Returns `1` and takes
/// the lock (recording it in the held-set) when it is free and uncontended;
/// returns `0` when the lock is busy or has queued waiters (no barging).
///
/// # Safety
/// `id` must be a live `Shared` id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_try_acquire(id: u64) -> i64 {
    let cell = cell(id);
    let mut s = runtime_lock(&cell.state);
    if !s.locked && s.queue.is_empty() {
        s.locked = true;
        drop(s);
        held_push(id);
        1
    } else {
        0
    }
}

/// Read the protected value word of a currently-held lock.
///
/// # Safety
/// `id` must be a live `Shared` id whose lock is held by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_read(id: u64) -> i64 {
    runtime_lock(&cell(id).state).value
}

/// Internal: clear the locked flag and wake the FIFO head waiter. Does not touch
/// the held-set.
fn release_inner(id: u64) {
    let cell = cell(id);
    let head = {
        let mut s = runtime_lock(&cell.state);
        s.locked = false;
        prune_dead_front(&mut s);
        s.queue.front().copied()
    };
    if let Some(w) = head {
        (w.wake)(w.data);
    }
}

/// Release a lock held by the calling task. Clears the flag, removes it from the
/// held-set, and wakes queued waiters so acquisition can resume on re-poll.
///
/// # Safety
/// `id` must be a live `Shared` id currently held by this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_release(id: u64) {
    held_pop(id);
    release_inner(id);
}

/// Release EVERY lock the calling task still holds (panic-boundary / cancellation
/// cleanup). Drains the held-set, releasing each in LIFO order.
///
/// # Safety
/// Callable only from the runtime's worker panic boundary or a future's cancel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_shared_release_all() {
    let held: Vec<u64> = TASK_HELD
        .with(|h| {
            h.borrow()
                .as_ref()
                .map(|task| std::mem::take(&mut *runtime_lock(task)))
        })
        .unwrap_or_else(|| HELD.with(|h| std::mem::take(&mut *h.borrow_mut())));
    for id in held.into_iter().rev() {
        release_inner(id);
    }
}

/// Release every lock held by an executor task that is being cancelled while it
/// is suspended. This is the task-local equivalent of
/// [`lang_shared_release_all`], used by the M:N executor when there is no
/// current polling thread to host `TASK_HELD`.
pub fn release_task_held(held: &Arc<Mutex<Vec<u64>>>) {
    let held = std::mem::take(&mut *runtime_lock(held));
    for id in held.into_iter().rev() {
        release_inner(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn wake_noop(_: *mut u8) {}

    fn fresh_state() -> State {
        State {
            locked: false,
            value: 0,
            queue: VecDeque::new(),
            next_ticket: 1,
        }
    }

    /// Register a cell directly (no GC) so the registry-backed entry points are
    /// testable without initialising the collector.
    fn register_cell() -> u64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        runtime_lock(registry()).insert(
            id,
            std::sync::Arc::new(Cell {
                state: Mutex::new(fresh_state()),
            }),
        );
        id
    }

    #[test]
    fn registry_lock_recovers_after_poison() {
        let h = std::thread::spawn(|| {
            let _guard = registry().lock().unwrap();
            panic!("poison shared registry");
        });
        assert!(h.join().is_err());

        let id = register_cell();
        assert_eq!(unsafe { lang_shared_try_acquire(id) }, 1);
        assert_eq!(unsafe { lang_shared_read(id) }, 0);
        unsafe { lang_shared_release(id) };
    }

    #[test]
    fn cell_state_lock_recovers_after_poison() {
        let id = register_cell();
        let cell = cell(id);
        let h = std::thread::spawn(move || {
            let _guard = cell.state.lock().unwrap();
            panic!("poison shared cell state");
        });
        assert!(h.join().is_err());

        assert_eq!(unsafe { lang_shared_try_acquire(id) }, 1);
        assert_eq!(unsafe { lang_shared_read(id) }, 0);
        unsafe { lang_shared_release(id) };
        assert_eq!(unsafe { lang_shared_try_acquire(id) }, 1);
        unsafe { lang_shared_release(id) };
    }

    #[test]
    fn uncontended_acquire_then_busy_then_release() {
        let mut s = fresh_state();
        // First acquirer takes it immediately.
        let (a, st, _) = advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        assert!(a && st == ST_ACQUIRED && s.locked);
        // A second INIT poll while held queues (does not barge).
        let (b, stb, tb) = advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        assert!(!b && stb == ST_QUEUED && tb == 1 && s.queue.len() == 1);
    }

    #[test]
    fn fifo_order_and_no_barging() {
        let mut s = fresh_state();
        // A acquires.
        advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        // B, C queue with tickets 1, 2.
        let (_, _, tb) = advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        let (_, _, tc) = advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        assert_eq!((tb, tc), (1, 2));
        // A releases.
        s.locked = false;
        // C re-polls first but is NOT at the head → stays pending.
        let (c2, _, _) = advance(&mut s, ST_QUEUED, tc, std::ptr::null_mut(), wake_noop);
        assert!(!c2, "ticket 2 must wait behind ticket 1");
        // B (head) re-polls → acquires; C now becomes head.
        let (b2, stb, _) = advance(&mut s, ST_QUEUED, tb, std::ptr::null_mut(), wake_noop);
        assert!(b2 && stb == ST_ACQUIRED);
        // A fresh INIT poll while the queue is non-empty must not barge ahead.
        let (d, _, td) = advance(&mut s, ST_INIT, 0, std::ptr::null_mut(), wake_noop);
        assert!(!d && td == 3, "newcomer queues behind existing waiters");
    }

    #[test]
    fn queued_poll_with_missing_ticket_reenters_fifo() {
        let mut s = fresh_state();
        s.locked = true;
        s.next_ticket = 7;

        let (acquired, st, ticket) =
            advance(&mut s, ST_QUEUED, 42, std::ptr::null_mut(), wake_noop);
        assert!(!acquired);
        assert_eq!(st, ST_QUEUED);
        assert_eq!(ticket, 7);
        assert_eq!(s.queue.front().map(|w| w.ticket), Some(7));

        s.locked = false;
        s.queue.clear();
        let (acquired, st, _) = advance(&mut s, ST_QUEUED, 99, std::ptr::null_mut(), wake_noop);
        assert!(acquired);
        assert_eq!(st, ST_ACQUIRED);
    }

    #[test]
    fn init_poll_acquires_after_removing_own_stale_waiter() {
        let mut s = fresh_state();
        let (data, wake) = crate::threads::test_executor_waker();
        s.queue.push_back(Waker {
            data,
            wake,
            ticket: 1,
        });
        s.next_ticket = 2;

        let (acquired, st, _) = advance(&mut s, ST_INIT, 0, data, wake);

        assert!(
            acquired,
            "free lock should not park behind its own stale waiter"
        );
        assert_eq!(st, ST_ACQUIRED);
        assert!(s.queue.is_empty());
        assert!(s.locked);
    }

    #[test]
    fn try_acquire_release_roundtrip() {
        let id = register_cell();
        assert_eq!(unsafe { lang_shared_try_acquire(id) }, 1, "free → acquired");
        assert_eq!(
            unsafe { lang_shared_try_acquire(id) },
            0,
            "held → busy (non-reentrant)"
        );
        unsafe { lang_shared_release(id) };
        assert_eq!(
            unsafe { lang_shared_try_acquire(id) },
            1,
            "released → re-acquirable"
        );
        unsafe { lang_shared_release(id) };
    }

    #[test]
    fn release_all_drains_held_set() {
        let a = register_cell();
        let b = register_cell();
        assert_eq!(unsafe { lang_shared_try_acquire(a) }, 1);
        assert_eq!(unsafe { lang_shared_try_acquire(b) }, 1);
        // Simulate a panic/cancel: drain everything this task holds.
        unsafe { lang_shared_release_all() };
        assert_eq!(unsafe { lang_shared_try_acquire(a) }, 1, "a was released");
        assert_eq!(unsafe { lang_shared_try_acquire(b) }, 1, "b was released");
        assert!(
            HELD.with(|h| h.borrow().len() == 2),
            "held-set tracks the re-acquires"
        );
        unsafe { lang_shared_release_all() };
    }

    /// Releasing wakes the live FIFO head and leaves the queue intact; the head
    /// entry is consumed by `advance` when that task re-polls.
    #[test]
    fn release_wakes_live_head_without_consuming_ticket() {
        use std::sync::atomic::AtomicU32;
        static WOKEN: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(_: *mut u8) {
            WOKEN.fetch_add(1, Ordering::SeqCst);
        }
        WOKEN.store(0, Ordering::SeqCst);
        let id = register_cell();
        {
            let cell = cell(id);
            let mut s = cell.state.lock().unwrap();
            s.locked = true;
            s.queue.push_back(Waker {
                data: std::ptr::null_mut(),
                wake: count_wake,
                ticket: 1,
            });
            s.queue.push_back(Waker {
                data: std::ptr::null_mut(),
                wake: count_wake,
                ticket: 2,
            });
        }
        unsafe { lang_shared_release(id) };
        assert_eq!(
            WOKEN.load(Ordering::SeqCst),
            1,
            "only the FIFO head is woken, avoiding a thundering herd under contention"
        );
        assert!(
            cell(id).state.lock().unwrap().queue.len() == 2,
            "release preserves tickets until re-poll"
        );
    }
}
