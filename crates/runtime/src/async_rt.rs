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
//! The private `lang_drive_root_future` root driver polls a top-level future
//! until it resolves on the current thread, parking on a condvar between polls. The
//! future arranges its own re-poll by invoking `ctx.wake_fn(ctx.waker_data)`
//! (typically from an I/O callback or the shared timer driver); a future that
//! returns `Pending` without ever waking is a forever-hung task, exactly as the
//! spec warns.

use crate::gc;
use crate::strings::{LangStr, lang_str_from_utf8, str_bytes};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError, WaitTimeoutResult};

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
/// condvar the private root driver waits on.
struct ThreadWaker {
    woken: Mutex<bool>,
    cv: Condvar,
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn runtime_lock_no_roots<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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

fn spawn_runtime_thread_native_wait<F>(f: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    gc::native_wait(|| std::thread::spawn(f))
}

/// The `wake_fn` installed in the [`Context`] for the private root driver.
/// `data` points at its [`ThreadWaker`]; waking sets the flag and signals the
/// condvar so the parked root future re-polls.
extern "C" fn wake_thunk(data: *mut u8) {
    let w = unsafe { &*(data as *const ThreadWaker) };
    *lock_unpoison(&w.woken) = true;
    w.cv.notify_all();
}

/// Drive `fut` (a `Future<Out>` interface-object box) until it resolves on the
/// current thread and return its `Out`, widened to a machine word
/// (`docs/21` §6, internal root-future driver). `pending_tid` is the `Pending`
/// type id the code generator passes so a `Pending` poll result can be
/// distinguished from a `Ready<Out>`.
///
/// Between `Pending` polls the thread parks on a condvar (in GC *native* state,
/// so it stays scannable) until the future's waker fires.
///
/// # Safety
/// `fut` must be a valid `Future<Out>` interface-object box whose vtable slot 0
/// is a `poll` function with the ABI documented at the module level.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_drive_root_future(fut: *mut u8, pending_tid: i64) -> i64 {
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
/// captured task waker outside the reactor mutex. The future's cancellation/drop
/// path may immediately unregister the handle without waiting on target I/O.
pub fn reactor_register(waker_data: usize, wake_fn: extern "C" fn(*mut u8)) -> ReactorRegistration {
    let r = reactor();
    let id = r.next.fetch_add(1, Ordering::Relaxed);
    let active = Arc::new(AtomicBool::new(true));
    runtime_lock_no_roots(&r.waiters).insert(
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
    runtime_lock_no_roots(&reactor().waiters).remove(&reg.id);
    true
}

/// Deliver readiness for a previously registered one-shot I/O waiter.
///
/// Returns true when a live registration was found and woken. Stale readiness
/// notifications are harmless and return false.
pub fn reactor_wake_ready(id: u64) -> bool {
    let waiter = runtime_lock_no_roots(&reactor().waiters).remove(&id);
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
        let mut slot = runtime_lock_no_roots(&self.registration);
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
        if let Some(reg) = runtime_lock_no_roots(&self.registration).take() {
            reactor_cancel(&reg);
        }
    }

    fn take_registration(&self) -> Option<ReactorRegistration> {
        runtime_lock_no_roots(&self.registration).take()
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
    gc::enter_runtime_native_no_roots();
    STARTED.get_or_init(|| {
        let driver_addr = driver as *const TimerDriver as usize;
        let _ = std::thread::spawn(move || {
            let driver = unsafe { &*(driver_addr as *const TimerDriver) };
            timer_loop(driver);
        });
    });
    gc::leave_native();
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
    let mut heap = runtime_lock_no_roots(&driver.heap);
    heap.push(entry);
    drop(heap);
    driver.cv.notify_one();
}

fn wait_timer_driver<'a>(
    driver: &TimerDriver,
    heap: MutexGuard<'a, BinaryHeap<TimerEntry>>,
) -> MutexGuard<'a, BinaryHeap<TimerEntry>> {
    gc::enter_runtime_native_no_roots();
    let heap = wait_unpoison(&driver.cv, heap);
    gc::leave_native();
    heap
}

fn wait_timer_driver_timeout<'a>(
    driver: &TimerDriver,
    heap: MutexGuard<'a, BinaryHeap<TimerEntry>>,
    wait: Duration,
) -> MutexGuard<'a, BinaryHeap<TimerEntry>> {
    gc::enter_runtime_native_no_roots();
    let (heap, _) = wait_timeout_unpoison(&driver.cv, heap, wait);
    gc::leave_native();
    heap
}

fn timer_loop(driver: &'static TimerDriver) {
    gc::thread_start();
    let mut heap = runtime_lock_no_roots(&driver.heap);
    loop {
        let Some(next) = heap.peek() else {
            heap = wait_timer_driver(driver, heap);
            continue;
        };
        let now = Instant::now();
        if next.at > now {
            let wait = next.at.duration_since(now);
            heap = wait_timer_driver_timeout(driver, heap, wait);
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
        heap = runtime_lock_no_roots(&driver.heap);
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

// -- helper-backed stdlib async futures -------------------------------------
//
// Portable stdio and descriptor-backed filesystem handles are not
// readiness-based on every target. The executor-facing contract still must not
// park a worker poll on a target wait, so these futures hand the wait-capable
// provider operation to a helper thread and use the same cancellable reactor
// registration as timers/I/O readiness to wake the parked task. The provider
// helper called by `io_run` owns GC native-state bracketing for the actual host
// wait; the completion path below runs after that wait has ended and must not
// leave a native state it did not enter.

#[derive(Clone)]
enum IoOp {
    StdinRead {
        count: i64,
    },
    StdinReadToEnd,
    StdoutWrite {
        contents_hex: String,
    },
    StderrWrite {
        contents_hex: String,
    },
    StdoutFlush,
    StderrFlush,
    FsReadText {
        path: String,
    },
    FsWriteText {
        path: String,
        contents: String,
    },
    FsAppendText {
        path: String,
        contents: String,
    },
    FsReadBytes {
        path: String,
    },
    FsWriteBytes {
        path: String,
        contents_hex: String,
    },
    FsExists {
        path: String,
    },
    FsIsFile {
        path: String,
    },
    FsIsDir {
        path: String,
    },
    FsKind {
        path: String,
    },
    FsLen {
        path: String,
    },
    FsReadOnly {
        path: String,
    },
    FsExecutable {
        path: String,
    },
    FsRemove {
        path: String,
    },
    FsRename {
        from: String,
        to: String,
    },
    FsCreateDir {
        path: String,
    },
    FsCreateDirAll {
        path: String,
    },
    FsCanonicalize {
        path: String,
    },
    FsReadDir {
        path: String,
    },
    FsFileOpen {
        path: String,
        mode: String,
    },
    FsFileClose {
        handle: i64,
    },
    FsFileRead {
        handle: i64,
        count: i64,
    },
    FsFileReadToEnd {
        handle: i64,
    },
    FsFileWrite {
        handle: i64,
        contents_hex: String,
    },
    FsFileFlush {
        handle: i64,
    },
    FsFileSeek {
        handle: i64,
        mode: String,
        offset: i64,
    },
    RandOsBytes {
        count: i64,
    },
    TimeMonotonicNanos,
    TimeSystemNanos,
    TimeLocalOffsetSeconds {
        unix_nanos: i64,
    },
    ProcessArgs,
    ProcessEnv {
        name: String,
    },
    ProcessEnvAll,
    ProcessSetEnv {
        name: String,
        value: String,
    },
    ProcessStatus {
        payload: String,
    },
    ProcessOutput {
        payload: String,
    },
    ProcessSpawn {
        payload: String,
    },
    ProcessChildWait {
        handle: i64,
    },
    ProcessChildKill {
        handle: i64,
    },
    NetResolve {
        host: String,
    },
    NetTcpConnect {
        addr: String,
    },
    NetTcpConnectTimeout {
        addr: String,
        nanos: i64,
    },
    NetTcpStreamRead {
        handle: i64,
        count: i64,
    },
    NetTcpStreamReadToEnd {
        handle: i64,
    },
    NetTcpStreamWrite {
        handle: i64,
        contents_hex: String,
    },
    NetTcpStreamWriteAll {
        handle: i64,
        contents_hex: String,
    },
    NetTcpStreamFlush {
        handle: i64,
    },
    NetTcpStreamPeek {
        handle: i64,
        count: i64,
    },
    NetTcpStreamClose {
        handle: i64,
    },
    NetTcpStreamPeerAddr {
        handle: i64,
    },
    NetTcpStreamLocalAddr {
        handle: i64,
    },
    NetTcpStreamTakeError {
        handle: i64,
    },
    NetTcpStreamNodelay {
        handle: i64,
    },
    NetTcpStreamSetNodelay {
        handle: i64,
        on: i64,
    },
    NetTcpStreamSetNonblocking {
        handle: i64,
        on: i64,
    },
    NetTcpStreamReadTimeout {
        handle: i64,
    },
    NetTcpStreamSetReadTimeout {
        handle: i64,
        nanos: i64,
        present: i64,
    },
    NetTcpStreamWriteTimeout {
        handle: i64,
    },
    NetTcpStreamSetWriteTimeout {
        handle: i64,
        nanos: i64,
        present: i64,
    },
    NetTcpStreamTtl {
        handle: i64,
    },
    NetTcpStreamSetTtl {
        handle: i64,
        ttl: i64,
    },
    NetTcpListenerBind {
        addr: String,
    },
    NetTcpListenerClose {
        handle: i64,
    },
    NetTcpListenerAccept {
        handle: i64,
    },
    NetTcpListenerLocalAddr {
        handle: i64,
    },
    NetTcpListenerTakeError {
        handle: i64,
    },
    NetTcpListenerSetNonblocking {
        handle: i64,
        on: i64,
    },
    NetTcpListenerTtl {
        handle: i64,
    },
    NetTcpListenerSetTtl {
        handle: i64,
        ttl: i64,
    },
    NetUdpBind {
        addr: String,
    },
    NetUdpClose {
        handle: i64,
    },
    NetUdpConnect {
        handle: i64,
        addr: String,
    },
    NetUdpLocalAddr {
        handle: i64,
    },
    NetUdpPeerAddr {
        handle: i64,
    },
    NetUdpTakeError {
        handle: i64,
    },
    NetUdpSetNonblocking {
        handle: i64,
        on: i64,
    },
    NetUdpReadTimeout {
        handle: i64,
    },
    NetUdpSetReadTimeout {
        handle: i64,
        nanos: i64,
        present: i64,
    },
    NetUdpWriteTimeout {
        handle: i64,
    },
    NetUdpSetWriteTimeout {
        handle: i64,
        nanos: i64,
        present: i64,
    },
    NetUdpTtl {
        handle: i64,
    },
    NetUdpSetTtl {
        handle: i64,
        ttl: i64,
    },
    NetUdpBroadcast {
        handle: i64,
    },
    NetUdpSetBroadcast {
        handle: i64,
        on: i64,
    },
    NetUdpMulticastLoopV4 {
        handle: i64,
    },
    NetUdpSetMulticastLoopV4 {
        handle: i64,
        on: i64,
    },
    NetUdpMulticastLoopV6 {
        handle: i64,
    },
    NetUdpSetMulticastLoopV6 {
        handle: i64,
        on: i64,
    },
    NetUdpMulticastTtlV4 {
        handle: i64,
    },
    NetUdpSetMulticastTtlV4 {
        handle: i64,
        ttl: i64,
    },
    NetUdpJoinMulticastV4 {
        handle: i64,
        group: String,
        interface: String,
    },
    NetUdpLeaveMulticastV4 {
        handle: i64,
        group: String,
        interface: String,
    },
    NetUdpJoinMulticastV6 {
        handle: i64,
        group: String,
        interface: i64,
    },
    NetUdpLeaveMulticastV6 {
        handle: i64,
        group: String,
        interface: i64,
    },
    NetUdpSend {
        handle: i64,
        contents_hex: String,
    },
    NetUdpRecv {
        handle: i64,
        count: i64,
    },
    NetUdpPeek {
        handle: i64,
        count: i64,
    },
    NetUdpSendTo {
        handle: i64,
        contents_hex: String,
        addr: String,
    },
    NetUdpRecvFrom {
        handle: i64,
        count: i64,
    },
    NetUdpPeekFrom {
        handle: i64,
        count: i64,
    },
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
    // [ready_tid][pending_tid][id] — the wait-capable operation payload is copied
    // into the Rust-side cell so no managed field needs tracing here.
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
        IoOp::FsReadText { path } => crate::fs::fs_read_text_encoded(path.clone()),
        IoOp::FsWriteText { path, contents } => {
            crate::fs::fs_write_text_encoded(path.clone(), contents.clone())
        }
        IoOp::FsAppendText { path, contents } => {
            crate::fs::fs_append_text_encoded(path.clone(), contents.clone())
        }
        IoOp::FsReadBytes { path } => crate::fs::fs_read_bytes_encoded(path.clone()),
        IoOp::FsWriteBytes { path, contents_hex } => {
            crate::fs::fs_write_bytes_encoded(path.clone(), contents_hex.clone())
        }
        IoOp::FsExists { path } => crate::fs::fs_exists_encoded(path.clone()),
        IoOp::FsIsFile { path } => crate::fs::fs_is_file_encoded(path.clone()),
        IoOp::FsIsDir { path } => crate::fs::fs_is_dir_encoded(path.clone()),
        IoOp::FsKind { path } => crate::fs::fs_kind_encoded(path.clone()),
        IoOp::FsLen { path } => crate::fs::fs_len_encoded(path.clone()),
        IoOp::FsReadOnly { path } => crate::fs::fs_read_only_encoded(path.clone()),
        IoOp::FsExecutable { path } => crate::fs::fs_executable_encoded(path.clone()),
        IoOp::FsRemove { path } => crate::fs::fs_remove_encoded(path.clone()),
        IoOp::FsRename { from, to } => crate::fs::fs_rename_encoded(from.clone(), to.clone()),
        IoOp::FsCreateDir { path } => crate::fs::fs_create_dir_encoded(path.clone()),
        IoOp::FsCreateDirAll { path } => crate::fs::fs_create_dir_all_encoded(path.clone()),
        IoOp::FsCanonicalize { path } => crate::fs::fs_canonicalize_encoded(path.clone()),
        IoOp::FsReadDir { path } => crate::fs::fs_read_dir_encoded(path.clone()),
        IoOp::FsFileOpen { path, mode } => {
            crate::fs::fs_file_open_encoded(path.clone(), mode.clone())
        }
        IoOp::FsFileClose { handle } => crate::fs::fs_file_close_encoded(*handle),
        IoOp::FsFileRead { handle, count } => crate::fs::fs_file_read_encoded(*handle, *count),
        IoOp::FsFileReadToEnd { handle } => crate::fs::fs_file_read_to_end_encoded(*handle),
        IoOp::FsFileWrite {
            handle,
            contents_hex,
        } => crate::fs::fs_file_write_encoded(*handle, contents_hex),
        IoOp::FsFileFlush { handle } => crate::fs::fs_file_flush_encoded(*handle),
        IoOp::FsFileSeek {
            handle,
            mode,
            offset,
        } => crate::fs::fs_file_seek_encoded(*handle, mode, *offset),
        IoOp::RandOsBytes { count } => crate::rand::rand_os_bytes_encoded(*count),
        IoOp::TimeMonotonicNanos => crate::time::time_monotonic_nanos_encoded(),
        IoOp::TimeSystemNanos => crate::time::time_system_nanos_encoded(),
        IoOp::TimeLocalOffsetSeconds { unix_nanos } => {
            crate::time::time_local_offset_seconds_encoded(*unix_nanos)
        }
        IoOp::ProcessArgs => crate::process::process_args_encoded(),
        IoOp::ProcessEnv { name } => crate::process::process_env_encoded(name.clone()),
        IoOp::ProcessEnvAll => crate::process::process_env_all_encoded(),
        IoOp::ProcessSetEnv { name, value } => {
            crate::process::process_set_env_encoded(name.clone(), value.clone())
        }
        IoOp::ProcessStatus { payload } => crate::process::process_status_encoded(payload.clone()),
        IoOp::ProcessOutput { payload } => crate::process::process_output_encoded(payload.clone()),
        IoOp::ProcessSpawn { payload } => crate::process::process_spawn_encoded(payload.clone()),
        IoOp::ProcessChildWait { handle } => crate::process::process_child_wait_encoded(*handle),
        IoOp::ProcessChildKill { handle } => crate::process::process_child_kill_encoded(*handle),
        IoOp::NetResolve { host } => crate::net::net_resolve_encoded(host.clone()),
        IoOp::NetTcpConnect { addr } => crate::net::net_tcp_connect_encoded(addr.clone()),
        IoOp::NetTcpConnectTimeout { addr, nanos } => {
            crate::net::net_tcp_connect_timeout_encoded(addr.clone(), *nanos)
        }
        IoOp::NetTcpStreamRead { handle, count } => {
            crate::net::net_tcp_stream_read_encoded(*handle, *count)
        }
        IoOp::NetTcpStreamReadToEnd { handle } => {
            crate::net::net_tcp_stream_read_to_end_encoded(*handle)
        }
        IoOp::NetTcpStreamWrite {
            handle,
            contents_hex,
        } => crate::net::net_tcp_stream_write_encoded(*handle, contents_hex.clone()),
        IoOp::NetTcpStreamWriteAll {
            handle,
            contents_hex,
        } => crate::net::net_tcp_stream_write_all_encoded(*handle, contents_hex.clone()),
        IoOp::NetTcpStreamFlush { handle } => crate::net::net_tcp_stream_flush_encoded(*handle),
        IoOp::NetTcpStreamPeek { handle, count } => {
            crate::net::net_tcp_stream_peek_encoded(*handle, *count)
        }
        IoOp::NetTcpStreamClose { handle } => crate::net::net_tcp_stream_close_encoded(*handle),
        IoOp::NetTcpStreamPeerAddr { handle } => {
            crate::net::net_tcp_stream_peer_addr_encoded(*handle)
        }
        IoOp::NetTcpStreamLocalAddr { handle } => {
            crate::net::net_tcp_stream_local_addr_encoded(*handle)
        }
        IoOp::NetTcpStreamTakeError { handle } => {
            crate::net::net_tcp_stream_take_error_encoded(*handle)
        }
        IoOp::NetTcpStreamNodelay { handle } => crate::net::net_tcp_stream_nodelay_encoded(*handle),
        IoOp::NetTcpStreamSetNodelay { handle, on } => {
            crate::net::net_tcp_stream_set_nodelay_encoded(*handle, *on)
        }
        IoOp::NetTcpStreamSetNonblocking { handle, on } => {
            crate::net::net_tcp_stream_set_nonblocking_encoded(*handle, *on)
        }
        IoOp::NetTcpStreamReadTimeout { handle } => {
            crate::net::net_tcp_stream_read_timeout_encoded(*handle)
        }
        IoOp::NetTcpStreamSetReadTimeout {
            handle,
            nanos,
            present,
        } => crate::net::net_tcp_stream_set_read_timeout_encoded(*handle, *nanos, *present),
        IoOp::NetTcpStreamWriteTimeout { handle } => {
            crate::net::net_tcp_stream_write_timeout_encoded(*handle)
        }
        IoOp::NetTcpStreamSetWriteTimeout {
            handle,
            nanos,
            present,
        } => crate::net::net_tcp_stream_set_write_timeout_encoded(*handle, *nanos, *present),
        IoOp::NetTcpStreamTtl { handle } => crate::net::net_tcp_stream_ttl_encoded(*handle),
        IoOp::NetTcpStreamSetTtl { handle, ttl } => {
            crate::net::net_tcp_stream_set_ttl_encoded(*handle, *ttl)
        }
        IoOp::NetTcpListenerBind { addr } => {
            crate::net::net_tcp_listener_bind_encoded(addr.clone())
        }
        IoOp::NetTcpListenerClose { handle } => crate::net::net_tcp_listener_close_encoded(*handle),
        IoOp::NetTcpListenerAccept { handle } => {
            crate::net::net_tcp_listener_accept_encoded(*handle)
        }
        IoOp::NetTcpListenerLocalAddr { handle } => {
            crate::net::net_tcp_listener_local_addr_encoded(*handle)
        }
        IoOp::NetTcpListenerTakeError { handle } => {
            crate::net::net_tcp_listener_take_error_encoded(*handle)
        }
        IoOp::NetTcpListenerSetNonblocking { handle, on } => {
            crate::net::net_tcp_listener_set_nonblocking_encoded(*handle, *on)
        }
        IoOp::NetTcpListenerTtl { handle } => crate::net::net_tcp_listener_ttl_encoded(*handle),
        IoOp::NetTcpListenerSetTtl { handle, ttl } => {
            crate::net::net_tcp_listener_set_ttl_encoded(*handle, *ttl)
        }
        IoOp::NetUdpBind { addr } => crate::net::net_udp_bind_encoded(addr.clone()),
        IoOp::NetUdpClose { handle } => crate::net::net_udp_close_encoded(*handle),
        IoOp::NetUdpConnect { handle, addr } => {
            crate::net::net_udp_connect_encoded(*handle, addr.clone())
        }
        IoOp::NetUdpLocalAddr { handle } => crate::net::net_udp_local_addr_encoded(*handle),
        IoOp::NetUdpPeerAddr { handle } => crate::net::net_udp_peer_addr_encoded(*handle),
        IoOp::NetUdpTakeError { handle } => crate::net::net_udp_take_error_encoded(*handle),
        IoOp::NetUdpSetNonblocking { handle, on } => {
            crate::net::net_udp_set_nonblocking_encoded(*handle, *on)
        }
        IoOp::NetUdpReadTimeout { handle } => crate::net::net_udp_read_timeout_encoded(*handle),
        IoOp::NetUdpSetReadTimeout {
            handle,
            nanos,
            present,
        } => crate::net::net_udp_set_read_timeout_encoded(*handle, *nanos, *present),
        IoOp::NetUdpWriteTimeout { handle } => crate::net::net_udp_write_timeout_encoded(*handle),
        IoOp::NetUdpSetWriteTimeout {
            handle,
            nanos,
            present,
        } => crate::net::net_udp_set_write_timeout_encoded(*handle, *nanos, *present),
        IoOp::NetUdpTtl { handle } => crate::net::net_udp_ttl_encoded(*handle),
        IoOp::NetUdpSetTtl { handle, ttl } => crate::net::net_udp_set_ttl_encoded(*handle, *ttl),
        IoOp::NetUdpBroadcast { handle } => crate::net::net_udp_broadcast_encoded(*handle),
        IoOp::NetUdpSetBroadcast { handle, on } => {
            crate::net::net_udp_set_broadcast_encoded(*handle, *on)
        }
        IoOp::NetUdpMulticastLoopV4 { handle } => {
            crate::net::net_udp_multicast_loop_v4_encoded(*handle)
        }
        IoOp::NetUdpSetMulticastLoopV4 { handle, on } => {
            crate::net::net_udp_set_multicast_loop_v4_encoded(*handle, *on)
        }
        IoOp::NetUdpMulticastLoopV6 { handle } => {
            crate::net::net_udp_multicast_loop_v6_encoded(*handle)
        }
        IoOp::NetUdpSetMulticastLoopV6 { handle, on } => {
            crate::net::net_udp_set_multicast_loop_v6_encoded(*handle, *on)
        }
        IoOp::NetUdpMulticastTtlV4 { handle } => {
            crate::net::net_udp_multicast_ttl_v4_encoded(*handle)
        }
        IoOp::NetUdpSetMulticastTtlV4 { handle, ttl } => {
            crate::net::net_udp_set_multicast_ttl_v4_encoded(*handle, *ttl)
        }
        IoOp::NetUdpJoinMulticastV4 {
            handle,
            group,
            interface,
        } => {
            crate::net::net_udp_join_multicast_v4_encoded(*handle, group.clone(), interface.clone())
        }
        IoOp::NetUdpLeaveMulticastV4 {
            handle,
            group,
            interface,
        } => crate::net::net_udp_leave_multicast_v4_encoded(
            *handle,
            group.clone(),
            interface.clone(),
        ),
        IoOp::NetUdpJoinMulticastV6 {
            handle,
            group,
            interface,
        } => crate::net::net_udp_join_multicast_v6_encoded(*handle, group.clone(), *interface),
        IoOp::NetUdpLeaveMulticastV6 {
            handle,
            group,
            interface,
        } => crate::net::net_udp_leave_multicast_v6_encoded(*handle, group.clone(), *interface),
        IoOp::NetUdpSend {
            handle,
            contents_hex,
        } => crate::net::net_udp_send_encoded(*handle, contents_hex.clone()),
        IoOp::NetUdpRecv { handle, count } => crate::net::net_udp_recv_encoded(*handle, *count),
        IoOp::NetUdpPeek { handle, count } => crate::net::net_udp_peek_encoded(*handle, *count),
        IoOp::NetUdpSendTo {
            handle,
            contents_hex,
            addr,
        } => crate::net::net_udp_send_to_encoded(*handle, contents_hex.clone(), addr.clone()),
        IoOp::NetUdpRecvFrom { handle, count } => {
            crate::net::net_udp_recv_from_encoded(*handle, *count)
        }
        IoOp::NetUdpPeekFrom { handle, count } => {
            crate::net::net_udp_peek_from_encoded(*handle, *count)
        }
    }
}

fn io_complete(cell: std::sync::Arc<IoCell>, encoded: String) {
    if cell.cancelled.load(Ordering::SeqCst) {
        io_discard_cancelled_result(&cell.op, &encoded);
        return;
    }
    let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
    gc::add_extra_root(ptr);
    let reg = {
        let mut result = lock_unpoison(&cell.result);
        if cell.cancelled.load(Ordering::SeqCst) {
            io_discard_cancelled_result(&cell.op, &encoded);
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
    let _ = spawn_runtime_thread_native_wait(move || {
        gc::thread_start();
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
            io_discard_cancelled_result_ptr(&cell.op, ptr);
            gc::remove_extra_root(ptr);
        }
    }
}

fn io_discard_cancelled_result_ptr(op: &IoOp, ptr: usize) {
    let encoded = String::from_utf8_lossy(unsafe { str_bytes(ptr as *const LangStr) }).into_owned();
    io_discard_cancelled_result(op, &encoded);
}

fn io_discard_cancelled_result(op: &IoOp, encoded: &str) {
    match op {
        IoOp::NetTcpConnect { .. } | IoOp::NetTcpConnectTimeout { .. } => {
            // Both async connect forms may register a stream before a cancelled
            // helper result is discarded; release that handle deterministically.
            if let Some(handle) = encoded_success_payload(encoded).and_then(parse_i64_payload) {
                crate::net::lang_net_tcp_stream_release(handle);
            }
        }
        IoOp::NetTcpListenerBind { .. } => {
            // A cancelled bind may still create and register a listener before
            // the helper result is discarded.
            if let Some(handle) = encoded_success_payload(encoded).and_then(parse_i64_payload) {
                crate::net::lang_net_tcp_listener_release(handle);
            }
        }
        IoOp::NetUdpBind { .. } => {
            if let Some(handle) = encoded_success_payload(encoded).and_then(parse_i64_payload) {
                crate::net::lang_net_udp_release(handle);
            }
        }
        IoOp::FsFileOpen { .. } => {
            if let Some(handle) = encoded_success_payload(encoded).and_then(parse_i64_payload) {
                let _ = crate::fs::fs_file_close_encoded(handle);
            }
        }
        IoOp::NetTcpListenerAccept { .. } => {
            if let Some(handle) = encoded_success_payload(encoded)
                .and_then(first_encoded_field)
                .and_then(parse_i64_payload)
            {
                crate::net::lang_net_tcp_stream_release(handle);
            }
        }
        IoOp::ProcessSpawn { .. } => {
            if let Some(handle) = encoded_success_payload(encoded)
                .and_then(first_encoded_field)
                .and_then(parse_i64_payload)
            {
                crate::process::lang_process_child_release(handle);
            }
        }
        _ => {}
    }
}

fn encoded_success_payload(encoded: &str) -> Option<&str> {
    encoded.strip_prefix('0')
}

fn parse_i64_payload(payload: &str) -> Option<i64> {
    payload.parse::<i64>().ok()
}

fn first_encoded_field(payload: &str) -> Option<&str> {
    let colon = payload.find(':')?;
    let len = payload[..colon].parse::<usize>().ok()?;
    let start = colon + 1;
    let end = start.checked_add(len)?;
    if end <= payload.len() {
        Some(&payload[start..end])
    } else {
        None
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

fn runtime_str_to_string(s: *const LangStr) -> String {
    String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
}

unsafe fn fs_path_future(
    path: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
    make_op: impl FnOnce(String) -> IoOp,
) -> *mut u8 {
    let path = runtime_str_to_string(path);
    lang_io_future(make_op(path), ready_tid, pending_tid)
}

unsafe fn fs_two_str_future(
    first: *const LangStr,
    second: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
    make_op: impl FnOnce(String, String) -> IoOp,
) -> *mut u8 {
    let first = runtime_str_to_string(first);
    let second = runtime_str_to_string(second);
    lang_io_future(make_op(first, second), ready_tid, pending_tid)
}

/// Build a reactor-backed future for reading a UTF-8 text file.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_text_async(
    path: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_path_future(path, ready_tid, pending_tid, |path| IoOp::FsReadText {
            path,
        })
    }
}

/// Build a reactor-backed future for creating/truncating a UTF-8 text file.
///
/// # Safety
/// `path` and `contents` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_write_text_async(
    path: *const LangStr,
    contents: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_two_str_future(path, contents, ready_tid, pending_tid, |path, contents| {
            IoOp::FsWriteText { path, contents }
        })
    }
}

/// Build a reactor-backed future for appending a UTF-8 text file.
///
/// # Safety
/// `path` and `contents` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_append_text_async(
    path: *const LangStr,
    contents: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_two_str_future(path, contents, ready_tid, pending_tid, |path, contents| {
            IoOp::FsAppendText { path, contents }
        })
    }
}

/// Build a reactor-backed future for reading a binary file.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_bytes_async(
    path: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_path_future(path, ready_tid, pending_tid, |path| IoOp::FsReadBytes {
            path,
        })
    }
}

/// Build a reactor-backed future for writing a binary file from a hex payload.
///
/// # Safety
/// `path` and `contents_hex` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_write_bytes_async(
    path: *const LangStr,
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_two_str_future(
            path,
            contents_hex,
            ready_tid,
            pending_tid,
            |path, contents_hex| IoOp::FsWriteBytes { path, contents_hex },
        )
    }
}

macro_rules! fs_path_async_export {
    ($name:ident, $variant:ident, $doc:literal) => {
        #[doc = $doc]
        ///
        /// # Safety
        /// `path` must be a valid runtime `str` pointer.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            path: *const LangStr,
            ready_tid: i64,
            pending_tid: i64,
        ) -> *mut u8 {
            unsafe { fs_path_future(path, ready_tid, pending_tid, |path| IoOp::$variant { path }) }
        }
    };
}

fs_path_async_export!(
    lang_fs_exists_async,
    FsExists,
    "Build a reactor-backed future for testing path existence."
);
fs_path_async_export!(
    lang_fs_is_file_async,
    FsIsFile,
    "Build a reactor-backed future for testing whether a path is a file."
);
fs_path_async_export!(
    lang_fs_is_dir_async,
    FsIsDir,
    "Build a reactor-backed future for testing whether a path is a directory."
);
fs_path_async_export!(
    lang_fs_kind_async,
    FsKind,
    "Build a reactor-backed future for reading a path's file kind."
);
fs_path_async_export!(
    lang_fs_len_async,
    FsLen,
    "Build a reactor-backed future for reading a path's byte length."
);
fs_path_async_export!(
    lang_fs_read_only_async,
    FsReadOnly,
    "Build a reactor-backed future for reading a path's read-only permission bit."
);
fs_path_async_export!(
    lang_fs_executable_async,
    FsExecutable,
    "Build a reactor-backed future for reading a path's executable permission bit."
);
fs_path_async_export!(
    lang_fs_remove_async,
    FsRemove,
    "Build a reactor-backed future for removing a filesystem path."
);
fs_path_async_export!(
    lang_fs_create_dir_async,
    FsCreateDir,
    "Build a reactor-backed future for creating a directory."
);
fs_path_async_export!(
    lang_fs_create_dir_all_async,
    FsCreateDirAll,
    "Build a reactor-backed future for creating a directory and parents."
);
fs_path_async_export!(
    lang_fs_canonicalize_async,
    FsCanonicalize,
    "Build a reactor-backed future for canonicalizing a filesystem path."
);
fs_path_async_export!(
    lang_fs_read_dir_async,
    FsReadDir,
    "Build a reactor-backed future for reading a directory snapshot."
);

/// Build a reactor-backed future for renaming or moving a path.
///
/// # Safety
/// `from` and `to` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_rename_async(
    from: *const LangStr,
    to: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    unsafe {
        fs_two_str_future(from, to, ready_tid, pending_tid, |from, to| {
            IoOp::FsRename { from, to }
        })
    }
}

/// Build a reactor-backed future for opening a descriptor-backed file.
///
/// # Safety
/// `path` and `mode` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_open_async(
    path: *const LangStr,
    mode: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let path = runtime_str_to_string(path);
    let mode = runtime_str_to_string(mode);
    lang_io_future(IoOp::FsFileOpen { path, mode }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for closing a descriptor-backed file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_close_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::FsFileClose { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for reading up to `count` bytes from a file.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_read_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::FsFileRead { handle, count }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for reading all remaining bytes from a file.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_read_to_end_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::FsFileReadToEnd { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for writing a hex byte payload to a file.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_write_async(
    handle: i64,
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = runtime_str_to_string(contents_hex);
    lang_io_future(
        IoOp::FsFileWrite {
            handle,
            contents_hex,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for flushing a file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_flush_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::FsFileFlush { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for seeking a file handle.
///
/// # Safety
/// `mode` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_seek_async(
    handle: i64,
    mode: *const LangStr,
    offset: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let mode = runtime_str_to_string(mode);
    lang_io_future(
        IoOp::FsFileSeek {
            handle,
            mode,
            offset,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for target-backed OS entropy bytes.
#[unsafe(no_mangle)]
pub extern "C" fn lang_rand_os_bytes_async(
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::RandOsBytes { count }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for a target-backed monotonic clock read.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_monotonic_nanos_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::TimeMonotonicNanos, ready_tid, pending_tid)
}

/// Build a reactor-backed future for a target-backed wall-clock read.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_system_nanos_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::TimeSystemNanos, ready_tid, pending_tid)
}

/// Build a reactor-backed future for a provider-backed local UTC offset lookup.
#[unsafe(no_mangle)]
pub extern "C" fn lang_time_local_offset_seconds_async(
    unix_nanos: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::TimeLocalOffsetSeconds { unix_nanos },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for snapshotting the current argv vector.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_args_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::ProcessArgs, ready_tid, pending_tid)
}

/// Build a reactor-backed future for reading one environment variable.
///
/// # Safety
/// `name` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_env_async(
    name: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let name = String::from_utf8_lossy(unsafe { str_bytes(name) }).into_owned();
    lang_io_future(IoOp::ProcessEnv { name }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for snapshotting the current environment.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_env_all_async(ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::ProcessEnvAll, ready_tid, pending_tid)
}

/// Build a reactor-backed future for mutating one environment variable.
///
/// # Safety
/// `name` and `value` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_set_env_async(
    name: *const LangStr,
    value: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let name = String::from_utf8_lossy(unsafe { str_bytes(name) }).into_owned();
    let value = String::from_utf8_lossy(unsafe { str_bytes(value) }).into_owned();
    lang_io_future(IoOp::ProcessSetEnv { name, value }, ready_tid, pending_tid)
}

/// Build a reactor-backed future that resolves a command status.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by `std:process`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_status_async(
    payload: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let payload = String::from_utf8_lossy(unsafe { str_bytes(payload) }).into_owned();
    lang_io_future(IoOp::ProcessStatus { payload }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for running a command and capturing output.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by `std:process`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_output_async(
    payload: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let payload = String::from_utf8_lossy(unsafe { str_bytes(payload) }).into_owned();
    lang_io_future(IoOp::ProcessOutput { payload }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for spawning a live child process.
///
/// If the future is cancelled after the provider returns a child handle, the
/// runtime releases the handle table entry before discarding the result.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by `std:process`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_spawn_async(
    payload: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let payload = String::from_utf8_lossy(unsafe { str_bytes(payload) }).into_owned();
    lang_io_future(IoOp::ProcessSpawn { payload }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for waiting on a child process handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_wait_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::ProcessChildWait { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for killing a child process handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_kill_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::ProcessChildKill { handle }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for resolving a host through DNS.
///
/// The provider lookup happens on a helper thread so executor `poll` never
/// parks a worker. Cancelling the future drops its reactor registration and
/// ignores the eventual encoded result.
///
/// # Safety
/// `host` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_resolve_async(
    host: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let host = String::from_utf8_lossy(unsafe { str_bytes(host) }).into_owned();
    lang_io_future(IoOp::NetResolve { host }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for connecting a TCP stream.
///
/// The OS connect happens on a helper thread so executor `poll` never parks a
/// worker. Cancelling the future before completion drops its reactor
/// registration and ignores the eventual helper result.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_connect_async(
    addr: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(IoOp::NetTcpConnect { addr }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for a timed TCP connect.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_connect_timeout_async(
    addr: *const LangStr,
    nanos: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(
        IoOp::NetTcpConnectTimeout { addr, nanos },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading from a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_read_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamRead { handle, count },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading a TCP stream handle until EOF.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_read_to_end_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamReadToEnd { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for writing a hex byte payload to a TCP stream.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_stream_write_async(
    handle: i64,
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    lang_io_future(
        IoOp::NetTcpStreamWrite {
            handle,
            contents_hex,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for writing a complete hex byte payload to a TCP stream.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_stream_write_all_async(
    handle: i64,
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    lang_io_future(
        IoOp::NetTcpStreamWriteAll {
            handle,
            contents_hex,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for flushing a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_flush_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpStreamFlush { handle }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for peeking from a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_peek_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamPeek { handle, count },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for closing a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_close_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpStreamClose { handle }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for reading a TCP stream peer address.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_peer_addr_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamPeerAddr { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading a TCP stream local address.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_local_addr_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamLocalAddr { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading and clearing a TCP stream socket error.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_take_error_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamTakeError { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading TCP_NODELAY.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_nodelay_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpStreamNodelay { handle }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for setting TCP_NODELAY.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_nodelay_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamSetNodelay { handle, on },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for setting TCP stream nonblocking mode.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_nonblocking_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamSetNonblocking { handle, on },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading the TCP stream read timeout.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_read_timeout_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamReadTimeout { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for setting the TCP stream read timeout.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_read_timeout_async(
    handle: i64,
    nanos: i64,
    present: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamSetReadTimeout {
            handle,
            nanos,
            present,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading the TCP stream write timeout.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_write_timeout_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamWriteTimeout { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for setting the TCP stream write timeout.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_write_timeout_async(
    handle: i64,
    nanos: i64,
    present: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamSetWriteTimeout {
            handle,
            nanos,
            present,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-waking future for reading the TCP stream TTL.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_ttl_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpStreamTtl { handle }, ready_tid, pending_tid)
}

/// Build a reactor-waking future for setting the TCP stream TTL.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_ttl_async(
    handle: i64,
    ttl: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpStreamSetTtl { handle, ttl },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for accepting one TCP listener connection.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_listener_bind_async(
    addr: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(IoOp::NetTcpListenerBind { addr }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for closing a TCP listener handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_close_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpListenerClose { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for accepting one TCP listener connection.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_accept_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpListenerAccept { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for reading a TCP listener local address.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_local_addr_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpListenerLocalAddr { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for reading and clearing a TCP listener socket error.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_take_error_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpListenerTakeError { handle },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for toggling TCP listener nonblocking mode.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_set_nonblocking_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpListenerSetNonblocking { handle, on },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for reading a TCP listener TTL value.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_ttl_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetTcpListenerTtl { handle }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for setting a TCP listener TTL value.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_set_ttl_async(
    handle: i64,
    ttl: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetTcpListenerSetTtl { handle, ttl },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for binding a UDP socket.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_bind_async(
    addr: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(IoOp::NetUdpBind { addr }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_close_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpClose { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_connect_async(
    handle: i64,
    addr: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(IoOp::NetUdpConnect { handle, addr }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_local_addr_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpLocalAddr { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_peer_addr_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpPeerAddr { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_take_error_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpTakeError { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_nonblocking_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetNonblocking { handle, on },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_read_timeout_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpReadTimeout { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_read_timeout_async(
    handle: i64,
    nanos: i64,
    present: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetReadTimeout {
            handle,
            nanos,
            present,
        },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_write_timeout_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpWriteTimeout { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_write_timeout_async(
    handle: i64,
    nanos: i64,
    present: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetWriteTimeout {
            handle,
            nanos,
            present,
        },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_ttl_async(handle: i64, ready_tid: i64, pending_tid: i64) -> *mut u8 {
    lang_io_future(IoOp::NetUdpTtl { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_ttl_async(
    handle: i64,
    ttl: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpSetTtl { handle, ttl }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_broadcast_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpBroadcast { handle }, ready_tid, pending_tid)
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_broadcast_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetBroadcast { handle, on },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_multicast_loop_v4_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpMulticastLoopV4 { handle },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_multicast_loop_v4_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetMulticastLoopV4 { handle, on },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_multicast_loop_v6_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpMulticastLoopV6 { handle },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_multicast_loop_v6_async(
    handle: i64,
    on: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetMulticastLoopV6 { handle, on },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_multicast_ttl_v4_async(
    handle: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpMulticastTtlV4 { handle },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_set_multicast_ttl_v4_async(
    handle: i64,
    ttl: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpSetMulticastTtlV4 { handle, ttl },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_join_multicast_v4_async(
    handle: i64,
    group: *const LangStr,
    interface: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let group = String::from_utf8_lossy(unsafe { str_bytes(group) }).into_owned();
    let interface = String::from_utf8_lossy(unsafe { str_bytes(interface) }).into_owned();
    lang_io_future(
        IoOp::NetUdpJoinMulticastV4 {
            handle,
            group,
            interface,
        },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_leave_multicast_v4_async(
    handle: i64,
    group: *const LangStr,
    interface: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let group = String::from_utf8_lossy(unsafe { str_bytes(group) }).into_owned();
    let interface = String::from_utf8_lossy(unsafe { str_bytes(interface) }).into_owned();
    lang_io_future(
        IoOp::NetUdpLeaveMulticastV4 {
            handle,
            group,
            interface,
        },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_join_multicast_v6_async(
    handle: i64,
    group: *const LangStr,
    interface: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let group = String::from_utf8_lossy(unsafe { str_bytes(group) }).into_owned();
    lang_io_future(
        IoOp::NetUdpJoinMulticastV6 {
            handle,
            group,
            interface,
        },
        ready_tid,
        pending_tid,
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_leave_multicast_v6_async(
    handle: i64,
    group: *const LangStr,
    interface: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let group = String::from_utf8_lossy(unsafe { str_bytes(group) }).into_owned();
    lang_io_future(
        IoOp::NetUdpLeaveMulticastV6 {
            handle,
            group,
            interface,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for sending through a connected UDP socket.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_send_async(
    handle: i64,
    contents_hex: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    lang_io_future(
        IoOp::NetUdpSend {
            handle,
            contents_hex,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for receiving through a connected UDP socket.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_recv_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpRecv { handle, count }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for peeking through a connected UDP socket.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_peek_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(IoOp::NetUdpPeek { handle, count }, ready_tid, pending_tid)
}

/// Build a reactor-backed future for sending a datagram to a UDP address.
///
/// # Safety
/// `contents_hex` and `addr` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_send_to_async(
    handle: i64,
    contents_hex: *const LangStr,
    addr: *const LangStr,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    let contents_hex = String::from_utf8_lossy(unsafe { str_bytes(contents_hex) }).into_owned();
    let addr = String::from_utf8_lossy(unsafe { str_bytes(addr) }).into_owned();
    lang_io_future(
        IoOp::NetUdpSendTo {
            handle,
            contents_hex,
            addr,
        },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for receiving one UDP datagram and source address.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_recv_from_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpRecvFrom { handle, count },
        ready_tid,
        pending_tid,
    )
}

/// Build a reactor-backed future for peeking one UDP datagram and source address.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_peek_from_async(
    handle: i64,
    count: i64,
    ready_tid: i64,
    pending_tid: i64,
) -> *mut u8 {
    lang_io_future(
        IoOp::NetUdpPeekFrom { handle, count },
        ready_tid,
        pending_tid,
    )
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
/// private root driver understand.
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
    fn runtime_thread_spawn_helper_uses_native_state_marker() {
        let marker = Arc::new(AtomicBool::new(false));
        let worker_marker = marker.clone();
        let handle = spawn_runtime_thread_native_wait(move || {
            worker_marker.store(true, AtomicOrdering::SeqCst);
        });

        handle.join().expect("runtime helper thread should finish");
        assert!(marker.load(AtomicOrdering::SeqCst));
    }

    #[test]
    fn timer_loop_registers_and_brackets_native_waits() {
        let source = include_str!("async_rt.rs");
        let timer_driver_body = source
            .split("fn timer_driver() -> &'static TimerDriver")
            .nth(1)
            .and_then(|rest| rest.split("fn schedule_timer(").next())
            .expect("timer_driver should remain in async_rt.rs");
        assert!(
            timer_driver_body.contains("gc::enter_runtime_native_no_roots();")
                && timer_driver_body.contains("gc::leave_native();"),
            "timer driver startup and concurrent OnceLock waiters must not remain RUNNING"
        );
        assert!(
            timer_driver_body.contains("let driver_addr = driver as *const TimerDriver as usize;"),
            "timer thread startup must capture the initialized driver instead of re-entering timer_driver()"
        );
        let timer_loop_body = source
            .split("fn timer_loop(driver: &'static TimerDriver)")
            .nth(1)
            .and_then(|rest| rest.split("/// Per-`sleep` timer state").next())
            .expect("timer_loop should remain in async_rt.rs");
        assert!(
            timer_loop_body.contains("gc::thread_start();"),
            "timer driver thread must register before invoking reactor wakers"
        );
        assert!(
            timer_loop_body.contains("wait_timer_driver(driver, heap)"),
            "timer driver idle wait must use the runtime-native no-root helper"
        );
        assert!(
            timer_loop_body.contains("wait_timer_driver_timeout(driver, heap, wait)"),
            "timer driver deadline wait must use the runtime-native no-root helper"
        );
    }

    #[test]
    fn reactor_and_timer_locks_use_runtime_no_root_bracketing() {
        let source = include_str!("async_rt.rs");
        assert!(
            source.contains("fn runtime_lock_no_roots<T>(mutex: &Mutex<T>)"),
            "async runtime needs a no-root lock helper for private reactor/timer mutexes"
        );
        assert!(
            source.contains("gc::enter_runtime_native_no_roots();"),
            "private runtime lock contention must not leave a mutator marked RUNNING"
        );
        for required in [
            "runtime_lock_no_roots(&r.waiters).insert(",
            "runtime_lock_no_roots(&reactor().waiters).remove(&reg.id)",
            "runtime_lock_no_roots(&reactor().waiters).remove(&id)",
            "runtime_lock_no_roots(&self.registration)",
            "runtime_lock_no_roots(&driver.heap)",
        ] {
            assert!(
                source.contains(required),
                "missing runtime-native lock bracketing for `{required}`"
            );
        }
    }

    fn collect_runtime_exports(source: &str, out: &mut Vec<String>) {
        let mut lines = source.lines();
        while let Some(line) = lines.next() {
            let trimmed = line.trim_start();
            for prefix in ["pub extern \"C\" fn ", "pub unsafe extern \"C\" fn "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    if let Some(name) = rest
                        .split(|ch: char| ch == '(' || ch == '<' || ch.is_whitespace())
                        .next()
                    {
                        if name.starts_with("lang_") {
                            out.push(name.to_string());
                        }
                    }
                }
            }

            if trimmed == "fs_path_async_export!(" {
                for macro_line in lines.by_ref() {
                    let candidate = macro_line
                        .trim()
                        .trim_end_matches(',')
                        .trim_end_matches(';');
                    if candidate.starts_with("lang_") {
                        out.push(candidate.to_string());
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn runtime_wait_capable_exports_stay_future_constructors() {
        let mut exports = Vec::new();
        for source in [
            include_str!("async_rt.rs"),
            include_str!("channels.rs"),
            include_str!("fs.rs"),
            include_str!("net.rs"),
            include_str!("process.rs"),
            include_str!("rand.rs"),
            include_str!("shared.rs"),
            include_str!("strings.rs"),
            include_str!("threads.rs"),
            include_str!("time.rs"),
        ] {
            collect_runtime_exports(source, &mut exports);
        }
        let exports: std::collections::BTreeSet<_> = exports.into_iter().collect();

        for required in [
            "lang_async_sleep",
            "lang_async_timeout",
            "lang_async_yield",
            "lang_chan_recv_future",
            "lang_shared_lock_future",
            "lang_thread_join_future",
            "lang_io_stdin_read_async",
            "lang_io_stdin_read_to_end_async",
            "lang_io_stdout_write_async",
            "lang_io_stderr_write_async",
            "lang_io_stdout_flush_async",
            "lang_io_stderr_flush_async",
            "lang_fs_file_open_async",
            "lang_fs_file_close_async",
            "lang_fs_file_read_async",
            "lang_fs_file_read_to_end_async",
            "lang_fs_file_write_async",
            "lang_fs_file_flush_async",
            "lang_fs_file_seek_async",
            "lang_fs_read_text_async",
            "lang_fs_write_text_async",
            "lang_fs_append_text_async",
            "lang_fs_read_bytes_async",
            "lang_fs_write_bytes_async",
            "lang_fs_exists_async",
            "lang_fs_is_file_async",
            "lang_fs_is_dir_async",
            "lang_fs_kind_async",
            "lang_fs_len_async",
            "lang_fs_read_only_async",
            "lang_fs_executable_async",
            "lang_fs_remove_async",
            "lang_fs_rename_async",
            "lang_fs_create_dir_async",
            "lang_fs_create_dir_all_async",
            "lang_fs_canonicalize_async",
            "lang_fs_read_dir_async",
            "lang_rand_os_bytes_async",
            "lang_time_monotonic_nanos_async",
            "lang_time_system_nanos_async",
            "lang_time_local_offset_seconds_async",
            "lang_process_args_async",
            "lang_process_env_async",
            "lang_process_env_all_async",
            "lang_process_set_env_async",
            "lang_process_status_async",
            "lang_process_output_async",
            "lang_process_spawn_async",
            "lang_process_child_wait_async",
            "lang_process_child_kill_async",
            "lang_net_resolve_async",
            "lang_net_tcp_connect_async",
            "lang_net_tcp_connect_timeout_async",
            "lang_net_tcp_stream_read_async",
            "lang_net_tcp_stream_read_to_end_async",
            "lang_net_tcp_stream_write_async",
            "lang_net_tcp_stream_write_all_async",
            "lang_net_tcp_stream_flush_async",
            "lang_net_tcp_stream_peek_async",
            "lang_net_tcp_stream_close_async",
            "lang_net_tcp_stream_peer_addr_async",
            "lang_net_tcp_stream_local_addr_async",
            "lang_net_tcp_stream_take_error_async",
            "lang_net_tcp_stream_nodelay_async",
            "lang_net_tcp_stream_set_nodelay_async",
            "lang_net_tcp_stream_set_nonblocking_async",
            "lang_net_tcp_stream_read_timeout_async",
            "lang_net_tcp_stream_set_read_timeout_async",
            "lang_net_tcp_stream_write_timeout_async",
            "lang_net_tcp_stream_set_write_timeout_async",
            "lang_net_tcp_stream_ttl_async",
            "lang_net_tcp_stream_set_ttl_async",
            "lang_net_tcp_listener_bind_async",
            "lang_net_tcp_listener_close_async",
            "lang_net_tcp_listener_accept_async",
            "lang_net_tcp_listener_local_addr_async",
            "lang_net_tcp_listener_take_error_async",
            "lang_net_tcp_listener_set_nonblocking_async",
            "lang_net_tcp_listener_ttl_async",
            "lang_net_tcp_listener_set_ttl_async",
            "lang_net_udp_bind_async",
            "lang_net_udp_close_async",
            "lang_net_udp_connect_async",
            "lang_net_udp_local_addr_async",
            "lang_net_udp_peer_addr_async",
            "lang_net_udp_take_error_async",
            "lang_net_udp_set_nonblocking_async",
            "lang_net_udp_read_timeout_async",
            "lang_net_udp_set_read_timeout_async",
            "lang_net_udp_write_timeout_async",
            "lang_net_udp_set_write_timeout_async",
            "lang_net_udp_ttl_async",
            "lang_net_udp_set_ttl_async",
            "lang_net_udp_broadcast_async",
            "lang_net_udp_set_broadcast_async",
            "lang_net_udp_multicast_loop_v4_async",
            "lang_net_udp_set_multicast_loop_v4_async",
            "lang_net_udp_multicast_loop_v6_async",
            "lang_net_udp_set_multicast_loop_v6_async",
            "lang_net_udp_multicast_ttl_v4_async",
            "lang_net_udp_set_multicast_ttl_v4_async",
            "lang_net_udp_join_multicast_v4_async",
            "lang_net_udp_leave_multicast_v4_async",
            "lang_net_udp_join_multicast_v6_async",
            "lang_net_udp_leave_multicast_v6_async",
            "lang_net_udp_send_async",
            "lang_net_udp_recv_async",
            "lang_net_udp_peek_async",
            "lang_net_udp_send_to_async",
            "lang_net_udp_recv_from_async",
            "lang_net_udp_peek_from_async",
        ] {
            assert!(
                exports.contains(required),
                "runtime export catalog must include future constructor `{required}`; exports={exports:?}"
            );
        }

        for name in &exports {
            let allowed_plain = matches!(
                name.as_str(),
                "lang_fs_native_separator"
                    | "lang_net_tcp_stream_release"
                    | "lang_net_tcp_listener_release"
                    | "lang_net_udp_release"
                    | "lang_process_child_release"
            );
            let wait_capable_family = name.starts_with("lang_io_")
                || name.starts_with("lang_fs_")
                || name.starts_with("lang_net_")
                || name.starts_with("lang_process_")
                || name.starts_with("lang_rand_")
                || name.starts_with("lang_time_");
            if wait_capable_family && !allowed_plain {
                assert!(
                    name.ends_with("_async"),
                    "wait-capable runtime export `{name}` must remain a future-constructor ABI"
                );
            }
        }

        for async_name in exports.iter().filter(|name| {
            name.ends_with("_async")
                && (name.starts_with("lang_io_")
                    || name.starts_with("lang_fs_")
                    || name.starts_with("lang_net_")
                    || name.starts_with("lang_process_")
                    || name.starts_with("lang_rand_")
                    || name.starts_with("lang_time_"))
        }) {
            let retired = async_name.trim_end_matches("_async");
            assert!(
                !exports.contains(retired),
                "retired ordinary-result runtime export `{retired}` must not coexist with `{async_name}`"
            );
        }
        for retired in [
            "lang_chan_recv",
            "lang_shared_lock",
            "lang_thread_join",
            "lang_process_child_wait",
            "lang_process_child_kill",
        ] {
            assert!(
                !exports.contains(retired),
                "retired wait-capable runtime export `{retired}` must not reappear"
            );
        }
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
    fn drive_root_future_returns_immediately_ready() {
        // vtable: one slot = poll fn address.
        let vtable: Box<[usize; 1]> = Box::new([ready_poll as *const () as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data: Box<[i64; 1]> = Box::new([0]); // unused state
        let data_ptr = Box::into_raw(data) as usize;
        // interface-object box: [vtable][data][type_id].
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        let fut_ptr = Box::into_raw(fut) as *mut u8;
        // Pending type id is 9 here; the Ready box carries 7, so it is Ready.
        let out = unsafe { lang_drive_root_future(fut_ptr, 9) };
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
    fn drive_root_future_parks_then_resumes_on_wake() {
        POLLS.store(0, std::sync::atomic::Ordering::SeqCst);
        let vtable: Box<[usize; 1]> = Box::new([yield_once_poll as *const () as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data_ptr = Box::into_raw(Box::new([0i64; 1])) as usize;
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        let fut_ptr = Box::into_raw(fut) as *mut u8;
        let out = unsafe { lang_drive_root_future(fut_ptr, 9) };
        assert_eq!(out, 7);
        assert_eq!(POLLS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn drive_root_future_pending_wait_uses_gc_native_boundary() {
        let source = include_str!("async_rt.rs");
        let body = source
            .split(
                "pub unsafe extern \"C\" fn lang_drive_root_future(fut: *mut u8, pending_tid: i64) -> i64",
            )
            .nth(1)
            .and_then(|rest| rest.split("// -- yield_now").next())
            .expect("lang_drive_root_future should remain in async_rt.rs");
        assert!(
            body.contains("gc::enter_native();"),
            "root driver must enter GC native state before parking on Pending"
        );
        assert!(
            body.contains("woken = wait_unpoison(&waker.cv, woken);"),
            "root driver must park on the root-driver condvar through the wait helper"
        );
        assert!(
            body.contains("gc::leave_native();"),
            "root driver must leave GC native state after the condvar wait resumes"
        );
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
    fn cancelling_completed_tcp_connect_future_releases_result_stream_and_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let encoded = crate::net::net_tcp_connect_encoded(addr);
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("loopback connect should register a TCP stream");
        assert!(crate::net::test_stream_handle_registered(handle));

        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::NetTcpConnect {
                addr: String::new(),
            },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        assert!(!crate::net::test_stream_handle_registered(handle));
    }

    #[test]
    fn cancelling_completed_tcp_listener_bind_future_releases_result_listener_and_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let encoded = crate::net::net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("loopback listener bind should register a TCP listener");
        assert!(crate::net::test_listener_handle_registered(handle));

        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::NetTcpListenerBind {
                addr: "127.0.0.1:0".to_string(),
            },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        assert!(!crate::net::test_listener_handle_registered(handle));
    }

    #[test]
    fn cancelling_completed_udp_bind_future_releases_result_socket_and_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let encoded = crate::net::net_udp_bind_encoded("127.0.0.1:0".to_string());
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("loopback UDP bind should register a UDP socket");
        assert!(crate::net::test_udp_handle_registered(handle));

        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::NetUdpBind {
                addr: "127.0.0.1:0".to_string(),
            },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        assert!(!crate::net::test_udp_handle_registered(handle));
    }

    #[test]
    fn cancelling_completed_tcp_accept_future_releases_result_stream_and_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let bind_encoded = crate::net::net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        let listener_handle = encoded_success_payload(&bind_encoded)
            .and_then(parse_i64_payload)
            .expect("loopback listener bind should register a TCP listener");

        let local_encoded = crate::net::net_tcp_listener_local_addr_encoded(listener_handle);
        let local_addr = encoded_success_payload(&local_encoded)
            .expect("listener local address should encode successfully")
            .to_string();
        let client = std::thread::spawn(move || std::net::TcpStream::connect(local_addr).unwrap());
        let encoded = crate::net::net_tcp_listener_accept_encoded(listener_handle);
        let _client_stream = client.join().expect("loopback client should connect");
        let handle = encoded_success_payload(&encoded)
            .and_then(first_encoded_field)
            .and_then(parse_i64_payload)
            .expect("loopback accept should register a TCP stream");
        assert!(crate::net::test_stream_handle_registered(handle));

        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::NetTcpListenerAccept {
                handle: listener_handle,
            },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        assert!(!crate::net::test_stream_handle_registered(handle));
        crate::net::lang_net_tcp_listener_release(listener_handle);
    }

    #[test]
    fn cancelling_completed_udp_recv_from_future_removes_result_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let receiver_encoded = crate::net::net_udp_bind_encoded("127.0.0.1:0".to_string());
        let receiver_handle = encoded_success_payload(&receiver_encoded)
            .and_then(parse_i64_payload)
            .expect("loopback receiver bind should register a UDP socket");

        let sender_encoded = crate::net::net_udp_bind_encoded("127.0.0.1:0".to_string());
        let sender_handle = encoded_success_payload(&sender_encoded)
            .and_then(parse_i64_payload)
            .expect("loopback sender bind should register a UDP socket");

        let receiver_addr = crate::net::net_udp_local_addr_encoded(receiver_handle);
        let receiver_addr = encoded_success_payload(&receiver_addr)
            .expect("receiver local address should encode successfully")
            .to_string();
        assert_eq!(
            encoded_success_payload(&crate::net::net_udp_send_to_encoded(
                sender_handle,
                "70696e67".to_string(),
                receiver_addr,
            ))
            .and_then(parse_i64_payload),
            Some(4)
        );

        let encoded = crate::net::net_udp_recv_from_encoded(receiver_handle, 4);
        assert!(
            encoded_success_payload(&encoded).is_some(),
            "loopback UDP recv_from should encode success: {encoded}"
        );
        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::NetUdpRecvFrom {
                handle: receiver_handle,
                count: 4,
            },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        crate::net::lang_net_udp_release(sender_handle);
        crate::net::lang_net_udp_release(receiver_handle);
    }

    #[test]
    fn cancelling_completed_fs_file_read_to_end_future_removes_result_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "otter_fusion_async_fs_cancel_{}_{}.bin",
            std::process::id(),
            IO_NEXT.load(Ordering::Relaxed)
        ));
        std::fs::write(&path, b"otter").unwrap();

        let path = path.to_string_lossy().into_owned();
        let opened = crate::fs::fs_file_open_encoded(path.clone(), "open".to_string());
        let handle = encoded_success_payload(&opened)
            .and_then(parse_i64_payload)
            .expect("temp file open should register a descriptor handle");

        let encoded = crate::fs::fs_file_read_to_end_encoded(handle);
        assert_eq!(
            encoded_success_payload(&encoded),
            Some("6f74746572"),
            "fs read_to_end should encode a byte-buffer payload"
        );
        let ptr = unsafe { lang_str_from_utf8(encoded.as_ptr(), encoded.len()) } as usize;
        gc::add_extra_root(ptr);
        assert_eq!(gc::extra_root_count_for(ptr), 1);

        let id = IO_NEXT.fetch_add(1, Ordering::Relaxed);
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::FsFileReadToEnd { handle },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(Some(ptr)),
            registration: Mutex::new(None),
        });
        lock_unpoison(io_registry()).insert(id, cell.clone());

        io_cancel_id(id);

        assert!(!lock_unpoison(io_registry()).contains_key(&id));
        assert!(cell.cancelled.load(Ordering::SeqCst));
        assert_eq!(gc::extra_root_count_for(ptr), 0);
        assert_eq!(
            crate::fs::fs_file_close_encoded(handle),
            "0",
            "cancelling a completed fs read result must not close the descriptor"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn io_completion_roots_result_and_wakes_without_thread_wide_native_leave() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);
        let reg_id = reg.id();
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::StdoutFlush,
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            result: Mutex::new(None),
            registration: Mutex::new(Some(reg)),
        });

        io_complete(cell.clone(), "0".to_string());

        assert!(!reactor_has_waiter(reg_id));
        assert_eq!(*waker.count.lock().unwrap(), 1);
        let ptr = lock_unpoison(&cell.result)
            .take()
            .expect("completion should store the encoded result");
        assert_eq!(gc::extra_root_count_for(ptr), 1);
        gc::remove_extra_root(ptr);
    }

    #[test]
    fn cancelling_late_stdio_flush_result_drops_payload_and_skips_wake() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);
        let reg_id = reg.id();
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::StdoutFlush,
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(true),
            result: Mutex::new(None),
            registration: Mutex::new(Some(reg.clone())),
        });

        io_complete(cell.clone(), "0".to_string());

        assert!(
            lock_unpoison(&cell.result).is_none(),
            "a late cancelled stdio result must not be rooted or stored"
        );
        assert_eq!(
            *waker.count.lock().unwrap(),
            0,
            "a late cancelled stdio result must not wake the cancelled waiter"
        );
        assert!(
            reactor_cancel(&reg),
            "the simulated cancellation owner should still be able to drain the waiter"
        );
        assert!(!reactor_has_waiter(reg_id));
    }

    #[test]
    fn cancelling_late_tcp_connect_result_releases_registered_stream() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let encoded = crate::net::net_tcp_connect_encoded(addr);
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("loopback connect should register a TCP stream");

        assert!(crate::net::test_stream_handle_registered(handle));
        io_discard_cancelled_result(
            &IoOp::NetTcpConnect {
                addr: String::new(),
            },
            &encoded,
        );
        assert!(!crate::net::test_stream_handle_registered(handle));
    }

    #[test]
    fn cancelling_late_tcp_connect_timeout_result_releases_registered_stream() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let encoded = crate::net::net_tcp_connect_timeout_encoded(addr, 1_000_000_000);
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("timed loopback connect should register a TCP stream");

        assert!(crate::net::test_stream_handle_registered(handle));
        io_discard_cancelled_result(
            &IoOp::NetTcpConnectTimeout {
                addr: String::new(),
                nanos: 1_000_000_000,
            },
            &encoded,
        );
        assert!(!crate::net::test_stream_handle_registered(handle));
    }

    #[test]
    fn cancelling_late_tcp_listener_bind_result_releases_registered_listener() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let encoded = crate::net::net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("loopback listener bind should register a TCP listener");

        assert!(crate::net::test_listener_handle_registered(handle));
        io_discard_cancelled_result(
            &IoOp::NetTcpListenerBind {
                addr: "127.0.0.1:0".to_string(),
            },
            &encoded,
        );
        assert!(!crate::net::test_listener_handle_registered(handle));
    }

    #[test]
    fn cancelling_late_tcp_accept_result_releases_registered_stream() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let bind_encoded = crate::net::net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        let listener_handle = encoded_success_payload(&bind_encoded)
            .and_then(parse_i64_payload)
            .expect("loopback listener bind should register a TCP listener");

        let local_encoded = crate::net::net_tcp_listener_local_addr_encoded(listener_handle);
        let local_addr = encoded_success_payload(&local_encoded)
            .expect("listener local address should encode successfully")
            .to_string();
        let client = std::thread::spawn(move || std::net::TcpStream::connect(local_addr).unwrap());
        let encoded = crate::net::net_tcp_listener_accept_encoded(listener_handle);
        let _client_stream = client.join().expect("loopback client should connect");
        let handle = encoded_success_payload(&encoded)
            .and_then(first_encoded_field)
            .and_then(parse_i64_payload)
            .expect("loopback accept should register a TCP stream");

        assert!(crate::net::test_stream_handle_registered(handle));
        io_discard_cancelled_result(
            &IoOp::NetTcpListenerAccept {
                handle: listener_handle,
            },
            &encoded,
        );
        assert!(!crate::net::test_stream_handle_registered(handle));
        crate::net::lang_net_tcp_listener_release(listener_handle);
    }

    #[test]
    fn cancelling_late_fs_file_open_result_releases_registered_descriptor() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "otter_fusion_async_fs_open_cancel_{}_{}.bin",
            std::process::id(),
            IO_NEXT.load(Ordering::Relaxed)
        ));
        let path = path.to_string_lossy().into_owned();
        let encoded = crate::fs::fs_file_open_encoded(path.clone(), "create".to_string());
        let handle = encoded_success_payload(&encoded)
            .and_then(parse_i64_payload)
            .expect("temp file open should register a descriptor handle");

        assert!(crate::fs::test_file_handle_registered(handle));
        io_discard_cancelled_result(
            &IoOp::FsFileOpen {
                path: String::new(),
                mode: String::new(),
            },
            &encoded,
        );
        assert!(!crate::fs::test_file_handle_registered(handle));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancelling_late_fs_file_read_to_end_result_drops_payload_without_closing_descriptor() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "otter_fusion_async_fs_late_cancel_{}_{}.bin",
            std::process::id(),
            IO_NEXT.load(Ordering::Relaxed)
        ));
        std::fs::write(&path, b"late").unwrap();

        let path = path.to_string_lossy().into_owned();
        let opened = crate::fs::fs_file_open_encoded(path.clone(), "open".to_string());
        let handle = encoded_success_payload(&opened)
            .and_then(parse_i64_payload)
            .expect("temp file open should register a descriptor handle");
        let encoded = crate::fs::fs_file_read_to_end_encoded(handle);
        assert_eq!(encoded_success_payload(&encoded), Some("6c617465"));

        let waker = Box::leak(Box::new(CountWaker {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }));
        let reg = reactor_register(waker as *const CountWaker as usize, count_wake);
        let reg_id = reg.id();
        let cell = std::sync::Arc::new(IoCell {
            op: IoOp::FsFileReadToEnd { handle },
            started: AtomicBool::new(true),
            cancelled: AtomicBool::new(true),
            result: Mutex::new(None),
            registration: Mutex::new(Some(reg.clone())),
        });

        io_complete(cell.clone(), encoded);

        assert!(
            lock_unpoison(&cell.result).is_none(),
            "a late cancelled fs result must not be rooted or stored"
        );
        assert_eq!(
            *waker.count.lock().unwrap(),
            0,
            "a late cancelled fs result must not wake the cancelled waiter"
        );
        assert!(
            reactor_cancel(&reg),
            "the simulated cancellation owner should still be able to drain the waiter"
        );
        assert!(!reactor_has_waiter(reg_id));
        assert_eq!(
            crate::fs::fs_file_close_encoded(handle),
            "0",
            "discarding a late fs read result must not close the descriptor"
        );
        let _ = std::fs::remove_file(path);
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
