//! OS threads for `Thread.spawn` / `JoinHandle.join` (`docs/20` §1) — **async
//! and non-blocking on the joiner side**.
//!
//! A spawned thread runs a lifted closure — a function `(env) -> R` whose first
//! env word is the function pointer (the closure ABI from `docs/09`). The
//! spawning thread hands the environment over; the child runs it on a fresh OS
//! thread. When the closure is **async** (`() -> Future<R>`,
//! [`lang_thread_spawn_async`]) the worker calls it to build the future and then
//! polls that future until it resolves on its own thread, publishing the awaited
//! `R` — so the same `JoinHandle<R>` machinery serves both flavors.
//! Results (and the environment) are pinned as global GC roots
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
//!
//! **Worker-panic isolation** (`docs/14`, `docs/20` §1, `docs/21` §11): every
//! worker runs its body under a `setjmp`/`longjmp` panic boundary
//! ([`crate::panic_boundary::run_under_boundary`]). A `panic` inside the worker
//! unwinds to that boundary rather than aborting the process; the boundary
//! restores GC/lock invariants and [`publish_panic`] records the message. The
//! worker then surfaces as `Panicked { message }` to a `join()`er, while a
//! `spawn EXPR` awaiter has the panic *re-propagated* at its own `await`
//! ([`spawn_poll`]) — the JS/Dart "promise rejection" model. Sibling workers
//! are unaffected.

use crate::gc;
#[cfg(test)]
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
    TryLockError,
};
use std::thread::JoinHandle as OsJoin;
#[cfg(test)]
use std::time::Duration;

/// A waker captured from a poll [`Context`]: the `(waker_data, wake_fn)` pair a
/// completing worker invokes to re-poll a suspended joiner.
type Waker = (usize, extern "C" fn(*mut u8));

#[cfg(test)]
thread_local! {
    static PRE_POLL_UNLOCK_HOOK: RefCell<Option<Box<dyn Fn(&Task)>>> = RefCell::new(None);
}

#[cfg(test)]
fn run_pre_poll_unlock_hook(task: &Task) {
    PRE_POLL_UNLOCK_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(task);
        }
    });
}

#[cfg(not(test))]
fn run_pre_poll_unlock_hook(_task: &Task) {}

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
    /// Whether a successful result is a managed pointer that needs a cross-task
    /// handoff root until the joiner boxes it into a traced result graph.
    result_is_ptr: bool,
    /// Whether [`publish_done`] pinned `result` as an extra root.
    result_pinned: bool,
    /// True if the worker panicked (its body unwound to the panic boundary).
    panicked: bool,
    /// True if an executor task was cooperatively cancelled before completion.
    cancelled: bool,
    /// The worker's panic message — a GC-pinned `str` field-block pointer, valid
    /// when `panicked`. Pinned by the boundary for the cross-thread handoff and
    /// unpinned by the joiner once it has been copied into the `Panicked` box.
    message: usize,
    /// Wakers from suspended `join()` futures.
    waiters: Vec<Waker>,
    /// The OS thread handle, taken on the first `Ready` poll for cleanup.
    os: Option<OsJoin<()>>,
    /// Cancellation state for executor-backed tasks. `Thread.spawn` workers keep
    /// this `None`; they deliberately have no hard-kill cancellation hook.
    task_cancel: Option<TaskCancelCtl>,
    /// The handle was detached, so no future joiner can consume a result/panic
    /// handoff. Detached workers still continue independently, but terminal values
    /// are discarded instead of pinned forever.
    detached: bool,
}

struct ThreadCtl {
    inner: Mutex<ThreadInner>,
}

fn ctl_lock(ctl: &ThreadCtl) -> MutexGuard<'_, ThreadInner> {
    ctl.inner.lock().unwrap_or_else(|err| err.into_inner())
}

fn register_waiter(waiters: &mut Vec<Waker>, waiter: Waker) {
    let (data, wake) = waiter;
    let wake_addr = wake as *const () as usize;
    if !waiters.iter().any(|(existing_data, existing_wake)| {
        *existing_data == data && *existing_wake as *const () as usize == wake_addr
    }) {
        waiters.push(waiter);
    }
}

#[derive(Clone)]
struct TaskCancelCtl {
    task_id: u64,
    cancel_requested: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    reschedule: Arc<AtomicBool>,
    held_locks: Arc<Mutex<Vec<u64>>>,
    poll_lock: Arc<Mutex<()>>,
    input: usize,
    input_kind: TaskCancelInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskCancelInput {
    Future,
    OrdinaryClosure,
}

fn registry() -> &'static RwLock<HashMap<u64, Arc<ThreadCtl>>> {
    static R: OnceLock<RwLock<HashMap<u64, Arc<ThreadCtl>>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

const TASK_WAKER_SHARDS: usize = 64;

fn task_waker_shards() -> &'static [RwLock<HashMap<u64, Task>>] {
    static R: OnceLock<Vec<RwLock<HashMap<u64, Task>>>> = OnceLock::new();
    R.get_or_init(|| {
        (0..TASK_WAKER_SHARDS)
            .map(|_| RwLock::new(HashMap::new()))
            .collect()
    })
    .as_slice()
}

fn task_waker_shard(task_id: u64) -> &'static RwLock<HashMap<u64, Task>> {
    &task_waker_shards()[task_id as usize % TASK_WAKER_SHARDS]
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

fn new_ctl() -> Arc<ThreadCtl> {
    new_ctl_with_result_is_ptr(true)
}

fn new_ctl_with_result_is_ptr(result_is_ptr: bool) -> Arc<ThreadCtl> {
    Arc::new(ThreadCtl {
        inner: Mutex::new(ThreadInner {
            done: false,
            taken: false,
            result: 0,
            result_is_ptr,
            result_pinned: false,
            panicked: false,
            cancelled: false,
            message: 0,
            waiters: Vec::new(),
            os: None,
            task_cancel: None,
            detached: false,
        }),
    })
}

fn spawn_os_thread_native_wait<F>(f: F) -> OsJoin<()>
where
    F: FnOnce() + Send + 'static,
{
    gc::native_wait(|| std::thread::spawn(f))
}

/// Publish a worker's result and wake every joiner suspended on it.
fn publish_done(ctl: &ThreadCtl, result: i64) {
    let wakers = {
        let mut g = ctl_lock(ctl);
        if !g.detached && g.result_is_ptr && result != 0 {
            gc::add_extra_root(result as usize);
            g.result_pinned = true;
        }
        g.result = result;
        g.done = true;
        std::mem::take(&mut g.waiters)
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

/// Publish a worker's panic and wake every joiner suspended on it. `message` is
/// a GC-pinned `str` field-block pointer (the panic message), kept pinned until
/// the joiner copies it into the `Panicked` box.
fn publish_panic(ctl: &ThreadCtl, message: usize) {
    let wakers = {
        let mut g = ctl_lock(ctl);
        if g.detached {
            gc::remove_extra_root(message);
        }
        g.panicked = true;
        g.message = message;
        g.done = true;
        std::mem::take(&mut g.waiters)
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

/// Publish executor-task cancellation and wake every waiter suspended on the
/// task's `JoinHandle` / spawn future.
fn publish_cancelled(ctl: &ThreadCtl) {
    let wakers = {
        let mut g = ctl_lock(ctl);
        if g.done {
            return;
        }
        g.cancelled = true;
        g.done = true;
        std::mem::take(&mut g.waiters)
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

/// Common worker tail (`docs/20` §1): drop the spawner's pin on the worker's
/// input (`env` or `fut`) and publish the [`run_under_boundary`] outcome to the
/// joiner — either the (now-pinned) result or the captured panic message.
///
/// [`run_under_boundary`]: crate::panic_boundary::run_under_boundary
fn finish_worker(ctl: &ThreadCtl, input_addr: usize, outcome: Result<i64, usize>) {
    match outcome {
        Ok(result) => {
            publish_done(ctl, result);
            gc::remove_extra_root(input_addr);
        }
        Err(message) => {
            // The boundary already released held locks + unwind pins and pinned
            // `message`. Drop the spawner's input pin and report the panic.
            // Detached workers discard the message in `publish_panic`, under the
            // same mutex that coordinates detach vs. completion.
            publish_panic(ctl, message);
            gc::remove_extra_root(input_addr);
        }
    }
}

/// Spawn `() => R` on a new OS thread. `env` is the closure environment
/// (`[fn_ptr][captures…]`); the function pointer is its first word. Returns a
/// registry id for the resulting `JoinHandle<R>`.
///
/// `float_kind` selects the lifted function's result ABI so a float result is
/// read from the correct register and carried as its raw bit pattern:
/// `0` → `extern "C" fn(*mut u8) -> i64` (integers / pointers), `8` → `-> f64`
/// (stored as `f64::to_bits`), `4` → `-> f32` (stored as `f32::to_bits`). The
/// `Joined<R>.value` slot is byte-identical to the float's representation, so
/// the joiner reads it back as the float with no further conversion.
///
/// # Safety
/// `env` must be a valid closure environment whose lifted function has the
/// signature implied by `float_kind`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_spawn(env: *mut u8, float_kind: i64) -> u64 {
    // Pin the environment as soon as it crosses from generated code into the
    // runtime. Runtime Rust frames are not precise language stack-map frames, so
    // another mutator's stress collection must see this handoff explicitly.
    gc::add_extra_root(env as usize);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = new_ctl();
    runtime_write_lock(registry()).insert(id, ctl.clone());

    let env_addr = env as usize;

    let worker = ctl.clone();
    let os = spawn_os_thread_native_wait(move || {
        // Register as a mutator and gate on the world barrier before touching
        // managed memory, so the collector always accounts for this thread.
        gc::thread_start();
        // Run the closure under the panic boundary so a `panic` in the worker
        // is isolated to it (surfaced as `Panicked` on `join`) instead of
        // aborting the process.
        let outcome = crate::panic_boundary::run_under_boundary(|| {
            let fn_ptr = unsafe { (env_addr as *const usize).read() };
            match float_kind {
                8 => {
                    let f: extern "C" fn(*mut u8) -> f64 = unsafe { std::mem::transmute(fn_ptr) };
                    f(env_addr as *mut u8).to_bits() as i64
                }
                4 => {
                    let f: extern "C" fn(*mut u8) -> f32 = unsafe { std::mem::transmute(fn_ptr) };
                    f(env_addr as *mut u8).to_bits() as i64
                }
                _ => {
                    let f: extern "C" fn(*mut u8) -> i64 = unsafe { std::mem::transmute(fn_ptr) };
                    f(env_addr as *mut u8)
                }
            }
        });
        finish_worker(&worker, env_addr, outcome);
    });
    ctl_lock(&ctl).os = Some(os);
    id
}

/// Spawn `fut` (a `Future<T>` interface-object box) onto a new OS worker that
/// polls it until it resolves via the executor, returning a registry id for the
/// resulting `JoinHandle<T>` (`docs/21` §6 — `spawn`). `pending_tid` is the
/// `Pending` type id the private root driver needs.
///
/// # Safety
/// `fut` must be a valid `Future<T>` box (vtable slot 0 = `poll`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_async_spawn(
    fut: *mut u8,
    pending_tid: i64,
    value_is_ptr: i64,
) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = new_ctl_with_result_is_ptr(value_is_ptr != 0);
    let held_locks = Arc::new(Mutex::new(Vec::new()));
    let poll_lock = Arc::new(Mutex::new(()));
    let polling = Arc::new(AtomicBool::new(false));
    let queued = Arc::new(AtomicBool::new(false));
    let reschedule = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let cancel_requested = Arc::new(AtomicBool::new(false));
    ctl_lock(&ctl).task_cancel = Some(TaskCancelCtl {
        task_id,
        cancel_requested: cancel_requested.clone(),
        done: done.clone(),
        reschedule: reschedule.clone(),
        held_locks: held_locks.clone(),
        poll_lock: poll_lock.clone(),
        input: fut as usize,
        input_kind: TaskCancelInput::Future,
    });
    runtime_write_lock(registry()).insert(id, ctl.clone());

    // Pin the future across the executor handoff. The task releases this root
    // when it reaches a terminal state, mirroring `finish_worker`.
    gc::add_extra_root(fut as usize);
    let task = Task {
        id: task_id,
        work: TaskWork::Future {
            fut: fut as usize,
            pending_tid,
        },
        ctl,
        held_locks,
        poll_lock,
        polling,
        queued,
        reschedule,
        done,
        cancel_requested,
    };
    register_task_waker(task.clone());
    executor().spawn(task);
    id
}

// -- M:N async executor -----------------------------------------------------
//
// The executor below is the runtime substrate for `spawn EXPR` and, through the
// compiler-facing `lang_task_spawn*` aliases, `Task.spawn`. It deliberately does
// not serve `Thread.spawn`: OS threads remain the dedicated-thread primitive.
//
// Shape: a small, lazily-started worker pool; each worker owns a local FIFO run
// queue, there is a global injector for external submissions/wakes, and idle
// workers steal from their siblings before sleeping on the injector condvar.

#[derive(Clone)]
enum TaskWork {
    Future { fut: usize, pending_tid: i64 },
    OrdinaryClosure { env: usize, float_kind: i64 },
}

#[derive(Clone)]
struct Task {
    id: u64,
    work: TaskWork,
    ctl: Arc<ThreadCtl>,
    held_locks: Arc<Mutex<Vec<u64>>>,
    poll_lock: Arc<Mutex<()>>,
    polling: Arc<AtomicBool>,
    queued: Arc<AtomicBool>,
    reschedule: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
}

struct Worker {
    queue: Mutex<VecDeque<Task>>,
}

struct Injector {
    queue: Mutex<VecDeque<Task>>,
    sleep_epoch: Mutex<u64>,
    sleepers: AtomicUsize,
    cv: Condvar,
}

struct Executor {
    injector: Injector,
    workers: Vec<Worker>,
    next: AtomicU64,
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

fn runtime_try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(err)) => Some(err.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

fn runtime_read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    loop {
        match lock.try_read() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(err)) => return err.into_inner(),
            Err(TryLockError::WouldBlock) => {}
        }
        gc::runtime_safepoint();
        std::thread::yield_now();
    }
}

fn runtime_write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    loop {
        match lock.try_write() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(err)) => return err.into_inner(),
            Err(TryLockError::WouldBlock) => {}
        }
        gc::runtime_safepoint();
        std::thread::yield_now();
    }
}

fn executor() -> &'static Executor {
    static EXEC: OnceLock<Executor> = OnceLock::new();
    let exec = EXEC.get_or_init(Executor::start);
    EXEC_READY.store(true, Ordering::Release);
    exec
}

static EXEC_READY: AtomicBool = AtomicBool::new(false);

impl Executor {
    fn new_unstarted(worker_count: usize) -> Self {
        assert!(worker_count > 0, "executor needs at least one worker");
        Self {
            injector: Injector {
                queue: Mutex::new(VecDeque::new()),
                sleep_epoch: Mutex::new(0),
                sleepers: AtomicUsize::new(0),
                cv: Condvar::new(),
            },
            workers: (0..worker_count)
                .map(|_| Worker {
                    queue: Mutex::new(VecDeque::new()),
                })
                .collect(),
            next: AtomicU64::new(0),
        }
    }

    fn start() -> Self {
        let worker_count = executor_worker_count();
        let exec = Self::new_unstarted(worker_count);
        for id in 0..worker_count {
            let _ = spawn_os_thread_native_wait(move || worker_loop(id));
        }
        exec
    }

    fn spawn(&self, task: Task) {
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.workers.len();
        task.queued.store(true, Ordering::Release);
        runtime_lock(&self.workers[idx].queue).push_back(task);
        self.wake_one_sleeper();
    }

    fn inject(&self, task: Task) {
        task.queued.store(true, Ordering::Release);
        self.inject_marked(task);
    }

    fn inject_marked(&self, task: Task) {
        runtime_lock(&self.injector.queue).push_back(task);
        self.wake_one_sleeper();
    }

    fn wake_one_sleeper(&self) {
        if self.injector.sleepers.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut epoch = runtime_lock(&self.injector.sleep_epoch);
        *epoch = epoch.wrapping_add(1);
        self.injector.cv.notify_one();
    }
}

fn executor_worker_count() -> usize {
    if let Some(count) = std::env::var("OTTER_FUSION_TASK_WORKERS")
        .ok()
        .and_then(|raw| parse_worker_count_override(&raw))
    {
        return count;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 32)
}

fn parse_worker_count_override(raw: &str) -> Option<usize> {
    let count = raw.trim().parse::<usize>().ok()?;
    if count == 0 {
        None
    } else {
        Some(count.clamp(1, 256))
    }
}

fn pop_task(id: usize, exec: &Executor) -> Option<Task> {
    gc::runtime_safepoint();
    if let Some(mut q) = runtime_try_lock(&exec.workers[id].queue) {
        if let Some(task) = q.pop_front() {
            return Some(task);
        }
    }
    gc::runtime_safepoint();
    if let Some(mut q) = runtime_try_lock(&exec.injector.queue) {
        if let Some(task) = q.pop_front() {
            return Some(task);
        }
    }
    for offset in 1..exec.workers.len() {
        gc::runtime_safepoint();
        let victim = (id + offset) % exec.workers.len();
        if let Some(mut q) = runtime_try_lock(&exec.workers[victim].queue) {
            if let Some(task) = q.pop_back() {
                return Some(task);
            }
        }
    }
    None
}

fn pop_task_definitive(id: usize, exec: &Executor) -> Option<Task> {
    gc::runtime_safepoint();
    {
        let mut q = runtime_lock(&exec.workers[id].queue);
        if let Some(task) = q.pop_front() {
            return Some(task);
        }
    }
    gc::runtime_safepoint();
    {
        let mut q = runtime_lock(&exec.injector.queue);
        if let Some(task) = q.pop_front() {
            return Some(task);
        }
    }
    for offset in 1..exec.workers.len() {
        gc::runtime_safepoint();
        let victim = (id + offset) % exec.workers.len();
        let mut q = runtime_lock(&exec.workers[victim].queue);
        if let Some(task) = q.pop_back() {
            return Some(task);
        }
    }
    None
}

fn wait_for_task(id: usize, exec: &Executor) -> Task {
    loop {
        if let Some(task) = pop_task(id, exec) {
            return task;
        }
        let mut epoch = runtime_lock(&exec.injector.sleep_epoch);
        exec.injector.sleepers.fetch_add(1, Ordering::AcqRel);
        if let Some(task) = pop_task_definitive(id, exec) {
            exec.injector.sleepers.fetch_sub(1, Ordering::AcqRel);
            return task;
        }
        let observed = *epoch;
        while *epoch == observed {
            gc::enter_runtime_native_no_roots();
            epoch = exec
                .injector
                .cv
                .wait(epoch)
                .unwrap_or_else(|err| err.into_inner());
            gc::leave_native();
        }
        exec.injector.sleepers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker_loop(id: usize) {
    gc::thread_start();
    while !EXEC_READY.load(Ordering::Acquire) {
        gc::runtime_safepoint();
        std::thread::yield_now();
    }
    loop {
        let task = wait_for_task(id, executor());
        gc::runtime_safepoint();
        poll_task(task);
    }
}

fn register_task_waker(task: Task) {
    runtime_write_lock(task_waker_shard(task.id)).insert(task.id, task);
}

fn unregister_task_waker(task_id: u64) {
    runtime_write_lock(task_waker_shard(task_id)).remove(&task_id);
}

#[cfg(test)]
fn registered_task_waker_count() -> usize {
    task_waker_shards()
        .iter()
        .map(|shard| runtime_read_lock(shard).len())
        .sum()
}

pub(crate) extern "C" fn task_wake(data: *mut u8) {
    // `data` is the executor task id, not an owned pointer. Stale wakeups are
    // safe: terminal tasks are removed from the registry, so late callbacks find
    // no runnable task instead of touching freed memory or leaking per-poll
    // waker allocations.
    let task_id = data as usize as u64;
    let mut inject = None;
    let mut remove_done = false;
    {
        let tasks = runtime_read_lock(task_waker_shard(task_id));
        if let Some(task) = tasks.get(&task_id) {
            if task.done.load(Ordering::Acquire) {
                remove_done = true;
            } else if task.polling.load(Ordering::Acquire) {
                task.reschedule.store(true, Ordering::Release);
            } else if !task.queued.swap(true, Ordering::AcqRel) {
                inject = Some(task.clone());
            } else {
                task.reschedule.store(true, Ordering::Release);
            }
        }
    }
    if let Some(task) = inject {
        executor().inject_marked(task);
    } else if remove_done {
        unregister_task_waker(task_id);
    }
}

/// Return whether `data` is the leaked `Arc<Task>` payload for the executor
/// waker and the task is still able to receive wakeups. Runtime wait queues use
/// this to discard stale waiters left behind by a task that completed or was
/// cancelled before it was woken.
pub(crate) fn executor_waker_is_live(data: *mut u8, wake: extern "C" fn(*mut u8)) -> Option<bool> {
    executor_waker_task(data, wake).map(|(live, _)| live)
}

/// Return `(live, task_id)` for executor task wakers.
pub(crate) fn executor_waker_task(
    data: *mut u8,
    wake: extern "C" fn(*mut u8),
) -> Option<(bool, u64)> {
    let task_id = executor_waker_task_id(data, wake)?;
    let tasks = runtime_read_lock(task_waker_shard(task_id));
    Some((
        tasks
            .get(&task_id)
            .is_some_and(|task| !task.done.load(Ordering::Acquire)),
        task_id,
    ))
}

pub(crate) fn executor_waker_task_id(data: *mut u8, wake: extern "C" fn(*mut u8)) -> Option<u64> {
    if wake as usize != task_wake as *const () as usize {
        return None;
    }
    Some(data as usize as u64)
}

#[cfg(test)]
pub(crate) fn test_executor_waker() -> (*mut u8, extern "C" fn(*mut u8)) {
    let task = Task {
        id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
        work: TaskWork::Future {
            fut: 0,
            pending_tid: 0,
        },
        ctl: new_ctl(),
        held_locks: Arc::new(Mutex::new(Vec::new())),
        poll_lock: Arc::new(Mutex::new(())),
        polling: Arc::new(AtomicBool::new(false)),
        queued: Arc::new(AtomicBool::new(false)),
        reschedule: Arc::new(AtomicBool::new(false)),
        done: Arc::new(AtomicBool::new(false)),
        cancel_requested: Arc::new(AtomicBool::new(false)),
    };
    register_task_waker(task.clone());
    (task.id as usize as *mut u8, task_wake)
}

fn poll_task(task: Task) {
    const TASK_PENDING: i64 = i64::MIN;
    if task.done.load(Ordering::Acquire) {
        return;
    }
    let Some(_poll_guard) = runtime_try_lock(&task.poll_lock) else {
        // A wake can fire while this task is still inside its current poll
        // (notably `yield_now`, which schedules the next poll before returning
        // `Pending`). Do not block an executor worker here: a blocked worker is a
        // GC mutator that cannot reach a safepoint. Mark that the in-flight
        // poll needs a follow-up if it suspends, then let that poll decide
        // whether to requeue.
        task.reschedule.store(true, Ordering::Release);
        return;
    };
    if task.done.load(Ordering::Acquire) {
        return;
    }
    task.queued.store(false, Ordering::Release);
    task.polling.store(true, Ordering::Release);
    if task.cancel_requested.load(Ordering::Acquire) && matches!(task.work, TaskWork::Future { .. })
    {
        let input = task_input(&task);
        task.polling.store(false, Ordering::Release);
        task.done.store(true, Ordering::Release);
        unregister_task_waker(task.id);
        crate::shared::release_task_held(&task.held_locks);
        unsafe { drop_generated_future_state(input) };
        gc::remove_extra_root(input);
        publish_cancelled(&task.ctl);
        return;
    }
    let mut ctx = Context {
        waker_data: task.id as usize as *mut u8,
        wake_fn: task_wake,
    };
    crate::shared::enter_task_held(task.held_locks.clone());
    let outcome = match task.work {
        TaskWork::Future { fut, pending_tid } => crate::panic_boundary::run_under_boundary(|| {
            let fut_ptr = fut as *mut u8;
            let vtable = unsafe { (fut_ptr as *const usize).read() } as *const usize;
            let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
                unsafe { std::mem::transmute(vtable.read()) };
            let data = unsafe { ((fut_ptr as usize + 8) as *const usize).read() } as *mut u8;
            let result = poll(data, &mut ctx);
            let tag = unsafe { (result as *const i64).read() };
            if tag == pending_tid {
                return TASK_PENDING;
            }
            let ready = unsafe { ((result as usize + 8) as *const usize).read() };
            unsafe { (ready as *const i64).read() }
        }),
        TaskWork::OrdinaryClosure { env, float_kind } => {
            crate::panic_boundary::run_under_boundary(|| {
                let fn_ptr = unsafe { (env as *const usize).read() };
                match float_kind {
                    8 => {
                        let f: extern "C" fn(*mut u8) -> f64 =
                            unsafe { std::mem::transmute(fn_ptr) };
                        f(env as *mut u8).to_bits() as i64
                    }
                    4 => {
                        let f: extern "C" fn(*mut u8) -> f32 =
                            unsafe { std::mem::transmute(fn_ptr) };
                        f(env as *mut u8).to_bits() as i64
                    }
                    _ => {
                        let f: extern "C" fn(*mut u8) -> i64 =
                            unsafe { std::mem::transmute(fn_ptr) };
                        f(env as *mut u8)
                    }
                }
            })
        }
    };
    crate::shared::exit_task_held();
    task.polling.store(false, Ordering::Release);

    let mut requeue = false;
    match outcome {
        Ok(TASK_PENDING) => {
            if task.cancel_requested.load(Ordering::Acquire) {
                let input = task_input(&task);
                task.done.store(true, Ordering::Release);
                unregister_task_waker(task.id);
                crate::shared::release_task_held(&task.held_locks);
                unsafe { drop_generated_future_state(input) };
                gc::remove_extra_root(input);
                publish_cancelled(&task.ctl);
            } else {
                requeue = task.reschedule.swap(false, Ordering::AcqRel);
            }
        }
        Ok(value) => {
            task.done.store(true, Ordering::Release);
            unregister_task_waker(task.id);
            finish_task(&task, Ok(value));
        }
        Err(message) => {
            task.done.store(true, Ordering::Release);
            unregister_task_waker(task.id);
            finish_task(&task, Err(message));
        }
    }
    run_pre_poll_unlock_hook(&task);
    drop(_poll_guard);
    if !task.done.load(Ordering::Acquire) && task.reschedule.swap(false, Ordering::AcqRel) {
        requeue = true;
    }
    if requeue {
        executor().inject(task);
    }
}

fn finish_task(task: &Task, outcome: Result<i64, usize>) {
    finish_worker(&task.ctl, task_input(task), outcome);
}

fn task_input(task: &Task) -> usize {
    match task.work {
        TaskWork::Future { fut, .. } => fut,
        TaskWork::OrdinaryClosure { env, .. } => env,
    }
}

/// `Task.spawn(() => Future<R>)` runtime entry. The compiler passes the already
/// evaluated async closure environment; the task pool calls it once to build the
/// future and then schedules that future on the shared executor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_task_spawn_async(
    env: *mut u8,
    pending_tid: i64,
    value_is_ptr: i64,
) -> u64 {
    // The closure environment and the freshly-created future cross Rust runtime
    // frames before the executor owns them. Pin both sides of that handoff so a
    // concurrent stress collection cannot miss them.
    gc::add_extra_root(env as usize);
    let fn_ptr = unsafe { (env as *const usize).read() };
    let f: extern "C" fn(*mut u8) -> *mut u8 = unsafe { std::mem::transmute(fn_ptr) };
    let fut = f(env);
    gc::add_extra_root(fut as usize);
    gc::remove_extra_root(env as usize);
    let id = unsafe { lang_async_spawn(fut, pending_tid, value_is_ptr) };
    gc::remove_extra_root(fut as usize);
    id
}

/// `Task.spawn(() => R)` runtime entry. The closure runs as a single executor
/// task poll and publishes its value to the shared `JoinHandle` registry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_task_spawn(env: *mut u8, float_kind: i64, value_is_ptr: i64) -> u64 {
    // Same handoff rule as `Thread.spawn`: generated code no longer owns this
    // env once we enter a Rust runtime frame, but the executor has not yet taken
    // ownership either.
    gc::add_extra_root(env as usize);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = new_ctl_with_result_is_ptr(value_is_ptr != 0);
    let held_locks = Arc::new(Mutex::new(Vec::new()));
    let poll_lock = Arc::new(Mutex::new(()));
    let polling = Arc::new(AtomicBool::new(false));
    let queued = Arc::new(AtomicBool::new(false));
    let reschedule = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let cancel_requested = Arc::new(AtomicBool::new(false));
    ctl_lock(&ctl).task_cancel = Some(TaskCancelCtl {
        task_id,
        cancel_requested: cancel_requested.clone(),
        done: done.clone(),
        reschedule: reschedule.clone(),
        held_locks: held_locks.clone(),
        poll_lock: poll_lock.clone(),
        input: env as usize,
        input_kind: TaskCancelInput::OrdinaryClosure,
    });
    runtime_write_lock(registry()).insert(id, ctl.clone());

    let task = Task {
        id: task_id,
        work: TaskWork::OrdinaryClosure {
            env: env as usize,
            float_kind,
        },
        ctl,
        held_locks,
        poll_lock,
        polling,
        queued,
        reschedule,
        done,
        cancel_requested,
    };
    register_task_waker(task.clone());
    executor().spawn(task);
    id
}

/// Spawn an **async** `() => Future<R>` closure on a new OS worker: call the
/// lifted closure to construct its `Future<R>` box, then poll that future until
/// it resolves on the worker via the private root driver (`docs/20` §1). The published
/// result is the *awaited* `R` (widened to a machine word), so `join()` /
/// `JoinHandle` machinery is identical to an ordinary non-async worker's. This
/// fuses the closure-call of [`lang_thread_spawn`] with the private root-driver path of
/// [`lang_async_spawn`].
///
/// No `float_kind` is needed: the private root driver carries the awaited value
/// as its raw 8-byte representation (a float is already its bit pattern),
/// exactly the form the joiner reads back.
///
/// # Safety
/// `env` must be a valid closure environment whose lifted function has signature
/// `extern "C" fn(*mut u8) -> *mut u8` returning a `Future<R>` box (vtable slot 0
/// = `poll`). `pending_tid` is the worker's `Pending` type id for the private
/// root driver.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_spawn_async(env: *mut u8, pending_tid: i64) -> u64 {
    // Pin before any runtime-side setup for the same reason as
    // `lang_thread_spawn`: another mutator may request a collection while this
    // handoff is still only visible through Rust frames.
    gc::add_extra_root(env as usize);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctl = new_ctl();
    runtime_write_lock(registry()).insert(id, ctl.clone());

    let env_addr = env as usize;

    let worker = ctl.clone();
    let os = spawn_os_thread_native_wait(move || {
        gc::thread_start();
        let outcome = crate::panic_boundary::run_under_boundary(|| {
            // Call the lifted async closure to obtain its `Future<R>` box.
            let fn_ptr = unsafe { (env_addr as *const usize).read() };
            let f: extern "C" fn(*mut u8) -> *mut u8 = unsafe { std::mem::transmute(fn_ptr) };
            let fut = f(env_addr as *mut u8);
            // Pin the future across the call→root-driver window (unwind-scoped,
            // so a panic in the body releases it). `env` stays pinned by the
            // spawner until `finish_worker`.
            gc::pin_for_unwind(fut as usize);
            // Poll the future until it resolves on this worker (the
            // `spawn`-keyword path). The awaited `R` comes back widened to a
            // machine word.
            let result = unsafe { crate::async_rt::lang_drive_root_future(fut, pending_tid) };
            gc::unpin_for_unwind(fut as usize);
            result
        });
        finish_worker(&worker, env_addr, outcome);
    });
    ctl_lock(&ctl).os = Some(os);
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
/// `[size][kind=plain][type_id=0][n_ptrs][offsets…][n_rc=0]` (`docs/16`). The
/// mandatory trailing `n_rc` word (here `0`) is read by the collector for every
/// object it reclaims.
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

fn future_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn join_data_desc() -> *const u8 {
    // State: [id][ready_tid][pending_tid][joined_tid][panicked_tid]
    // [cancelled_tid][value_is_ptr] — no managed pointers.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(56, &[]) as usize) as *const u8
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
    cancelled_tid: i64,
    result: i64,
    panicked: bool,
    cancelled: bool,
    message: usize,
    value_is_ptr: bool,
) -> *mut u8 {
    // Inner `Joined<R> | Panicked` union box: [type_id @0][payload @8].
    let (variant_tid, payload_struct) = if cancelled {
        (cancelled_tid, std::ptr::null_mut())
    } else if panicked {
        let payload = unsafe { gc::alloc(panicked_struct_desc()) };
        unsafe { (payload as *mut usize).write(message) };
        (panicked_tid, payload)
    } else {
        let desc = if value_is_ptr {
            joined_value_managed_desc()
        } else {
            joined_value_plain_desc()
        };
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
    //       [panicked_tid @32][cancelled_tid @40][value_is_ptr @48].
    let id = unsafe { (data as *const u64).read() };
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let joined_tid = unsafe { ((data as usize + 24) as *const i64).read() };
    let panicked_tid = unsafe { ((data as usize + 32) as *const i64).read() };
    let cancelled_tid = unsafe { ((data as usize + 40) as *const i64).read() };
    let value_is_ptr = unsafe { ((data as usize + 48) as *const i64).read() } != 0;
    let ctl = runtime_read_lock(registry())
        .get(&id)
        .cloned()
        .expect("invalid JoinHandle");

    let (result, result_pinned, panicked, cancelled, message, os_handle, was_taken) = {
        let mut g = ctl_lock(&ctl);
        if !g.done {
            // Register the executor's waker (under the same lock the worker
            // takes), then report Pending so the task suspends.
            let c = unsafe { &*ctx };
            register_waiter(&mut g.waiters, (c.waker_data as usize, c.wake_fn));
            drop(g);
            gc::pause();
            let r = unsafe { pending_box(pending_tid) };
            gc::resume_with_return_root(r as usize);
            return r;
        }
        let was_taken = g.taken;
        g.taken = true;
        let os = if was_taken { None } else { g.os.take() };
        (
            g.result,
            g.result_pinned,
            g.panicked,
            g.cancelled,
            g.message,
            os,
            was_taken,
        )
    };

    // Worker already returned; this `join()` won't block.
    if let Some(os) = os_handle {
        let _ = os.join();
    }

    gc::pause();
    let r = unsafe {
        ready_join_box(
            ready_tid,
            joined_tid,
            panicked_tid,
            cancelled_tid,
            result,
            panicked,
            cancelled,
            message,
            value_is_ptr,
        )
    };
    gc::resume_with_return_root(r as usize);

    // The handed-off value now lives in the (traced) Ready graph — the worker's
    // `R` for a normal completion, or the panic `message` (inside the `Panicked`
    // box) for a panic. Unpin the corresponding cross-thread root on the first
    // Ready poll.
    if !was_taken && !cancelled && (panicked || result_pinned) {
        gc::remove_extra_root(if panicked { message } else { result as usize });
    }

    r
}

fn cancel_task_by_id(id: u64) -> bool {
    let ctl = runtime_read_lock(registry()).get(&id).cloned();
    let Some(ctl) = ctl else {
        return false;
    };
    let cancel = {
        let g = ctl_lock(&ctl);
        if g.done {
            return g.cancelled;
        }
        g.task_cancel.clone()
    };
    let Some(cancel) = cancel else {
        return false;
    };
    cancel.cancel_requested.store(true, Ordering::Release);
    if let Some(_guard) = runtime_try_lock(&cancel.poll_lock) {
        if cancel.input_kind == TaskCancelInput::Future {
            if !cancel.done.swap(true, Ordering::AcqRel) {
                unregister_task_waker(cancel.task_id);
                crate::shared::release_task_held(&cancel.held_locks);
                unsafe { drop_generated_future_state(cancel.input) };
                gc::remove_extra_root(cancel.input);
                publish_cancelled(&ctl);
            }
        }
    } else if cancel.input_kind == TaskCancelInput::Future {
        cancel.reschedule.store(true, Ordering::Release);
    }
    true
}

/// Run a generated future's cancellation cleanup hook, when it has one.
///
/// Generated `Future` boxes store a cleanup function pointer in their metadata
/// word at offset 16. Runtime-built futures use `0` or their own marker values,
/// so this remains a no-op for shapes that do not support prompt state cleanup.
unsafe fn drop_generated_future_state(fut: usize) {
    if fut == 0 {
        return;
    }
    if unsafe { crate::async_rt::cancel_runtime_future(fut as *mut u8) } {
        return;
    }
    let hook = unsafe { ((fut + 16) as *const usize).read() };
    if hook == 0 || hook as i64 == SPAWN_FUTURE_KIND {
        return;
    }
    let data = unsafe { ((fut + 8) as *const usize).read() } as *mut u8;
    let drop_fn: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(hook) };
    drop_fn(data);
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

/// Internal future-box marker for `spawn EXPR` futures. The public runtime
/// layout leaves a type-id word at offset 16; most hand-built futures use `0`.
/// Tagging spawned futures makes cancellation robust even in native builds where
/// private vtable statics can be duplicated by object/linker boundaries.
const SPAWN_FUTURE_KIND: i64 = -0x5150_4157_4e46_5554i64;

/// Build a `Ready<T> { value: result }` boxed in a `Ready<T> | Pending` union.
unsafe fn ready_value_box(ready_tid: i64, result: i64, value_is_ptr: bool) -> *mut u8 {
    let desc = if value_is_ptr {
        ready_t_managed_desc()
    } else {
        ready_t_plain_desc()
    };
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
    let ctl = runtime_read_lock(registry())
        .get(&id)
        .cloned()
        .expect("invalid spawn id");

    let (result, result_pinned, panicked, cancelled, message, os_handle, was_taken) = {
        let mut g = ctl_lock(&ctl);
        if !g.done {
            let c = unsafe { &*ctx };
            register_waiter(&mut g.waiters, (c.waker_data as usize, c.wake_fn));
            drop(g);
            gc::pause();
            let r = unsafe { pending_box(pending_tid) };
            gc::resume_with_return_root(r as usize);
            return r;
        }
        let was_taken = g.taken;
        g.taken = true;
        let os = if was_taken { None } else { g.os.take() };
        (
            g.result,
            g.result_pinned,
            g.panicked,
            g.cancelled,
            g.message,
            os,
            was_taken,
        )
    };

    if let Some(os) = os_handle {
        let _ = os.join();
    }

    if panicked {
        // Propagate the spawned task's panic at the awaiter (`docs/21` §11).
        unsafe { crate::lang_panic(message as *const crate::strings::LangStr) };
    }
    if cancelled {
        let msg = unsafe {
            crate::strings::lang_str_from_utf8(
                b"future cancelled".as_ptr(),
                "future cancelled".len(),
            )
        };
        unsafe { crate::lang_panic(msg) };
    }

    gc::pause();
    let r = unsafe { ready_value_box(ready_tid, result, value_is_ptr) };
    gc::resume_with_return_root(r as usize);

    if !was_taken && result_pinned {
        gc::remove_extra_root(result as usize);
    }
    r
}

/// Cancel a runtime-built future when possible. This has real teeth for
/// `spawn EXPR` futures: the underlying executor task is marked cancelled, any
/// locks it held while suspended are released, and stale wakes will not poll it
/// again. Other future shapes remain safe repeatable no-ops until they grow
/// specific cancellation hooks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_future_cancel(fut: *mut u8) {
    if fut.is_null() {
        return;
    }
    if unsafe { crate::async_rt::cancel_runtime_future(fut) } {
        return;
    }
    let vtable = unsafe { (fut as *const usize).read() };
    let kind = unsafe { ((fut as usize + 16) as *const i64).read() };
    if kind != SPAWN_FUTURE_KIND && vtable != spawn_vtable() as usize {
        return;
    }
    let data = unsafe { ((fut as usize + 8) as *const usize).read() };
    let id = unsafe { (data as *const u64).read() };
    cancel_task_by_id(id);
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
    let id = unsafe { lang_async_spawn(fut, pending_tid, value_is_ptr) };
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
        ((bx as usize + 16) as *mut i64).write(SPAWN_FUTURE_KIND);
    }
    gc::resume_with_return_root(bx as usize);
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
        ((data as usize + 40) as *mut i64).write(0);
        ((data as usize + 48) as *mut i64).write(value_is_ptr);
    }
    let bx = unsafe { gc::alloc(future_box_desc()) };
    unsafe {
        (bx as *mut usize).write(join_vtable() as usize); // vtable @0
        ((bx as usize + 8) as *mut usize).write(data as usize); // data @8
        ((bx as usize + 16) as *mut i64).write(0); // type_id @16
    }
    gc::resume_with_return_root(bx as usize);
    bx
}

/// Construct a `std:task` `JoinHandle<R>.join()` future. It shares the same
/// polling machinery as OS-thread joins but passes a `Cancelled` variant tag so
/// executor-task cancellation is surfaced instead of masquerading as a value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_task_join_future(
    id: u64,
    ready_tid: i64,
    pending_tid: i64,
    joined_tid: i64,
    panicked_tid: i64,
    cancelled_tid: i64,
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
        ((data as usize + 40) as *mut i64).write(cancelled_tid);
        ((data as usize + 48) as *mut i64).write(value_is_ptr);
    }
    let bx = unsafe { gc::alloc(future_box_desc()) };
    unsafe {
        (bx as *mut usize).write(join_vtable() as usize);
        ((bx as usize + 8) as *mut usize).write(data as usize);
        ((bx as usize + 16) as *mut i64).write(0);
    }
    gc::resume_with_return_root(bx as usize);
    bx
}

/// Request cooperative cancellation for a task handle. This only has teeth for
/// executor-scheduled tasks (`Task.spawn` / `spawn EXPR`); OS-thread handles do
/// not call this entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_task_cancel(id: u64) {
    cancel_task_by_id(id);
}

/// `JoinHandle<R>.detach()` (`docs/20` §1): relinquish the claim on a worker so
/// it continues independently in the background, fire-and-forget, with its
/// result discarded. The worker thread holds its own `Arc<ThreadCtl>` clone, so
/// it keeps running regardless; we drop the registry's claim and detach the OS
/// thread (drop its join handle without joining) so it is reclaimed on its own
/// when it finishes. Works identically for ordinary non-async and async workers.
///
/// # Safety
/// `id` must be a live `JoinHandle` id produced by [`lang_thread_spawn`],
/// [`lang_thread_spawn_async`], or [`lang_async_spawn`], not yet joined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_thread_detach(id: u64) {
    let ctl = runtime_write_lock(registry()).remove(&id);
    if let Some(ctl) = ctl {
        let os = {
            let mut g = ctl_lock(&ctl);
            g.detached = true;
            let os = g.os.take();
            if g.done && !g.taken {
                g.taken = true;
                if g.panicked {
                    gc::remove_extra_root(g.message);
                } else if !g.cancelled && g.result_pinned {
                    gc::remove_extra_root(g.result as usize);
                }
            }
            os
        };
        drop(os); // detach: never joined, reclaimed when the worker finishes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as O};

    fn marker_task(marker: usize) -> Task {
        Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            work: TaskWork::Future {
                fut: marker,
                pending_tid: marker as i64,
            },
            ctl: new_ctl(),
            held_locks: Arc::new(Mutex::new(Vec::new())),
            poll_lock: Arc::new(Mutex::new(())),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(false)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    fn task_marker(task: Task) -> usize {
        match task.work {
            TaskWork::Future { fut, .. } => fut,
            TaskWork::OrdinaryClosure { env, .. } => env,
        }
    }

    #[test]
    fn spawn_os_thread_helper_uses_native_state_marker() {
        let joined = spawn_os_thread_native_wait(|| {}).join();
        assert!(
            joined.is_ok(),
            "helper-spawned OS thread should join cleanly"
        );
    }

    #[test]
    fn executor_start_uses_native_wait_thread_creation_helper() {
        let source = include_str!("threads.rs");
        let start_body = source
            .split("fn start() -> Self")
            .nth(1)
            .and_then(|rest| rest.split("fn spawn(&self").next())
            .expect("Executor::start should remain in threads.rs");
        assert!(
            start_body.contains("spawn_os_thread_native_wait(move || worker_loop(id))"),
            "executor worker startup must use the GC native-state thread creation helper"
        );
        assert!(
            !start_body.contains("std::thread::spawn(move || worker_loop(id))"),
            "executor worker startup must not bypass the GC native-state helper"
        );
    }

    #[test]
    fn executor_idle_wait_uses_runtime_native_no_roots_boundary() {
        let source = include_str!("threads.rs");
        let wait_body = source
            .split("fn wait_for_task(id: usize, exec: &Executor) -> Task")
            .nth(1)
            .and_then(|rest| rest.split("fn worker_loop(id: usize)").next())
            .expect("wait_for_task should remain in threads.rs");
        assert!(
            wait_body.contains("gc::enter_runtime_native_no_roots();"),
            "executor worker idle wait must publish runtime-native/no-root state before parking"
        );
        assert!(
            wait_body.contains(".cv")
                && wait_body.contains(".wait(epoch)")
                && wait_body.contains("gc::leave_native();"),
            "executor worker idle wait must leave native state after the condvar wait resumes"
        );
    }

    #[test]
    fn cancelling_ordinary_closure_does_not_treat_env_as_future_state() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let reschedule = Arc::new(AtomicBool::new(false));
        let held_locks = Arc::new(Mutex::new(Vec::new()));
        let poll_lock = Arc::new(Mutex::new(()));
        let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        // Deliberately put a non-null, invalid-looking word at offset 16. A
        // regression in the cancellation path interpreted ordinary non-async
        // closure envs as generated future boxes and tried to call this word as
        // a drop hook.
        let env = Box::into_raw(Box::new([0usize, 0, usize::MAX])) as usize;
        ctl.inner.lock().unwrap().task_cancel = Some(TaskCancelCtl {
            task_id,
            cancel_requested: cancel_requested.clone(),
            done: done.clone(),
            reschedule,
            held_locks,
            poll_lock,
            input: env,
            input_kind: TaskCancelInput::OrdinaryClosure,
        });
        runtime_write_lock(registry()).insert(id, ctl.clone());

        assert!(cancel_task_by_id(id));
        assert!(cancel_requested.load(Ordering::Acquire));
        assert!(!done.load(Ordering::Acquire));
        {
            let g = ctl.inner.lock().unwrap();
            assert!(!g.done);
            assert!(!g.cancelled);
        }

        runtime_write_lock(registry()).remove(&id);
        unsafe {
            drop(Box::from_raw(env as *mut [usize; 3]));
        }
    }

    #[test]
    fn publish_cancelled_wakes_all_registered_waiters_once() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        static WAKES: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(data: *mut u8) {
            let counter = unsafe { &*(data as *const AtomicU32) };
            counter.fetch_add(1, O::SeqCst);
        }

        WAKES.store(0, O::SeqCst);
        let ctl = new_ctl();
        {
            let mut g = ctl.inner.lock().unwrap();
            g.waiters
                .push((&WAKES as *const AtomicU32 as usize, count_wake));
            g.waiters
                .push((&WAKES as *const AtomicU32 as usize, count_wake));
        }

        publish_cancelled(&ctl);

        {
            let g = ctl.inner.lock().unwrap();
            assert!(g.done);
            assert!(g.cancelled);
            assert!(
                g.waiters.is_empty(),
                "waiters must be drained when cancellation is published"
            );
        }
        assert_eq!(
            WAKES.load(O::SeqCst),
            2,
            "every suspended join/spawn waiter must be woken by cancellation"
        );

        publish_cancelled(&ctl);
        assert_eq!(
            WAKES.load(O::SeqCst),
            2,
            "publishing cancellation twice must not re-wake stale waiters"
        );
    }

    #[test]
    fn duplicate_waiter_registration_is_coalesced() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        static A: AtomicU32 = AtomicU32::new(0);
        static B: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(data: *mut u8) {
            let counter = unsafe { &*(data as *const AtomicU32) };
            counter.fetch_add(1, O::SeqCst);
        }

        A.store(0, O::SeqCst);
        B.store(0, O::SeqCst);
        let mut waiters = Vec::new();
        let a = &A as *const AtomicU32 as usize;
        let b = &B as *const AtomicU32 as usize;
        let wake = count_wake as extern "C" fn(*mut u8);
        register_waiter(&mut waiters, (a, wake));
        register_waiter(&mut waiters, (a, wake));
        register_waiter(&mut waiters, (b, wake));

        assert_eq!(
            waiters.len(),
            2,
            "re-polling the same pending join/spawn future with the same executor waker must not grow duplicate waiter entries"
        );
        for (data, wake) in waiters {
            wake(data as *mut u8);
        }
        assert_eq!(A.load(O::SeqCst), 1);
        assert_eq!(B.load(O::SeqCst), 1);
    }

    #[test]
    fn duplicate_pending_waiters_wake_once_on_cancellation() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        static WAKES: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(data: *mut u8) {
            let counter = unsafe { &*(data as *const AtomicU32) };
            counter.fetch_add(1, O::SeqCst);
        }

        WAKES.store(0, O::SeqCst);
        let ctl = new_ctl();
        {
            let mut g = ctl.inner.lock().unwrap();
            let waiter = (
                &WAKES as *const AtomicU32 as usize,
                count_wake as extern "C" fn(*mut u8),
            );
            register_waiter(&mut g.waiters, waiter);
            register_waiter(&mut g.waiters, waiter);
            assert_eq!(
                g.waiters.len(),
                1,
                "duplicate pending polls should share one waiter slot"
            );
        }

        publish_cancelled(&ctl);

        assert_eq!(
            WAKES.load(O::SeqCst),
            1,
            "one executor task waker only needs one wake after cancellation"
        );
        assert!(ctl.inner.lock().unwrap().waiters.is_empty());
    }

    #[test]
    fn detach_after_completion_releases_pinned_result_handoff() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let input = 0xD_E7AC_100usize;
        let result = 0xD_E7AC_200usize;
        gc::add_extra_root(input);
        finish_worker(&ctl, input, Ok(result as i64));
        assert_eq!(gc::extra_root_count_for(input), 0);
        assert_eq!(
            gc::extra_root_count_for(result),
            1,
            "completed worker result is pinned for a possible join"
        );

        unsafe { lang_thread_detach(id) };

        assert_eq!(
            gc::extra_root_count_for(result),
            0,
            "detaching an already-complete handle discards the handoff root"
        );
    }

    #[test]
    fn scalar_task_result_is_not_pinned_for_join_handoff() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let ctl = new_ctl_with_result_is_ptr(false);

        let result = 0x5CA1_A123usize;
        publish_done(&ctl, result as i64);

        assert_eq!(
            gc::extra_root_count_for(result),
            0,
            "scalar Task.spawn results must not hit the global extra-root list"
        );
        assert!(!ctl.inner.lock().unwrap().result_pinned);
    }

    #[test]
    fn managed_task_result_is_pinned_for_join_handoff() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let ctl = new_ctl_with_result_is_ptr(true);

        let result = 0x5CA1_B456usize;
        publish_done(&ctl, result as i64);

        assert_eq!(
            gc::extra_root_count_for(result),
            1,
            "managed Task.spawn results stay rooted until the joiner boxes them"
        );
        assert!(ctl.inner.lock().unwrap().result_pinned);
        gc::remove_extra_root(result);
    }

    #[test]
    fn managed_null_task_result_is_not_pinned_for_join_handoff() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let ctl = new_ctl_with_result_is_ptr(true);

        publish_done(&ctl, 0);

        assert_eq!(
            gc::extra_root_count_for(0),
            0,
            "null Task.spawn results must not hit the global extra-root list"
        );
        assert!(!ctl.inner.lock().unwrap().result_pinned);
    }

    #[test]
    fn detach_before_completion_discards_result_without_pinning() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let input = 0xD_E7AC_300usize;
        let result = 0xD_E7AC_400usize;
        gc::add_extra_root(input);
        unsafe { lang_thread_detach(id) };
        finish_worker(&ctl, input, Ok(result as i64));

        assert_eq!(gc::extra_root_count_for(input), 0);
        assert_eq!(
            gc::extra_root_count_for(result),
            0,
            "detached workers have no joiner, so their results are not pinned"
        );
    }

    #[test]
    fn detached_panic_releases_pinned_message_handoff() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let input = 0xD_E7AC_500usize;
        let message = 0xD_E7AC_600usize;
        gc::add_extra_root(input);
        gc::add_extra_root(message);
        unsafe { lang_thread_detach(id) };
        finish_worker(&ctl, input, Err(message));

        assert_eq!(gc::extra_root_count_for(input), 0);
        assert_eq!(
            gc::extra_root_count_for(message),
            0,
            "detached panics discard the message because no joiner can observe it"
        );
    }

    #[test]
    fn wake_during_poll_is_coalesced_into_reschedule_bit() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let task = marker_task(77);
        register_task_waker(task.clone());
        task.polling.store(true, Ordering::Release);
        assert!(!task.reschedule.load(Ordering::Acquire));

        task_wake(task.id as usize as *mut u8);

        assert!(
            task.reschedule.load(Ordering::Acquire),
            "wake-during-poll must request one follow-up poll instead of injecting duplicate runnable work"
        );
        unregister_task_waker(task.id);
    }

    #[test]
    fn wake_while_already_queued_records_followup_poll() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let task = marker_task(88);
        register_task_waker(task.clone());
        task.queued.store(true, Ordering::Release);
        assert!(!task.polling.load(Ordering::Acquire));
        assert!(!task.reschedule.load(Ordering::Acquire));

        task_wake(task.id as usize as *mut u8);

        assert!(
            task.reschedule.load(Ordering::Acquire),
            "wake for an already queued task must be preserved for the queued poll"
        );
        assert!(task.queued.load(Ordering::Acquire));
        unregister_task_waker(task.id);
    }

    #[test]
    fn duplicate_poll_attempt_preserves_queued_marker() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let task = marker_task(89);
        task.queued.store(true, Ordering::Release);
        let guard = task.poll_lock.lock().unwrap();

        poll_task(task.clone());

        assert!(
            task.queued.load(Ordering::Acquire),
            "a duplicate runnable copy must not clear another queued/polling copy"
        );
        assert!(
            task.reschedule.load(Ordering::Acquire),
            "the in-flight poll must receive one follow-up request"
        );
        drop(guard);
    }

    #[test]
    fn poll_task_recovers_poisoned_poll_lock_and_completes() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let before_wakers = registered_task_waker_count();
        let task = Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            work: TaskWork::Future {
                fut: make_future_box(ready99_poll) as usize,
                pending_tid: 9,
            },
            ctl: new_ctl_with_result_is_ptr(false),
            held_locks: Arc::new(Mutex::new(Vec::new())),
            poll_lock: Arc::new(Mutex::new(())),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(true)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        let poisoned = task.poll_lock.clone();
        let poisoner = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison task poll lock before normal poll");
        });
        assert!(poisoner.join().is_err());
        assert!(
            task.poll_lock.lock().is_err(),
            "test setup must leave the poll lock poisoned"
        );

        register_task_waker(task.clone());
        assert_eq!(registered_task_waker_count(), before_wakers + 1);

        poll_task(task.clone());

        assert!(task.done.load(Ordering::Acquire));
        assert!(!task.polling.load(Ordering::Acquire));
        assert_eq!(registered_task_waker_count(), before_wakers);
        let g = ctl_lock(&task.ctl);
        assert!(g.done);
        assert!(!g.panicked);
        assert_eq!(g.result, 99);
    }

    #[test]
    fn repeated_polls_reuse_one_registered_waker_payload() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let before = registered_task_waker_count();
        let task = Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            work: TaskWork::Future {
                fut: make_future_box(post_unlock_race_pending_poll) as usize,
                pending_tid: 9,
            },
            ctl: new_ctl(),
            held_locks: Arc::new(Mutex::new(Vec::new())),
            poll_lock: Arc::new(Mutex::new(())),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(true)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        register_task_waker(task.clone());
        assert_eq!(registered_task_waker_count(), before + 1);

        for _ in 0..8 {
            task.queued.store(true, Ordering::Release);
            poll_task(task.clone());
            assert_eq!(
                registered_task_waker_count(),
                before + 1,
                "polling must not allocate/register another persistent waker payload"
            );
        }

        task.done.store(true, Ordering::Release);
        unregister_task_waker(task.id);
        assert_eq!(registered_task_waker_count(), before);
    }

    #[test]
    fn executor_spawn_round_robins_across_worker_queues() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let exec = Executor::new_unstarted(3);
        for marker in 1..=6 {
            exec.spawn(marker_task(marker));
        }

        let queues: Vec<Vec<usize>> = exec
            .workers
            .iter()
            .map(|w| {
                w.queue
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .map(task_marker)
                    .collect()
            })
            .collect();
        assert_eq!(queues, vec![vec![1, 4], vec![2, 5], vec![3, 6]]);
    }

    #[test]
    fn pop_task_takes_local_then_global_injector_then_steals() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let exec = Executor::new_unstarted(3);
        exec.workers[0]
            .queue
            .lock()
            .unwrap()
            .push_back(marker_task(10));
        exec.inject(marker_task(20));
        exec.workers[1]
            .queue
            .lock()
            .unwrap()
            .push_back(marker_task(30));
        exec.workers[1]
            .queue
            .lock()
            .unwrap()
            .push_back(marker_task(31));

        assert_eq!(task_marker(pop_task(0, &exec).expect("local")), 10);
        assert_eq!(task_marker(pop_task(0, &exec).expect("injector")), 20);
        // Stealing takes from the victim's back so the oldest local work stays
        // closest to its owning worker.
        assert_eq!(task_marker(pop_task(0, &exec).expect("stolen")), 31);
        assert_eq!(task_marker(pop_task(0, &exec).expect("stolen again")), 30);
        assert!(pop_task(0, &exec).is_none());
    }

    #[test]
    fn definitive_pop_finds_work_after_transient_queue_contention() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let exec = Executor::new_unstarted(1);
        let mut guard = exec.workers[0].queue.lock().unwrap();
        guard.push_back(marker_task(222));

        assert!(
            pop_task(0, &exec).is_none(),
            "the fast scheduler scan intentionally skips contended queues"
        );
        assert_eq!(
            guard.len(),
            1,
            "a try-lock miss must not mean the queue is empty"
        );
        drop(guard);
        assert_eq!(
            task_marker(pop_task_definitive(0, &exec).expect("definitive pop")),
            222,
            "the pre-park scan must take real work before a worker sleeps"
        );
    }

    #[test]
    fn local_queue_spawn_wakes_parked_worker_without_timeout_polling() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let exec: &'static Executor = Box::leak(Box::new(Executor::new_unstarted(1)));
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            gc::thread_start();
            let task = wait_for_task(0, exec);
            tx.send(task_marker(task)).unwrap();
        });

        let started = std::time::Instant::now();
        while exec.injector.sleepers.load(Ordering::Acquire) == 0
            && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::yield_now();
        }
        assert_eq!(
            exec.injector.sleepers.load(Ordering::Acquire),
            1,
            "worker should be parked before local work is submitted"
        );

        exec.spawn(marker_task(123));

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            123,
            "local-queue submission must wake the parked worker without relying on timeout polling"
        );
    }

    #[test]
    fn worker_count_override_parses_and_clamps() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        assert_eq!(parse_worker_count_override("1"), Some(1));
        assert_eq!(parse_worker_count_override(" 12 "), Some(12));
        assert_eq!(parse_worker_count_override("512"), Some(256));
        assert_eq!(parse_worker_count_override("0"), None);
        assert_eq!(parse_worker_count_override("nope"), None);
    }

    #[test]
    fn runtime_read_lock_recovers_poisoned_rwlock() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let lock = Arc::new(RwLock::new(41_i64));
        let poisoned = lock.clone();
        let poisoner = std::thread::spawn(move || {
            let _guard = poisoned.write().unwrap();
            panic!("poison runtime read lock test");
        });
        assert!(poisoner.join().is_err());
        assert!(
            lock.read().is_err(),
            "test setup must leave the RwLock poisoned"
        );

        assert_eq!(*runtime_read_lock(&lock), 41);
    }

    #[test]
    fn runtime_write_lock_recovers_poisoned_rwlock() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let lock = Arc::new(RwLock::new(41_i64));
        let poisoned = lock.clone();
        let poisoner = std::thread::spawn(move || {
            let mut guard = poisoned.write().unwrap();
            *guard += 1;
            panic!("poison runtime write lock test");
        });
        assert!(poisoner.join().is_err());
        assert!(
            lock.write().is_err(),
            "test setup must leave the RwLock poisoned"
        );

        *runtime_write_lock(&lock) += 1;
        assert_eq!(*runtime_read_lock(&lock), 43);
    }

    /// Build a `Future<i64>` interface-object box (vtable slot 0 = `poll`).
    /// Memory is leaked (test-only).
    fn make_future_box(poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8) -> *mut u8 {
        make_future_box_with_word(poll, 0)
    }

    fn make_future_box_with_word(
        poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8,
        word: i64,
    ) -> *mut u8 {
        let vtable: Box<[usize; 1]> = Box::new([poll as usize]);
        let vtable_ptr = Box::into_raw(vtable) as usize;
        let data_ptr = Box::into_raw(Box::new([word; 1])) as usize;
        let fut: Box<[usize; 3]> = Box::new([vtable_ptr, data_ptr, 0]);
        Box::into_raw(fut) as *mut u8
    }

    /// Wait for the worker behind `id` to publish its result, then read it.
    fn wait_result(id: u64) -> (i64, bool) {
        loop {
            let ctl = runtime_read_lock(registry())
                .get(&id)
                .cloned()
                .expect("registered");
            let g = ctl.inner.lock().unwrap();
            if g.done {
                return (g.result, g.panicked);
            }
            drop(g);
            std::thread::yield_now();
        }
    }

    // A `poll` that completes immediately as `Ready<i64>{ value: 99 }`.
    extern "C" fn ready99_poll(_d: *mut u8, _c: *mut Context) -> *mut u8 {
        let ready: Box<[i64; 1]> = Box::new([99]);
        let ready_ptr = Box::into_raw(ready) as usize;
        let union_box: Box<[usize; 2]> = Box::new([7, ready_ptr]); // tag 7 = Ready
        Box::into_raw(union_box) as *mut u8
    }
    // The lifted async closure: env word 0 is this fn; calling it builds and
    // returns the `Future<i64>` box the worker then drives.
    extern "C" fn make_ready_future(_env: *mut u8) -> *mut u8 {
        make_future_box(ready99_poll)
    }

    #[test]
    fn spawn_async_drives_immediately_ready_future() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        // env: one word = the lifted closure fn ptr (the closure ABI, `docs/09`).
        let env: Box<[usize; 1]> = Box::new([make_ready_future as *const () as usize]);
        let env_ptr = Box::into_raw(env) as *mut u8;
        // Pending tid 9 here; the future is Ready (tag 7), so the private root
        // driver returns.
        let id = unsafe { lang_thread_spawn_async(env_ptr, 9) };
        assert_eq!(wait_result(id), (99, false));
    }

    // A `poll` that is Pending once (waking itself for an immediate re-poll),
    // then `Ready<i64>{ value: 123 }` — exercises a genuine suspend/resume cycle
    // on the worker thread, the behavior `lang_thread_spawn` cannot perform.
    static DRIVE_POLLS: AtomicU32 = AtomicU32::new(0);
    extern "C" fn yield_once_poll(_d: *mut u8, ctx: *mut Context) -> *mut u8 {
        if DRIVE_POLLS.fetch_add(1, O::SeqCst) == 0 {
            let c = unsafe { &*ctx };
            (c.wake_fn)(c.waker_data); // arrange an immediate re-poll
            let pending: Box<[usize; 2]> = Box::new([9, 0]); // tag 9 = Pending
            return Box::into_raw(pending) as *mut u8;
        }
        let ready: Box<[i64; 1]> = Box::new([123]);
        let ready_ptr = Box::into_raw(ready) as usize;
        let union_box: Box<[usize; 2]> = Box::new([7, ready_ptr]);
        Box::into_raw(union_box) as *mut u8
    }
    extern "C" fn make_yield_future(_env: *mut u8) -> *mut u8 {
        make_future_box(yield_once_poll)
    }

    extern "C" fn pending_never_poll(_d: *mut u8, _ctx: *mut Context) -> *mut u8 {
        let pending: Box<[usize; 2]> = Box::new([9, 0]);
        Box::into_raw(pending) as *mut u8
    }

    static POST_UNLOCK_RACE_POLLS: AtomicU32 = AtomicU32::new(0);
    extern "C" fn post_unlock_race_pending_poll(_d: *mut u8, _ctx: *mut Context) -> *mut u8 {
        POST_UNLOCK_RACE_POLLS.fetch_add(1, O::SeqCst);
        let pending: Box<[usize; 2]> = Box::new([9, 0]);
        Box::into_raw(pending) as *mut u8
    }

    static CANCEL_DURING_POLL_POLLS: AtomicU32 = AtomicU32::new(0);
    extern "C" fn cancel_during_poll_then_pending(data: *mut u8, _ctx: *mut Context) -> *mut u8 {
        CANCEL_DURING_POLL_POLLS.fetch_add(1, O::SeqCst);
        let id = unsafe { (data as *const i64).read() } as u64;
        assert!(
            cancel_task_by_id(id),
            "cancellation request should be recorded while the task is polling"
        );
        let pending: Box<[usize; 2]> = Box::new([9, 0]);
        Box::into_raw(pending) as *mut u8
    }

    static EXEC_POLLS: AtomicU32 = AtomicU32::new(0);
    extern "C" fn executor_yield_once_poll(_d: *mut u8, ctx: *mut Context) -> *mut u8 {
        if EXEC_POLLS.fetch_add(1, O::SeqCst) == 0 {
            let c = unsafe { &*ctx };
            (c.wake_fn)(c.waker_data);
            let pending: Box<[usize; 2]> = Box::new([9, 0]);
            return Box::into_raw(pending) as *mut u8;
        }
        let ready: Box<[i64; 1]> = Box::new([123]);
        let ready_ptr = Box::into_raw(ready) as usize;
        let union_box: Box<[usize; 2]> = Box::new([7, ready_ptr]);
        Box::into_raw(union_box) as *mut u8
    }

    /// A lifted ordinary non-async worker that raises a language panic.
    /// `lang_panic` runs on the worker thread (which has a boundary installed),
    /// so it must `longjmp` back instead of terminating the process.
    extern "C" fn panicking_worker(_env: *mut u8) -> i64 {
        let msg = unsafe { crate::strings::lang_str_from_utf8(b"boom".as_ptr(), 4) };
        unsafe { crate::lang_panic(msg) };
    }
    extern "C" fn answer_worker(_env: *mut u8) -> i64 {
        42
    }

    /// Join a worker's OS thread so its mutator handle fully deregisters (and
    /// its alloc log drains) before the heap-resetting teardown.
    fn join_worker(id: u64) {
        let os = runtime_read_lock(registry())
            .get(&id)
            .cloned()
            .and_then(|c| c.inner.lock().unwrap().os.take());
        if let Some(os) = os {
            let _ = os.join();
        }
    }

    #[test]
    fn worker_panic_is_isolated_and_reports_message() {
        // Shares the process-global heap (the boundary builds a managed message);
        // serialize against the GC tests and reset the heap around it.
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        unsafe { crate::gc::free_all() };

        // A panicking worker must not abort the process: it publishes `panicked`
        // with its message, and a sibling spawned alongside completes normally.
        let penv: Box<[usize; 1]> = Box::new([panicking_worker as *const () as usize]);
        let aenv: Box<[usize; 1]> = Box::new([answer_worker as *const () as usize]);
        let pid = unsafe { lang_thread_spawn(Box::into_raw(penv) as *mut u8, 0) };
        let aid = unsafe { lang_thread_spawn(Box::into_raw(aenv) as *mut u8, 0) };

        let (_pr, panicked) = wait_result(pid);
        assert!(panicked, "the worker's panic was isolated and recorded");
        // The message is the pinned `str` the boundary built.
        let msg = runtime_read_lock(registry())
            .get(&pid)
            .cloned()
            .unwrap()
            .inner
            .lock()
            .unwrap()
            .message;
        let bytes = unsafe { crate::strings::str_bytes(msg as *const crate::strings::LangStr) };
        assert_eq!(
            bytes, b"boom",
            "panic message propagated through the boundary"
        );

        let (ar, ap) = wait_result(aid);
        assert!(!ap, "the sibling did not panic");
        assert_eq!(ar, 42, "the sibling completed normally despite the panic");

        // Teardown: join both workers, unpin the message, reset the heap.
        join_worker(pid);
        join_worker(aid);
        gc::remove_extra_root(msg);
        unsafe { crate::gc::free_all() };
    }

    #[test]
    fn spawn_async_resolves_suspending_future() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        DRIVE_POLLS.store(0, O::SeqCst);
        let env: Box<[usize; 1]> = Box::new([make_yield_future as *const () as usize]);
        let env_ptr = Box::into_raw(env) as *mut u8;
        let id = unsafe { lang_thread_spawn_async(env_ptr, 9) };
        assert_eq!(wait_result(id), (123, false));
        // The worker polled twice: Pending, then Ready.
        assert_eq!(DRIVE_POLLS.load(O::SeqCst), 2);
    }

    #[test]
    fn async_spawn_uses_executor_and_resolves_suspending_future() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        EXEC_POLLS.store(0, O::SeqCst);
        let fut = make_future_box(executor_yield_once_poll);
        let id = unsafe { lang_async_spawn(fut, 9, 0) };
        assert_eq!(wait_result(id), (123, false));
        assert_eq!(EXEC_POLLS.load(O::SeqCst), 2);
    }

    #[test]
    fn pending_without_wake_parks_instead_of_busy_requeueing() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let task = Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            work: TaskWork::Future {
                fut: make_future_box(pending_never_poll) as usize,
                pending_tid: 9,
            },
            ctl: new_ctl(),
            held_locks: Arc::new(Mutex::new(Vec::new())),
            poll_lock: Arc::new(Mutex::new(())),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(true)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        poll_task(task.clone());

        assert!(!task.done.load(Ordering::Acquire));
        assert!(!task.queued.load(Ordering::Acquire));
        assert!(!task.reschedule.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_requested_during_poll_publishes_at_pending_suspension() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        static WAKES: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(data: *mut u8) {
            let counter = unsafe { &*(data as *const AtomicU32) };
            counter.fetch_add(1, O::SeqCst);
        }

        WAKES.store(0, O::SeqCst);
        CANCEL_DURING_POLL_POLLS.store(0, O::SeqCst);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        let fut = make_future_box_with_word(cancel_during_poll_then_pending, id as i64) as usize;
        let held_locks = Arc::new(Mutex::new(Vec::new()));
        let poll_lock = Arc::new(Mutex::new(()));
        let done = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let reschedule = Arc::new(AtomicBool::new(false));
        let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        ctl.inner.lock().unwrap().task_cancel = Some(TaskCancelCtl {
            task_id,
            cancel_requested: cancel_requested.clone(),
            done: done.clone(),
            reschedule: reschedule.clone(),
            held_locks: held_locks.clone(),
            poll_lock: poll_lock.clone(),
            input: fut,
            input_kind: TaskCancelInput::Future,
        });
        ctl.inner
            .lock()
            .unwrap()
            .waiters
            .push((&WAKES as *const AtomicU32 as usize, count_wake));
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let task = Task {
            id: task_id,
            work: TaskWork::Future {
                fut,
                pending_tid: 9,
            },
            ctl: ctl.clone(),
            held_locks,
            poll_lock,
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(true)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: done.clone(),
            cancel_requested: cancel_requested.clone(),
        };
        register_task_waker(task.clone());

        poll_task(task.clone());

        assert_eq!(CANCEL_DURING_POLL_POLLS.load(O::SeqCst), 1);
        assert!(cancel_requested.load(Ordering::Acquire));
        assert!(done.load(Ordering::Acquire));
        assert!(!task.polling.load(Ordering::Acquire));
        assert!(!task.reschedule.load(Ordering::Acquire));
        {
            let g = ctl.inner.lock().unwrap();
            assert!(g.done);
            assert!(g.cancelled);
            assert!(g.waiters.is_empty());
        }
        assert_eq!(
            WAKES.load(O::SeqCst),
            1,
            "the suspended waiter must be woken when cancellation publishes"
        );

        runtime_write_lock(registry()).remove(&id);
    }

    #[test]
    fn cancellation_recovers_poisoned_poll_lock_and_cleans_suspended_task() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        static WAKES: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count_wake(data: *mut u8) {
            let counter = unsafe { &*(data as *const AtomicU32) };
            counter.fetch_add(1, O::SeqCst);
        }

        WAKES.store(0, O::SeqCst);
        let before_wakers = registered_task_waker_count();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        let held_locks = Arc::new(Mutex::new(Vec::new()));
        let poll_lock = Arc::new(Mutex::new(()));
        let done = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let reschedule = Arc::new(AtomicBool::new(false));
        let fut = make_future_box(pending_never_poll) as usize;
        gc::add_extra_root(fut);

        ctl.inner.lock().unwrap().task_cancel = Some(TaskCancelCtl {
            task_id,
            cancel_requested: cancel_requested.clone(),
            done: done.clone(),
            reschedule: reschedule.clone(),
            held_locks: held_locks.clone(),
            poll_lock: poll_lock.clone(),
            input: fut,
            input_kind: TaskCancelInput::Future,
        });
        ctl.inner.lock().unwrap().waiters.push((
            &WAKES as *const AtomicU32 as usize,
            count_wake as extern "C" fn(*mut u8),
        ));
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let task = Task {
            id: task_id,
            work: TaskWork::Future {
                fut,
                pending_tid: 9,
            },
            ctl: ctl.clone(),
            held_locks,
            poll_lock: poll_lock.clone(),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(false)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: done.clone(),
            cancel_requested: cancel_requested.clone(),
        };
        register_task_waker(task);
        assert_eq!(registered_task_waker_count(), before_wakers + 1);

        let poisoned = poll_lock.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison task poll lock");
        })
        .join();
        assert!(
            poll_lock.lock().is_err(),
            "test setup should poison the poll lock"
        );

        assert!(
            cancel_task_by_id(id),
            "cancellation should still succeed after poll-lock poison"
        );
        assert!(cancel_requested.load(Ordering::Acquire));
        assert!(done.load(Ordering::Acquire));
        assert_eq!(
            registered_task_waker_count(),
            before_wakers,
            "poison recovery must still unregister the persistent task waker"
        );
        assert_eq!(
            gc::extra_root_count_for(fut),
            0,
            "poison recovery must still release the executor handoff root"
        );
        {
            let g = ctl.inner.lock().unwrap();
            assert!(g.done);
            assert!(g.cancelled);
            assert!(g.waiters.is_empty());
        }
        assert_eq!(
            WAKES.load(O::SeqCst),
            1,
            "poison recovery must still wake suspended join/spawn waiters"
        );

        runtime_write_lock(registry()).remove(&id);
    }

    #[test]
    fn cancellation_while_poll_lock_busy_requests_reschedule() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let before_wakers = registered_task_waker_count();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        let ctl = new_ctl();
        let held_locks = Arc::new(Mutex::new(Vec::new()));
        let poll_lock = Arc::new(Mutex::new(()));
        let done = Arc::new(AtomicBool::new(false));
        let reschedule = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let fut = make_future_box(pending_never_poll) as usize;
        gc::add_extra_root(fut);

        ctl.inner.lock().unwrap().task_cancel = Some(TaskCancelCtl {
            task_id,
            cancel_requested: cancel_requested.clone(),
            done: done.clone(),
            reschedule: reschedule.clone(),
            held_locks: held_locks.clone(),
            poll_lock: poll_lock.clone(),
            input: fut,
            input_kind: TaskCancelInput::Future,
        });
        runtime_write_lock(registry()).insert(id, ctl.clone());

        let task = Task {
            id: task_id,
            work: TaskWork::Future {
                fut,
                pending_tid: 9,
            },
            ctl,
            held_locks,
            poll_lock: poll_lock.clone(),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(false)),
            reschedule: reschedule.clone(),
            done: done.clone(),
            cancel_requested: cancel_requested.clone(),
        };
        register_task_waker(task);
        let _busy = poll_lock.lock().unwrap();

        assert!(cancel_task_by_id(id));
        assert!(cancel_requested.load(Ordering::Acquire));
        assert!(
            reschedule.load(Ordering::Acquire),
            "busy-poll cancellation must force a follow-up poll to observe cancellation"
        );
        assert!(!done.load(Ordering::Acquire));

        drop(_busy);
        unregister_task_waker(task_id);
        runtime_write_lock(registry()).remove(&id);
        gc::remove_extra_root(fut);
        assert_eq!(registered_task_waker_count(), before_wakers);
    }

    #[test]
    fn mass_cancellation_unregisters_task_wakers_and_releases_roots() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let before_wakers = registered_task_waker_count();
        let mut ids = Vec::new();
        let mut inputs = Vec::new();

        for _ in 0..512 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let task_id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
            let ctl = new_ctl();
            let held_locks = Arc::new(Mutex::new(Vec::new()));
            let poll_lock = Arc::new(Mutex::new(()));
            let done = Arc::new(AtomicBool::new(false));
            let cancel_requested = Arc::new(AtomicBool::new(false));
            let reschedule = Arc::new(AtomicBool::new(false));
            let fut = make_future_box(pending_never_poll) as usize;
            gc::add_extra_root(fut);

            ctl.inner.lock().unwrap().task_cancel = Some(TaskCancelCtl {
                task_id,
                cancel_requested: cancel_requested.clone(),
                done: done.clone(),
                reschedule: reschedule.clone(),
                held_locks: held_locks.clone(),
                poll_lock: poll_lock.clone(),
                input: fut,
                input_kind: TaskCancelInput::Future,
            });
            runtime_write_lock(registry()).insert(id, ctl.clone());

            let task = Task {
                id: task_id,
                work: TaskWork::Future {
                    fut,
                    pending_tid: 9,
                },
                ctl,
                held_locks,
                poll_lock,
                polling: Arc::new(AtomicBool::new(false)),
                queued: Arc::new(AtomicBool::new(false)),
                reschedule: Arc::new(AtomicBool::new(false)),
                done,
                cancel_requested,
            };
            register_task_waker(task);
            ids.push(id);
            inputs.push(fut);
        }

        assert_eq!(
            registered_task_waker_count(),
            before_wakers + ids.len(),
            "setup should register one persistent executor waker per task"
        );

        for id in &ids {
            assert!(cancel_task_by_id(*id));
        }

        assert_eq!(
            registered_task_waker_count(),
            before_wakers,
            "cancelling suspended executor tasks must remove their waker registry entries"
        );
        for input in inputs {
            assert_eq!(
                gc::extra_root_count_for(input),
                0,
                "cancelling a suspended task must release the executor handoff root"
            );
        }
        for id in ids {
            runtime_write_lock(registry()).remove(&id);
        }
    }

    #[test]
    fn reschedule_recorded_after_pending_swap_requeues_after_unlock() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        POST_UNLOCK_RACE_POLLS.store(0, O::SeqCst);
        let task = Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            work: TaskWork::Future {
                fut: make_future_box(post_unlock_race_pending_poll) as usize,
                pending_tid: 9,
            },
            ctl: new_ctl(),
            held_locks: Arc::new(Mutex::new(Vec::new())),
            poll_lock: Arc::new(Mutex::new(())),
            polling: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicBool::new(true)),
            reschedule: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        };
        PRE_POLL_UNLOCK_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|task| {
                assert!(
                    task.poll_lock.try_lock().is_err(),
                    "hook must run before the poll lock is released"
                );
                task.reschedule.store(true, Ordering::Release);
            }));
        });

        poll_task(task.clone());

        let started = std::time::Instant::now();
        while POST_UNLOCK_RACE_POLLS.load(O::SeqCst) < 2
            && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::yield_now();
        }
        assert_eq!(
            POST_UNLOCK_RACE_POLLS.load(O::SeqCst),
            2,
            "a wake recorded after the Pending-path reschedule swap must still schedule one follow-up poll"
        );
        assert!(!task.done.load(Ordering::Acquire));
        assert!(!task.reschedule.load(Ordering::Acquire));
    }
}
