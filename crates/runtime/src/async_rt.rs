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
//! callback or the shared timer driver); a future that returns `Pending` without ever
//! waking is a forever-hung task, exactly as the spec warns.

use crate::gc;
use crate::strings::{LangStr, lang_str_from_utf8, str_bytes};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, WaitTimeoutResult};

/// The waker context handed to `poll` (`docs/21` §2). Layout matches the
/// language `extern struct Context { waker_data: *u8, wake_fn: extern (*u8) =>
/// null }` so a C event loop can supply the callback natively.
#[repr(C)]
pub struct Context {
    waker_data: *mut u8,
    wake_fn: extern "C" fn(*mut u8),
}

impl Context {
    /// The opaque waker payload to hand back to [`Self::wake_fn`].
    pub fn waker_data(&self) -> *mut u8 {
        self.waker_data
    }
    /// The callback that schedules a re-poll of the awaiting task.
    pub fn wake_fn(&self) -> extern "C" fn(*mut u8) {
        self.wake_fn
    }
}

/// A parked-thread waker: a flag the future sets (via [`wake_thunk`]) and a
/// condvar the blocked executor waits on.
struct ThreadWaker {
    woken: Mutex<bool>,
    cv: Condvar,
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn wait_unpoison<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(|err| err.into_inner())
}

fn wait_timeout_unpoison<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    dur: Duration,
) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
    cv.wait_timeout(guard, dur)
        .unwrap_or_else(|err| err.into_inner())
}

/// The `wake_fn` installed in the [`Context`] for [`lang_block_on`]. `data`
/// points at the executor's [`ThreadWaker`]; waking sets the flag and signals
/// the condvar so a parked `block_on` re-polls.
extern "C" fn wake_thunk(data: *mut u8) {
    let w = unsafe { &*(data as *const ThreadWaker) };
    *lock_unpoison(&w.woken) = true;
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
    // struct) so a collection triggered inside `poll` cannot free it. The pin is
    // unwind-scoped: if a `poll` panics on a worker, the panic boundary's
    // `release_unwind_pins` drops it (the `longjmp` skips the removal below).
    gc::pin_for_unwind(fut as usize);

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
        let mut woken = lock_unpoison(&waker.woken);
        while !*woken {
            woken = wait_unpoison(&waker.cv, woken);
        }
        *woken = false;
        drop(woken);
        gc::leave_native();
    };

    gc::unpin_for_unwind(fut as usize);
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
    bytes.extend_from_slice(&0u32.to_le_bytes()); // n_ep = 0 (no endpoint fields)
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
        // Schedule an immediate re-poll, then report Pending.
        unsafe { (data as *mut i64).write(1) };
        let c = unsafe { &*ctx };
        (c.wake_fn)(c.waker_data);
        let bx = unsafe { gc::alloc(union_plain_desc()) };
        unsafe { (bx as *mut i64).write(pending_tid) };
        bx
    };
    gc::resume_with_return_root(result as usize);
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

// -- reactor core: one-shot readiness registrations for future I/O -----------

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct ReactorRegistration {
    id: u64,
    active: Arc<AtomicBool>,
}

impl ReactorRegistration {
    /// Opaque registration id passed to host/platform I/O readiness callbacks.
    pub fn id(&self) -> u64 {
        self.id
    }
}

struct ReactorWaiter {
    active: Arc<AtomicBool>,
    waker_data: usize,
    wake_fn: extern "C" fn(*mut u8),
}

struct Reactor {
    waiters: Mutex<HashMap<u64, ReactorWaiter>>,
    next: AtomicU64,
}

fn reactor() -> &'static Reactor {
    static R: OnceLock<Reactor> = OnceLock::new();
    R.get_or_init(|| Reactor {
        waiters: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    })
}

#[cfg(test)]
fn reactor_has_waiter(id: u64) -> bool {
    lock_unpoison(&reactor().waiters).contains_key(&id)
}

/// Register a one-shot readiness waiter with the shared async reactor.
///
/// This is the executor-facing core future I/O backends use after returning
/// `Pending`: when the OS/backend reports readiness it calls
/// [`reactor_wake_ready`], which removes the registration and invokes the
/// captured task waker outside the reactor mutex. The handle may be cancelled
/// synchronously by the future's cancellation/drop path.
pub fn reactor_register(waker_data: usize, wake_fn: extern "C" fn(*mut u8)) -> ReactorRegistration {
    let r = reactor();
    let id = r.next.fetch_add(1, Ordering::Relaxed);
    let active = Arc::new(AtomicBool::new(true));
    lock_unpoison(&r.waiters).insert(
        id,
        ReactorWaiter {
            active: active.clone(),
            waker_data,
            wake_fn,
        },
    );
    ReactorRegistration { id, active }
}

/// Cancel a pending reactor registration. Returns true only if this call won
/// the race against readiness delivery.
pub fn reactor_cancel(reg: &ReactorRegistration) -> bool {
    if !reg.active.swap(false, Ordering::SeqCst) {
        return false;
    }
    lock_unpoison(&reactor().waiters).remove(&reg.id);
    true
}

/// Deliver readiness for a previously registered one-shot I/O waiter.
///
/// Returns true when a live registration was found and woken. Stale readiness
/// notifications are harmless and return false.
pub fn reactor_wake_ready(id: u64) -> bool {
    let waiter = lock_unpoison(&reactor().waiters).remove(&id);
    let Some(waiter) = waiter else {
        return false;
    };
    if !waiter.active.swap(false, Ordering::SeqCst) {
        return false;
    }
    (waiter.wake_fn)(waiter.waker_data as *mut u8);
    true
}

/// C ABI hook for host/platform I/O backends that report readiness from outside
/// Rust runtime code.
#[unsafe(no_mangle)]
pub extern "C" fn lang_async_reactor_wake(id: u64) -> bool {
    reactor_wake_ready(id)
}

// -- sleep: a future that completes after a delay ----------------------------

#[derive(Clone)]
struct TimerToken {
    fired: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    registration: Arc<Mutex<Option<ReactorRegistration>>>,
}

impl TimerToken {
    fn new() -> Self {
        Self {
            fired: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            registration: Arc::new(Mutex::new(None)),
        }
    }

    fn arm_reactor(&self, waker_data: usize, wake_fn: extern "C" fn(*mut u8)) -> bool {
        let mut slot = lock_unpoison(&self.registration);
        if !self.active.load(Ordering::SeqCst) {
            return false;
        }
        let reg = reactor_register(waker_data, wake_fn);
        if !self.active.load(Ordering::SeqCst) {
            reactor_cancel(&reg);
            return false;
        }
        *slot = Some(reg);
        true
    }

    fn cancel(&self) {
        self.active.store(false, Ordering::SeqCst);
        if let Some(reg) = lock_unpoison(&self.registration).take() {
            reactor_cancel(&reg);
        }
    }

    fn take_registration(&self) -> Option<ReactorRegistration> {
        lock_unpoison(&self.registration).take()
    }
}

struct TimerEntry {
    at: Instant,
    seq: u64,
    token: TimerToken,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Reverse ordering: BinaryHeap pops the earliest deadline first.
        other
            .at
            .cmp(&self.at)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

struct TimerDriver {
    heap: Mutex<BinaryHeap<TimerEntry>>,
    cv: Condvar,
    seq: AtomicU64,
}

fn timer_driver() -> &'static TimerDriver {
    static DRIVER: OnceLock<TimerDriver> = OnceLock::new();
    let driver = DRIVER.get_or_init(|| TimerDriver {
        heap: Mutex::new(BinaryHeap::new()),
        cv: Condvar::new(),
        seq: AtomicU64::new(1),
    });
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| timer_loop(timer_driver()));
    });
    driver
}

fn schedule_timer(ms: i64, token: TimerToken, waker_data: usize, wake_fn: extern "C" fn(*mut u8)) {
    if !token.arm_reactor(waker_data, wake_fn) {
        return;
    }
    let driver = timer_driver();
    let delay = Duration::from_millis(ms.max(0) as u64);
    let entry = TimerEntry {
        at: Instant::now() + delay,
        seq: driver.seq.fetch_add(1, Ordering::Relaxed),
        token,
    };
    let mut heap = lock_unpoison(&driver.heap);
    heap.push(entry);
    drop(heap);
    driver.cv.notify_one();
}

fn timer_loop(driver: &'static TimerDriver) {
    let mut heap = lock_unpoison(&driver.heap);
    loop {
        let Some(next) = heap.peek() else {
            heap = wait_unpoison(&driver.cv, heap);
            continue;
        };
        let now = Instant::now();
        if next.at > now {
            let wait = next.at.duration_since(now);
            let (g, _) = wait_timeout_unpoison(&driver.cv, heap, wait);
            heap = g;
            continue;
        }
        let entry = heap.pop().unwrap();
        drop(heap);
        if entry.token.active.swap(false, Ordering::SeqCst) {
            entry.token.fired.store(true, Ordering::SeqCst);
            if let Some(reg) = entry.token.take_registration() {
                reactor_wake_ready(reg.id());
            }
        } else if let Some(reg) = entry.token.take_registration() {
            reactor_cancel(&reg);
        }
        heap = lock_unpoison(&driver.heap);
    }
}

/// Per-`sleep` timer state, keyed by id in [`sleep_registry`].
struct SleepCell {
    ms: i64,
    started: AtomicBool,
    token: TimerToken,
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

const SLEEP_FUTURE_KIND: i64 = -0x5150_534c_4545_5001i64;
const TIMEOUT_FUTURE_KIND: i64 = -0x5150_5449_4d45_4f01i64;
const IO_FUTURE_KIND: i64 = -0x5150_494f_4655_5401i64;

extern "C" fn sleep_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let ready_tid = unsafe { (data as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let id = unsafe { ((data as usize + 16) as *const i64).read() } as u64;
    let cell = lock_unpoison(sleep_registry()).get(&id).cloned();
    let Some(cell) = cell else {
        // Unknown id — treat as already elapsed.
        gc::pause();
        let r = unsafe { ready_null_box(ready_tid) };
        gc::resume_with_return_root(r as usize);
        return r;
    };
    if cell.token.fired.load(Ordering::SeqCst) {
        lock_unpoison(sleep_registry()).remove(&id);
        gc::pause();
        let r = unsafe { ready_null_box(ready_tid) };
        gc::resume_with_return_root(r as usize);
        return r;
    }
    if !cell.started.swap(true, Ordering::SeqCst) {
        // First poll: arm the shared timer driver and report Pending.
        let c = unsafe { &*ctx };
        schedule_timer(
            cell.ms,
            cell.token.clone(),
            c.waker_data as usize,
            c.wake_fn,
        );
    }
    gc::pause();
    let r = unsafe { pending_box(pending_tid) };
    gc::resume_with_return_root(r as usize);
    r
}

/// Construct a `sleep(ms)` future (`docs/21` §9 helper) — a `Future<null>` that
/// completes after `ms` milliseconds, waking the executor via the shared timer
/// driver.
///
/// # Safety
/// Callable only from generated code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_sleep(ms: i64, ready_tid: i64, pending_tid: i64) -> *mut u8 {
    let id = SLEEP_NEXT.fetch_add(1, Ordering::Relaxed);
    lock_unpoison(sleep_registry()).insert(
        id,
        std::sync::Arc::new(SleepCell {
            ms,
            started: AtomicBool::new(false),
            token: TimerToken::new(),
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
        ((bx as usize + 16) as *mut i64).write(SLEEP_FUTURE_KIND);
    }
    gc::resume_with_return_root(bx as usize);
    bx
}

// -- timeout: race a future against a deadline -------------------------------
//
// `timeout(fut, ms): Future<T | TimedOut>` polls `fut`; if it is ready the value
// is reboxed as the `T` variant of `T | TimedOut`, otherwise once `ms` elapses
// the future resolves to `TimedOut` and calls the same cancellation hook as
// explicit `fut.cancel()`. The code generator supplies `t_id` / `t_is_ptr` (so
// the runtime can build the `T`-variant box and the collector can trace its
// payload) plus the `TimedOut` / `Ready` / `Pending` type ids.

struct TimeoutCell {
    ms: i64,
    started: AtomicBool,
    token: TimerToken,
    cancelled_loser: AtomicBool,
}
fn timeout_registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<TimeoutCell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<TimeoutCell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
static TIMEOUT_NEXT: AtomicU64 = AtomicU64::new(1);

fn timeout_data_desc() -> *const u8 {
    // [inner_fut @0 (managed)][ready_tid][pending_tid][t_id][t_is_ptr]
    // [t_is_union][timedout_tid][id] — only the inner future is a managed pointer.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(64, &[0]) as usize) as *const u8
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

unsafe fn ready_managed_value_box(value: usize, ready_tid: i64) -> *mut u8 {
    let ready = unsafe { gc::alloc(value_desc_ptr()) };
    unsafe { (ready as *mut usize).write(value) };
    let outer = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (outer as *mut i64).write(ready_tid);
        ((outer as usize + 8) as *mut usize).write(ready as usize);
    }
    outer
}

extern "C" fn timeout_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let inner_fut = unsafe { (data as *const usize).read() } as *mut u8;
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let t_id = unsafe { ((data as usize + 24) as *const i64).read() };
    let t_is_ptr = unsafe { ((data as usize + 32) as *const i64).read() };
    let t_is_union = unsafe { ((data as usize + 40) as *const i64).read() } != 0;
    let timedout_tid = unsafe { ((data as usize + 48) as *const i64).read() };
    let id = unsafe { ((data as usize + 56) as *const i64).read() } as u64;

    // Poll the inner future once.
    let vtable = unsafe { (inner_fut as *const usize).read() } as *const usize;
    let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
        unsafe { std::mem::transmute(vtable.read()) };
    let inner_data = unsafe { ((inner_fut as usize + 8) as *const usize).read() } as *mut u8;
    let r = poll(inner_data, ctx);
    let tag = unsafe { (r as *const i64).read() };

    if tag != pending_tid {
        // Inner ready: rebox its value as the `T` variant of `T | TimedOut`.
        // If `T` is already a union/dynamic box, the surface `T | TimedOut`
        // type is flattened by sema, so the inner union box is already the
        // correct success value and must be passed through as-is.
        if let Some(cell) = lock_unpoison(timeout_registry()).remove(&id) {
            cell.token.cancel();
        }
        let ready_struct = unsafe { ((r as usize + 8) as *const usize).read() };
        let value = unsafe { (ready_struct as *const i64).read() };
        gc::pause();
        let tbox = if t_is_union {
            value as *mut u8
        } else {
            let tbox = unsafe {
                gc::alloc(if t_is_ptr != 0 {
                    union_managed_desc()
                } else {
                    union_plain_desc()
                })
            };
            unsafe {
                (tbox as *mut i64).write(t_id);
                ((tbox as usize + 8) as *mut i64).write(value);
            }
            tbox
        };
        let out = unsafe { wrap_ready(tbox, ready_tid) };
        gc::resume_with_return_root(out as usize);
        return out;
    }

    // Inner pending: arm the deadline timer on first poll, then report
    // `TimedOut` once it fires (the inner future's own waker covers the
    // ready case; either wake re-polls us).
    let cell = lock_unpoison(timeout_registry()).get(&id).cloned();
    if let Some(cell) = cell {
        if cell.token.fired.load(Ordering::SeqCst) {
            if !cell.cancelled_loser.swap(true, Ordering::SeqCst) {
                unsafe { crate::threads::lang_future_cancel(inner_fut) };
                lock_unpoison(timeout_registry()).remove(&id);
            }
            gc::pause();
            let tbox = unsafe { gc::alloc(union_plain_desc()) };
            unsafe { (tbox as *mut i64).write(timedout_tid) };
            let out = unsafe { wrap_ready(tbox, ready_tid) };
            gc::resume_with_return_root(out as usize);
            return out;
        }
        if !cell.started.swap(true, Ordering::SeqCst) {
            let c = unsafe { &*ctx };
            schedule_timer(
                cell.ms,
                cell.token.clone(),
                c.waker_data as usize,
                c.wake_fn,
            );
        }
    }
    gc::pause();
    let p = unsafe { pending_box(pending_tid) };
    gc::resume_with_return_root(p as usize);
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
    t_is_union: i64,
    timedout_tid: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let id = TIMEOUT_NEXT.fetch_add(1, Ordering::Relaxed);
    lock_unpoison(timeout_registry()).insert(
        id,
        std::sync::Arc::new(TimeoutCell {
            ms,
            started: AtomicBool::new(false),
            token: TimerToken::new(),
            cancelled_loser: AtomicBool::new(false),
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
        ((data as usize + 40) as *mut i64).write(t_is_union);
        ((data as usize + 48) as *mut i64).write(timedout_tid);
        ((data as usize + 56) as *mut i64).write(id as i64);
    }
    let bx = unsafe { gc::alloc(yield_box_desc()) };
    unsafe {
        (bx as *mut usize).write(timeout_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(TIMEOUT_FUTURE_KIND);
    }
    gc::resume_with_return_root(bx as usize);
    gc::remove_extra_root(fut as usize);
    bx
}

// -- std:io async futures ---------------------------------------------------
//
// Portable stdio is not readiness-based on every target. The executor-facing
// contract still must not block a worker poll, so these futures hand the
// blocking stream operation to a helper thread and use the same cancellable
// reactor registration as timers/I/O readiness to wake the parked task.

#[derive(Clone)]
enum IoOp {
    StdinRead { count: i64 },
    StdinReadToEnd,
    StdoutWrite { contents_hex: String },
    StderrWrite { contents_hex: String },
    StdoutFlush,
    StderrFlush,
}

struct IoCell {
    op: IoOp,
    started: AtomicBool,
    cancelled: AtomicBool,
    result: Mutex<Option<usize>>,
    registration: Mutex<Option<ReactorRegistration>>,
}

fn io_registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<IoCell>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<IoCell>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static IO_NEXT: AtomicU64 = AtomicU64::new(1);

fn io_data_desc() -> *const u8 {
    // [ready_tid][pending_tid][id] — the blocking payload is copied into the
    // Rust-side cell so no managed field needs tracing here.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[]) as usize) as *const u8
}

fn io_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = io_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

fn io_run(op: &IoOp) -> String {
    match op {
        IoOp::StdinRead { count } => crate::strings::io_read_stdin_count_encoded(*count),
        IoOp::StdinReadToEnd => crate::strings::io_read_stdin_to_end_encoded(),
        IoOp::StdoutWrite { contents_hex } => {
            crate::strings::io_write_stream_bytes_encoded(contents_hex, false)
        }
        IoOp::StderrWrite { contents_hex } => {
            crate::strings::io_write_stream_bytes_encoded(contents_hex, true)
        }
        IoOp::StdoutFlush => crate::strings::io_flush_stream_encoded(false),
        IoOp::StderrFlush => crate::strings::io_flush_stream_encoded(true),
    }
}

fn io_complete(cell: std::sync::Arc<IoCell>, encoded: String) {
    gc::leave_native();
    let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
    gc::add_extra_root(ptr);
    let reg = {
        let mut result = lock_unpoison(&cell.result);
        if cell.cancelled.load(Ordering::SeqCst) {
            gc::remove_extra_root(ptr);
            return;
        }
        *result = Some(ptr);
        lock_unpoison(&cell.registration).take()
    };
    if let Some(reg) = reg {
        reactor_wake_ready(reg.id());
    }
}

fn io_spawn_worker(cell: std::sync::Arc<IoCell>) {
    std::thread::spawn(move || {
        gc::thread_start();
        gc::enter_runtime_native_no_roots();
        let encoded = io_run(&cell.op);
        io_complete(cell, encoded);
    });
}

fn io_cancel_id(id: u64) {
    if let Some(cell) = lock_unpoison(io_registry()).remove(&id) {
        cell.cancelled.store(true, Ordering::SeqCst);
        if let Some(reg) = lock_unpoison(&cell.registration).take() {
            reactor_cancel(&reg);
        }
        if let Some(ptr) = lock_unpoison(&cell.result).take() {
            gc::remove_extra_root(ptr);
        }
    }
}

extern "C" fn io_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    let ready_tid = unsafe { (data as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let id = unsafe { ((data as usize + 16) as *const i64).read() } as u64;
    let cell = lock_unpoison(io_registry()).get(&id).cloned();
    let Some(cell) = cell else {
        gc::pause();
        let r = unsafe { ready_managed_value_box(0, ready_tid) };
        gc::resume_with_return_root(r as usize);
        return r;
    };

    if let Some(ptr) = lock_unpoison(&cell.result).take() {
        lock_unpoison(io_registry()).remove(&id);
        gc::pause();
        let r = unsafe { ready_managed_value_box(ptr, ready_tid) };
        gc::remove_extra_root(ptr);
        gc::resume_with_return_root(r as usize);
        return r;
    }

    if !cell.started.swap(true, Ordering::SeqCst) {
        let c = unsafe { &*ctx };
        let reg = reactor_register(c.waker_data as usize, c.wake_fn);
        if cell.cancelled.load(Ordering::SeqCst) {
            reactor_cancel(&reg);
        } else {
            *lock_unpoison(&cell.registration) = Some(reg);
            io_spawn_worker(cell.clone());
        }
    }

    gc::pause();
    let p = unsafe { pending_box(pending_tid) };
    gc::resume_with_return_root(p as usize);
    p
}

fn lang_io_future(op: IoOp, ready_tid: i64, pending_tid: i64) -> *mut u8 {
    let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
    lock_unpoison(io_registry()).insert(
        id,
        std::sync::Arc::new(IoCell {
            op,
            started: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(None),
            registration: Mutex::new(None),
        }),
    );
    gc::pause();
    let data = unsafe { gc::alloc(io_data_desc()) };
    unsafe {
        (data as *mut i64).write(ready_tid);
        ((data as usize + 8) as *mut i64).write(pending_tid);
        ((data as usize + 16) as *mut i64).write(id as i64);
    }
    let bx = unsafe { gc::alloc(yield_box_desc()) };
    unsafe {
        (bx as *mut usize).write(io_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(IO_FUTURE_KIND);
    }
    gc::resume_with_return_root(bx as usize);
    bx
}

/// Build a reactor-backed future for reading up to `count` stdin bytes.
#[unsafe(no_mangle)]
pub extern "C" fn lang_io_stdin_read_async(
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::StdinRead { count }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for reading all remaining stdin bytes.
#[unsafe(no_mangle)]
pub extern "C" fn lang_io_stdin_read_to_end_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::StdinReadToEnd, ready_tid, pending_tid)
}

/// Build a reactor-backed future for writing a hex byte payload to stdout.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_io_stdout_write_async(
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    lang_io_future(IoOp::StdoutWrite { contents_hex }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for writing a hex byte payload to stderr.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_io_stderr_write_async(
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    lang_io_future(IoOp::StderrWrite { contents_hex }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for flushing stdout.
#[unsafe(no_mangle)]
pub extern "C" fn lang_io_stdout_flush_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::StdoutFlush, ready_tid, pending_tid)
}

/// Build a reactor-backed future for flushing stderr.
#[unsafe(no_mangle)]
pub extern "C" fn lang_io_stderr_flush_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::StderrFlush, ready_tid, pending_tid)
}

/// Cancel runtime-built async futures that own reactor registrations.
///
/// Generated async state machines use their own cleanup hook at the future-box
/// metadata word. `sleep` and `timeout` are hand-built runtime futures, so they
/// use private negative markers in the same word and clean their reactor/timer
/// registration here when `future.cancel()` reaches them.
pub(crate) unsafe fn cancel_runtime_future(fut: *mut u8) -> bool {
    if fut.is_null() {
        return false;
    }
    let kind = unsafe { ((fut as usize + 16) as *const i64).read() };
    let data = unsafe { ((fut as usize + 8) as *const usize).read() };
    match kind {
        SLEEP_FUTURE_KIND => {
            let id = unsafe { ((data + 16) as *const i64).read() } as u64;
            if let Some(cell) = lock_unpoison(sleep_registry()).remove(&id) {
                cell.token.cancel();
            }
            true
        }
        TIMEOUT_FUTURE_KIND => {
            let inner_fut = unsafe { (data as *const usize).read() } as *mut u8;
            let id = unsafe { ((data + 56) as *const i64).read() } as u64;
            if let Some(cell) = lock_unpoison(timeout_registry()).remove(&id) {
                cell.token.cancel();
                if !cell.cancelled_loser.swap(true, Ordering::SeqCst) {
                    unsafe { crate::threads::lang_future_cancel(inner_fut) };
                }
            }
            true
        }
        IO_FUTURE_KIND => {
            let id = unsafe { ((data + 16) as *const i64).read() } as u64;
            io_cancel_id(id);
            true
        }
        crate::channels::CHAN_RECV_FUTURE_KIND => unsafe {
            crate::channels::cancel_recv_future(fut)
        },
        _ => false,
    }
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
    gc::resume_with_return_root(bx as usize);
    bx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering as AtomicOrdering;

    struct CountWaker {
        count: Mutex<u32>,
        cv: Condvar,
    }

    extern "C" fn count_wake(data: *mut u8) {
        let w = unsafe { &*(data as *const CountWaker) };
        let mut count = w.count.lock().unwrap();
        *count += 1;
        w.cv.notify_all();
    }

    struct ReentrantWaker {
        count: Mutex<u32>,
    }

    extern "C" fn reentrant_wake(data: *mut u8) {
        let w = unsafe { &*(data as *const ReentrantWaker) };
        *w.count.lock().unwrap() += 1;
        let reg = reactor_register(data as usize, reentrant_wake);
        assert!(reactor_cancel(&reg));
    }

    #[test]
    fn runtime_lock_helpers_recover_poisoned_mutexes() {
        let mutex = Arc::new(Mutex::new(41));
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poisoned = std::panic::catch_unwind({
            let mutex = mutex.clone();
            move || {
                let mut value = mutex.lock().unwrap();
                *value += 1;
                panic!("poison this test mutex");
            }
        });
        std::panic::set_hook(previous_hook);
        assert!(poisoned.is_err());

        let mut value = lock_unpoison(&mutex);
        assert_eq!(*value, 42);
        *value += 1;
        assert_eq!(*value, 43);
    }

    fn poll_future_once(fut: *mut u8, waker: &CountWaker) -> *mut u8 {
        let vtable = unsafe { (fut as *const usize).read() } as *const usize;
        let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
            unsafe { std::mem::transmute(vtable.read()) };
        let data = unsafe { ((fut as usize + 8) as *const usize).read() } as *mut u8;
        let mut ctx = Context {
            waker_data: waker as *const CountWaker as *mut u8,
            wake_fn: count_wake,
        };
        poll(data, &mut ctx)
    }

    fn future_data_word(fut: *mut u8, offset: usize) -> i64 {
        let data = unsafe { ((fut as usize + 8) as *const usize).read() };
        unsafe { ((data + offset) as *const i64).read() }
    }

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
        let vtable: Box<[usize; 1]> = Box::new([ready_poll as *const () as usize]);
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
        let vtable: Box<[usize; 1]> = Box::new([yield_once_poll as *const () as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data_ptr = Box::into_raw(Box::new([0i64; 1])) as usize;
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        let fut_ptr = Box::into_raw(fut) as *mut u8;
        let out = unsafe { lang_block_on(fut_ptr, 9) };
        assert_eq!(out, 7);
        assert_eq!(POLLS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn shared_timer_wakes_via_reactor_and_skips_cancelled_entries() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let live_a = TimerToken::new();
        let live_b = TimerToken::new();
        let cancelled = TimerToken::new();
        cancelled.cancel();

        schedule_timer(
            1,
            live_a.clone(),
            waker as *const CountWaker as usize,
            count_wake,
        );
        schedule_timer(
            1,
            live_b.clone(),
            waker as *const CountWaker as usize,
            count_wake,
        );
        schedule_timer(
            1,
            cancelled.clone(),
            waker as *const CountWaker as usize,
            count_wake,
        );
        assert!(
            cancelled.take_registration().is_none(),
            "cancelled timer must not leave a reactor registration behind"
        );

        let mut count = waker.count.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while *count < 2 {
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "timer driver did not wake both live entries"
            );
            let wait = deadline.duration_since(now);
            let (g, _) = waker.cv.wait_timeout(count, wait).unwrap();
            count = g;
        }
        assert_eq!(*count, 2);
        assert!(live_a.fired.load(AtomicOrdering::SeqCst));
        assert!(live_b.fired.load(AtomicOrdering::SeqCst));
        assert!(!cancelled.fired.load(AtomicOrdering::SeqCst));
    }

    #[test]
    fn reactor_readiness_wakes_registered_waiter_once() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);

        assert!(reactor_wake_ready(reg.id()));
        assert!(!reactor_wake_ready(reg.id()));
        assert!(!reactor_cancel(&reg));

        let count = waker.count.lock().unwrap();
        assert_eq!(*count, 1);
    }

    #[test]
    fn reactor_cancel_removes_waiter_and_skips_stale_readiness() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);

        assert!(reactor_cancel(&reg));
        assert!(!reactor_cancel(&reg));
        assert!(!reactor_wake_ready(reg.id()));

        let count = waker.count.lock().unwrap();
        assert_eq!(*count, 0);
    }

    #[test]
    fn reactor_invokes_waker_outside_registry_lock() {
        let waker = Box::leak(Box::new(ReentrantWaker {
            count: Mutex::new(0),
        }));
        let reg = reactor_register(waker as *const ReentrantWaker as usize, reentrant_wake);

        assert!(reactor_wake_ready(reg.id()));
        assert_eq!(*waker.count.lock().unwrap(), 1);
    }

    #[test]
    fn c_abi_reactor_wake_delivers_one_shot_readiness() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);

        assert!(lang_async_reactor_wake(reg.id()));
        assert!(!lang_async_reactor_wake(reg.id()));
        assert!(!reactor_cancel(&reg));

        let count = waker.count.lock().unwrap();
        assert_eq!(
            *count, 1,
            "the exported readiness hook must wake a registered waiter exactly once"
        );
    }

    #[test]
    fn reactor_cancel_and_readiness_race_has_exactly_one_winner() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));

        for _ in 0..1024 {
            let reg = reactor_register(waker as *const CountWaker as usize, count_wake);
            let cancel_reg = reg.clone();
            let id = reg.id();
            let gate = Arc::new(std::sync::Barrier::new(3));

            let cancel_gate = gate.clone();
            let cancel = std::thread::spawn(move || {
                cancel_gate.wait();
                reactor_cancel(&cancel_reg)
            });

            let wake_gate = gate.clone();
            let wake = std::thread::spawn(move || {
                wake_gate.wait();
                reactor_wake_ready(id)
            });

            gate.wait();
            let cancel_won = cancel.join().unwrap();
            let wake_won = wake.join().unwrap();

            assert_eq!(
                cancel_won as u8 + wake_won as u8,
                1,
                "reactor cancellation and readiness delivery must be one-shot even when racing"
            );
            assert!(
                !reactor_has_waiter(id),
                "the racing registration must always be drained"
            );
        }
    }

    #[test]
    fn mass_reactor_registrations_cancel_and_c_abi_wake_without_leaks() {
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let regs: Vec<ReactorRegistration> = (0..2048)
            .map(|_| reactor_register(waker as *const CountWaker as usize, count_wake))
            .collect();

        assert!(
            regs.iter().all(|reg| reactor_has_waiter(reg.id())),
            "setup must register one reactor waiter per future I/O interest"
        );

        for reg in regs.iter().step_by(2) {
            assert!(reactor_cancel(reg));
            assert!(!lang_async_reactor_wake(reg.id()));
        }

        let mut live = 0usize;
        for reg in regs.iter().skip(1).step_by(2) {
            assert!(lang_async_reactor_wake(reg.id()));
            assert!(!lang_async_reactor_wake(reg.id()));
            assert!(!reactor_cancel(reg));
            live += 1;
        }

        assert!(
            regs.iter().all(|reg| !reactor_has_waiter(reg.id())),
            "mass cancellation/readiness delivery must drain the reactor registry"
        );
        assert_eq!(
            *waker.count.lock().unwrap(),
            live as u32,
            "only live one-shot registrations should invoke the waker"
        );
    }

    #[test]
    fn cancelling_sleep_future_removes_timer_registration() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let waker = CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        };
        let fut = unsafe { lang_async_sleep(60_000, 7, 9) };
        let result = poll_future_once(fut, &waker);
        assert_eq!(unsafe { (result as *const i64).read() }, 9);

        let id = future_data_word(fut, 16) as u64;
        let cell = sleep_registry()
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .expect("sleep future should be registered after first pending poll");
        assert!(cell.token.registration.lock().unwrap().is_some());

        unsafe { crate::threads::lang_future_cancel(fut) };

        assert!(!sleep_registry().lock().unwrap().contains_key(&id));
        assert!(!cell.token.active.load(AtomicOrdering::SeqCst));
        assert!(cell.token.registration.lock().unwrap().is_none());
        assert_eq!(*waker.count.lock().unwrap(), 0);
    }

    #[test]
    fn cancelling_io_future_drains_reactor_waiter_and_result_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);
        let reg_id = reg.id();
        let result = unsafe { lang_str_from_utf8("0".as_ptr(), 1) } as usize;
        gc::add_extra_root(result);
        assert_eq!(gc::extra_root_count_for(result), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::StdoutFlush,
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(result)),
            registration: Mutex::new(Some(reg)),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert!(!reactor_has_waiter(reg_id));
        assert_eq!(gc::extra_root_count_for(result), 0);
        assert_eq!(*waker.count.lock().unwrap(), 0);
    }

    #[test]
    fn cancelling_timeout_future_cancels_timer_and_inner_future() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let waker = CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        };
        let inner = unsafe { lang_async_sleep(60_000, 7, 9) };
        let fut = unsafe { lang_async_timeout(inner, 60_000, 123, 0, 0, 8, 7, 9) };
        let result = poll_future_once(fut, &waker);
        assert_eq!(unsafe { (result as *const i64).read() }, 9);

        let inner_id = future_data_word(inner, 16) as u64;
        let timeout_id = future_data_word(fut, 56) as u64;
        let inner_cell = sleep_registry()
            .lock()
            .unwrap()
            .get(&inner_id)
            .cloned()
            .expect("inner sleep should be pending before timeout cancellation");
        let timeout_cell = timeout_registry()
            .lock()
            .unwrap()
            .get(&timeout_id)
            .cloned()
            .expect("timeout should be pending before cancellation");

        unsafe { crate::threads::lang_future_cancel(fut) };

        assert!(!timeout_registry().lock().unwrap().contains_key(&timeout_id));
        assert!(!timeout_cell.token.active.load(AtomicOrdering::SeqCst));
        assert!(timeout_cell.token.registration.lock().unwrap().is_none());
        assert!(!sleep_registry().lock().unwrap().contains_key(&inner_id));
        assert!(!inner_cell.token.active.load(AtomicOrdering::SeqCst));
        assert!(inner_cell.token.registration.lock().unwrap().is_none());
        assert_eq!(*waker.count.lock().unwrap(), 0);
    }
}
