//! Typed message-passing channels (`docs/20` §2) — async `recv`,
//! non-blocking `send`/`try_recv`, and deterministic close.
//!
//! A channel is an unbounded FIFO queue shared between `Sender` and `Receiver`
//! ends (the language-level structs carry only the integer channel id below).
//! `send` enqueues a message and wakes any task awaiting a value; it never
//! blocks (the queue is unbounded). `recv` is **asynchronous**: it builds a
//! `Future<T | ChannelClosed>` (`docs/21`) rather than parking the calling OS
//! thread. Polling that future pops the next message if one is ready, reports
//! `ChannelClosed` once every sender has been dropped *and* the queue is
//! drained, otherwise registers the executor's waker and reports `Pending`.
//!
//! ## Deterministic close — reference-counted endpoints (`docs/16` §8)
//!
//! General managed `Drop` is best-effort / GC-timed, but channel endpoints are
//! **runtime handle types** with a stronger guarantee: each channel tracks a
//! live **sender count** and **receiver count**. The compiler emits
//! [`lang_chan_sender_acquire`]/[`lang_chan_sender_release`] (and the receiver
//! equivalents) at deterministic scope boundaries, so the channel closes the
//! instant its last sender is released — without waiting for a collection. This
//! is the deterministic-release carve-out described in `docs/16` §8; ordinary
//! user types are unaffected. The tracing GC remains the backstop reclaimer of
//! the underlying queue once both ends are unreachable.
//!
//! When the last **sender** is released the channel is *closed for receiving*:
//! a drained `recv` resolves to `ChannelClosed`, and async receiver iteration
//! (`for await n in rx`) terminates after the same close observation. When the
//! last **receiver** is released the channel is *closed for sending*: `send`
//! returns `ChannelClosed` instead of enqueuing (the message would never be
//! observed).
//!
//! Queued values may be managed pointers that, while sitting in the queue, are
//! not referenced by any thread stack — so each is pinned as a global GC root
//! (`gc::add_extra_root`) on `send` and unpinned once a `recv` moves it into a
//! traced result slot, keeping it alive across the hand-off.

use crate::gc;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

/// A waker captured from a poll [`Context`]: the `(waker_data, wake_fn)` pair a
/// `send` invokes to re-poll a suspended receiver.
type Waker = (usize, extern "C" fn(*mut u8));

fn same_waker(a: Waker, b: Waker) -> bool {
    a.0 == b.0 && a.1 as *const () as usize == b.1 as *const () as usize
}

fn prune_dead_waiters(waiters: &mut Vec<Waker>) {
    waiters.retain(|(data, wake)| {
        crate::threads::executor_waker_is_live(*data as *mut u8, *wake) != Some(false)
    });
}

fn register_waiter(waiters: &mut Vec<Waker>, waiter: Waker) {
    prune_dead_waiters(waiters);
    let task_id = crate::threads::executor_waker_task_id(waiter.0 as *mut u8, waiter.1);
    waiters.retain(|existing| {
        if same_waker(*existing, waiter) {
            return false;
        }
        match task_id {
            Some(task_id) => {
                crate::threads::executor_waker_task_id(existing.0 as *mut u8, existing.1)
                    != Some(task_id)
            }
            None => true,
        }
    });
    waiters.push(waiter);
}

fn drain_waiters(waiters: &mut Vec<Waker>) -> Vec<Waker> {
    prune_dead_waiters(waiters);
    std::mem::take(waiters)
}

fn remove_waiter(waiters: &mut Vec<Waker>, waiter: Waker) {
    waiters.retain(|existing| !same_waker(*existing, waiter));
}

/// The waker context handed to a future's `poll` (`docs/21` §2). Layout matches
/// the language `extern struct Context`; see `async_rt` for the canonical copy.
#[repr(C)]
struct Context {
    waker_data: *mut u8,
    wake_fn: extern "C" fn(*mut u8),
}

/// Queue + parked-receiver wakers + endpoint reference counts, guarded by one
/// mutex so a receiver's "queue empty → register waker" check and a sender's
/// "enqueue → wake" (and the sender/receiver count transitions) are atomic with
/// respect to each other (no lost wakeups, no torn close decision).
struct Inner {
    queue: VecDeque<i64>,
    waiters: Vec<Waker>,
    /// Number of live `Sender` handles. Starts at 1 (the sender returned by
    /// `channel<T>()`); `0` means *closed for receiving*.
    senders: u64,
    /// Number of live `Receiver` handles. Starts at 1; `0` means *closed for
    /// sending* (no consumer can ever observe further messages).
    receivers: u64,
}

struct Channel {
    inner: Mutex<Inner>,
    /// Signalled when a message is enqueued or the channel becomes closed.
    /// Public Otter Fusion waits use `waiters`; this condvar is private runtime
    /// machinery for host-side tests and cleanup paths.
    not_empty: Condvar,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<Channel>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Channel>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
fn wait_unpoison<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(|err| err.into_inner())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Create a channel; returns its id (stored in the `Sender`/`Receiver` structs).
/// The channel starts with exactly one sender and one receiver — the pair
/// `channel<T>()` returns.
#[unsafe(no_mangle)]
pub extern "C" fn lang_channel_new() -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ch = std::sync::Arc::new(Channel {
        inner: Mutex::new(Inner {
            queue: VecDeque::new(),
            waiters: Vec::new(),
            senders: 1,
            receivers: 1,
        }),
        not_empty: Condvar::new(),
    });
    lock_unpoison(registry()).insert(id, ch);
    id
}

fn channel(id: u64) -> std::sync::Arc<Channel> {
    lock_unpoison(registry())
        .get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("invalid channel id {id}"))
}

// -- endpoint reference counting (deterministic close, `docs/16` §8) ----------

/// Register an additional live `Sender` (a `Sender.clone()` produced one more
/// producer, `docs/20` §2). Balances a later [`lang_chan_sender_release`].
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_sender_acquire(id: u64) {
    let ch = channel(id);
    lock_unpoison(&ch.inner).senders += 1;
}

/// Release a live `Sender`. When the count reaches zero the channel is *closed
/// for receiving*: a drained `recv` yields `ChannelClosed`, async receiver
/// iteration terminates, and any private host-side waiters are woken.
///
/// # Safety
/// `id` must be a live channel id; called once per acquired sender.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_sender_release(id: u64) {
    let ch = channel(id);
    let wakers = {
        let mut g = lock_unpoison(&ch.inner);
        debug_assert!(g.senders > 0, "sender release underflow");
        g.senders = g.senders.saturating_sub(1);
        if g.senders == 0 {
            ch.not_empty.notify_all();
            drain_waiters(&mut g.waiters)
        } else {
            Vec::new()
        }
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

/// Register an additional live `Receiver`. (MPSC receivers are not `Clone`, but
/// the symmetry keeps the MPMC variant and any future hand-off uniform.)
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_receiver_acquire(id: u64) {
    let ch = channel(id);
    lock_unpoison(&ch.inner).receivers += 1;
}

/// Release a live `Receiver`. When the count reaches zero the channel is
/// *closed for sending*: subsequent `send`s return `ChannelClosed`.
///
/// # Safety
/// `id` must be a live channel id; called once per acquired receiver.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_receiver_release(id: u64) {
    let ch = channel(id);
    let mut g = lock_unpoison(&ch.inner);
    debug_assert!(g.receivers > 0, "receiver release underflow");
    g.receivers = g.receivers.saturating_sub(1);
}

// -- send ---------------------------------------------------------------------

/// Enqueue `value` (widened to a machine word) and wake every task awaiting a
/// message. Non-blocking: the queue is unbounded, so `send` always returns at
/// once. Returns `0` on success, or `1` (`ChannelClosed`) when every receiver
/// has been dropped — the message is *not* enqueued in that case.
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_send(id: u64, value: i64) -> i64 {
    let ch = channel(id);
    let wakers = {
        let mut g = lock_unpoison(&ch.inner);
        if g.receivers == 0 {
            // Closed for sending — nobody can ever observe this message.
            return 1;
        }
        // Pin the value while it sits in the queue (no thread stack references
        // it). Done under the lock so it is paired with the push.
        gc::add_extra_root(value as usize);
        g.queue.push_back(value);
        // Take the parked async wakers under the same lock the receiver
        // registered them under; each will re-poll and re-register if still
        // empty.
        drain_waiters(&mut g.waiters)
    };
    // Wake a private host-side waiter.
    ch.not_empty.notify_one();
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
    0
}

// -- recv: an asynchronous future --------------------------------------------
//
// `recv()` returns a `Future<T | ChannelClosed>` interface-object box
// (`docs/11` §5 / `docs/21`) `[vtable @0][data @8][type_id @16]`, vtable slot 0
// = `chan_recv_poll`. Polling pops the next message (→ `Ready<T | ChannelClosed>`
// carrying the value), reports `ChannelClosed` once drained and all senders are
// gone, or registers the executor waker and reports `Pending`.

/// Build (once) and leak a descriptor blob:
/// `[size][kind=plain][type_id=0][n_ptrs][offsets…][n_rc=0]` (`docs/16`). The
/// mandatory trailing `n_rc` word (here `0` — these boxes own no `@RefCounted`
/// fields) is read by the collector for every object it reclaims.
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

fn recv_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn recv_data_desc() -> *const u8 {
    // State:
    // [chan_id][ready_tid][pending_tid][elem_is_ptr][value_tid][closed_tid]
    // [waker_data][wake_fn][registered_waiter] — no managed ptrs.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(72, &[]) as usize) as *const u8
}
fn union_managed_desc() -> *const u8 {
    // A `{type_id, payload @8 (managed)}` box (Ready wrapper, or a managed
    // `T | ChannelClosed` variant box).
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[8]) as usize) as *const u8
}
fn union_plain_desc() -> *const u8 {
    // A `{type_id, payload @8 (scalar/null)}` box.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(16, &[]) as usize) as *const u8
}
fn ready_managed_desc() -> *const u8 {
    // `Ready<Out>.value` slot — Out (a `T | ChannelClosed` union) is always a
    // managed box, so the slot is traced.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn recv_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = chan_recv_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Private marker for runtime-built channel receive futures. This lets the
/// shared future cancellation hook distinguish them from generated futures and
/// remove their pinned handoff state promptly.
pub(crate) const CHAN_RECV_FUTURE_KIND: i64 = -0x4348_414e_5f52_4543i64;

fn active_recv_data() -> &'static Mutex<HashSet<usize>> {
    static R: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

fn finish_recv_data(data: usize) -> bool {
    if lock_unpoison(active_recv_data()).remove(&data) {
        gc::remove_extra_root(data);
        true
    } else {
        false
    }
}

/// Build a `T | ChannelClosed` union box `{type_id, payload}` tagged with
/// `variant_tid`. `is_ptr` selects a traced payload slot for a managed value.
unsafe fn variant_box(variant_tid: i64, value: i64, is_ptr: bool) -> *mut u8 {
    let desc = if is_ptr {
        union_managed_desc()
    } else {
        union_plain_desc()
    };
    let bx = unsafe { gc::alloc(desc) };
    unsafe {
        (bx as *mut i64).write(variant_tid);
        ((bx as usize + 8) as *mut i64).write(value);
    }
    bx
}

/// Wrap an already-built `Out` box in `Ready<Out>` then in a `Ready<Out> |
/// Pending` union box tagged with `ready_tid` (the `poll` result shape,
/// `docs/21` §1). `out_box` is a managed pointer (the `T | ChannelClosed` box).
unsafe fn ready_box(ready_tid: i64, out_box: *mut u8) -> *mut u8 {
    let ready = unsafe { gc::alloc(ready_managed_desc()) };
    unsafe { (ready as *mut usize).write(out_box as usize) };
    let bx = unsafe { gc::alloc(union_managed_desc()) };
    unsafe {
        (bx as *mut i64).write(ready_tid);
        ((bx as usize + 8) as *mut usize).write(ready as usize);
    }
    bx
}

/// Build a `Pending` union box tagged with `pending_tid`.
unsafe fn pending_box(pending_tid: i64) -> *mut u8 {
    let bx = unsafe { gc::alloc(union_plain_desc()) };
    unsafe { (bx as *mut i64).write(pending_tid) };
    bx
}

extern "C" fn chan_recv_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    // data: [chan_id][ready_tid][pending_tid][elem_is_ptr][value_tid][closed_tid].
    let id = unsafe { (data as *const i64).read() } as u64;
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let elem_is_ptr = unsafe { ((data as usize + 24) as *const i64).read() } != 0;
    let value_tid = unsafe { ((data as usize + 32) as *const i64).read() };
    let closed_tid = unsafe { ((data as usize + 40) as *const i64).read() };
    let ch = channel(id);

    let mut g = lock_unpoison(&ch.inner);
    if let Some(value) = g.queue.pop_front() {
        drop(g);
        // The result boxes must not be collected mid-build.
        gc::pause();
        let inner = unsafe { variant_box(value_tid, value, elem_is_ptr) };
        let r = unsafe { ready_box(ready_tid, inner) };
        gc::resume_with_return_root(r as usize);
        finish_recv_data(data as usize);
        // The value now lives in the (traced) variant slot; unpin the queue root.
        if elem_is_ptr {
            gc::remove_extra_root(value as usize);
        }
        return r;
    }
    if g.senders == 0 {
        // Drained and every sender is gone: resolve to `ChannelClosed`.
        drop(g);
        gc::pause();
        let inner = unsafe { variant_box(closed_tid, 0, false) };
        let r = unsafe { ready_box(ready_tid, inner) };
        gc::resume_with_return_root(r as usize);
        finish_recv_data(data as usize);
        return r;
    }
    // Queue empty but senders remain: register the executor's waker (under the
    // same lock `send` takes), then report Pending so the task suspends.
    let c = unsafe { &*ctx };
    let waiter = (c.waker_data as usize, c.wake_fn);
    register_waiter(&mut g.waiters, waiter);
    unsafe {
        ((data as usize + 48) as *mut usize).write(waiter.0);
        ((data as usize + 56) as *mut usize).write(waiter.1 as usize);
        ((data as usize + 64) as *mut i64).write(1);
    }
    drop(g);
    gc::pause();
    let r = unsafe { pending_box(pending_tid) };
    gc::resume_with_return_root(r as usize);
    r
}

/// Construct a `recv()` future (`docs/20` §2): a `Future<T | ChannelClosed>`
/// that resolves to the next message or to `ChannelClosed` once the channel is
/// drained and closed. `ready_tid`/`pending_tid` are the `Ready<Out>`/`Pending`
/// type ids; `value_tid`/`closed_tid` tag the `T` and `ChannelClosed` variants
/// of the resolved `Out` union; `elem_is_ptr` is non-zero when `T` is managed.
///
/// # Safety
/// Callable only from generated code with the runtime initialised; `id` must be
/// a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_recv_future(
    id: u64,
    ready_tid: i64,
    pending_tid: i64,
    elem_is_ptr: i64,
    value_tid: i64,
    closed_tid: i64,
) -> *mut u8 {
    gc::pause();
    let data = unsafe { gc::alloc(recv_data_desc()) };
    unsafe {
        (data as *mut i64).write(id as i64);
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
        ((data as usize + 24) as *mut i64).write(elem_is_ptr);
        ((data as usize + 32) as *mut i64).write(value_tid);
        ((data as usize + 40) as *mut i64).write(closed_tid);
        ((data as usize + 48) as *mut usize).write(0);
        ((data as usize + 56) as *mut usize).write(0);
        ((data as usize + 64) as *mut i64).write(0);
    }
    let bx = unsafe { gc::alloc(recv_box_desc()) };
    unsafe {
        (bx as *mut usize).write(recv_vtable() as usize); // vtable @0
        ((bx as usize + 8) as *mut usize).write(data as usize); // data @8
        ((bx as usize + 16) as *mut i64).write(CHAN_RECV_FUTURE_KIND); // type_id @16
    }
    // The future data carries only scalar ids, so it can be invisible to stack
    // scans in the tiny window between construction and the generated async
    // frame storing the returned future. Pin it until the future resolves.
    gc::add_extra_root(data as usize);
    lock_unpoison(active_recv_data()).insert(data as usize);
    gc::resume_with_return_root(bx as usize);
    bx
}

/// Cancel a pending `recv()` future. This removes its parked waiter, if any,
/// and releases the construction handoff root exactly once. Returns true when
/// `fut` has the runtime channel-recv shape, even if an earlier cancel/resolve
/// already consumed the active state.
pub(crate) unsafe fn cancel_recv_future(fut: *mut u8) -> bool {
    if fut.is_null() {
        return false;
    }
    let kind = unsafe { ((fut as usize + 16) as *const i64).read() };
    if kind != CHAN_RECV_FUTURE_KIND {
        return false;
    }
    let data = unsafe { ((fut as usize + 8) as *const usize).read() };
    let registered = unsafe { ((data + 64) as *const i64).read() } != 0;
    if registered {
        let id = unsafe { (data as *const i64).read() } as u64;
        let waker_data = unsafe { ((data + 48) as *const usize).read() };
        let wake_fn_addr = unsafe { ((data + 56) as *const usize).read() };
        if wake_fn_addr != 0 {
            let wake_fn: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(wake_fn_addr) };
            let ch = channel(id);
            remove_waiter(&mut lock_unpoison(&ch.inner).waiters, (waker_data, wake_fn));
        }
        unsafe { ((data + 64) as *mut i64).write(0) };
    }
    finish_recv_data(data);
    true
}

/// Private `cfg(test)` receive helper used by runtime tests. Public Otter Fusion
/// source and generated code cannot call this path; receiver waits lower through
/// async futures.
/// Parks the private helper thread (cooperating with the GC via
/// `gc::native_wait`)
/// until a message is available or the channel is closed-and-drained. Writes
/// `1` to `*got` and returns the value if one arrived, or writes `0` when the
/// channel is closed and empty.
///
/// # Safety
/// `id` must be a live channel id; `got` must point to a writable `i64`.
#[cfg(test)]
pub(crate) unsafe fn chan_recv_native_wait_for_runtime_tests(id: u64, got: *mut i64) -> i64 {
    let ch = channel(id);
    let mut g = lock_unpoison(&ch.inner);
    loop {
        if let Some(value) = g.queue.pop_front() {
            drop(g);
            gc::remove_extra_root(value as usize);
            unsafe { got.write(1) };
            return value;
        }
        if g.senders == 0 {
            // Closed and drained → terminate the iterator.
            unsafe { got.write(0) };
            return 0;
        }
        // Wait until a send or the last-sender release notifies us. The
        // helper marks this mutator as native only for the host wait.
        g = gc::native_wait(|| wait_unpoison(&ch.not_empty, g));
    }
}

/// Non-blocking receive. Writes `1` to `*has` and returns the value if one was
/// available, else writes `0` and returns `0` (the queue is empty — whether the
/// channel is open or closed; `try_recv` reports `T | null`, `docs/20` §2).
///
/// # Safety
/// `id` must be a live channel id; `has` must point to a writable `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_try_recv(id: u64, has: *mut i64) -> i64 {
    let ch = channel(id);
    let mut g = lock_unpoison(&ch.inner);
    match g.queue.pop_front() {
        Some(value) => {
            drop(g);
            gc::remove_extra_root(value as usize);
            unsafe { has.write(1) };
            value
        }
        None => {
            unsafe { has.write(0) };
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static WOKEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    extern "C" fn count_wake(_: *mut u8) {
        WOKEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn poll_recv_once(fut: *mut u8) -> *mut u8 {
        let vtable = unsafe { (fut as *const usize).read() } as *const usize;
        let poll: extern "C" fn(*mut u8, *mut Context) -> *mut u8 =
            unsafe { std::mem::transmute(vtable.read()) };
        let data = unsafe { ((fut as usize + 8) as *const usize).read() } as *mut u8;
        let mut ctx = Context {
            waker_data: std::ptr::null_mut(),
            wake_fn: count_wake,
        };
        poll(data, &mut ctx)
    }

    /// Acquiring and releasing senders drives the close-for-receiving flag; a
    /// drained private runtime-test receive then terminates instead of parking.
    #[test]
    fn last_sender_release_closes_for_receiving() {
        let id = lang_channel_new();
        // Two producers: the initial sender plus one clone.
        unsafe { lang_chan_sender_acquire(id) };
        unsafe { lang_chan_send(id, 7) };
        // One producer drops — still open.
        unsafe { lang_chan_sender_release(id) };
        let mut got = 0i64;
        let v = unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) };
        assert_eq!((got, v), (1, 7), "drains the queued value");
        // Last producer drops; the private receive helper now reports Done.
        unsafe { lang_chan_sender_release(id) };
        let mut got2 = 0i64;
        let _ = unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got2) };
        assert_eq!(got2, 0, "closed + drained → Done");
    }

    /// Queued messages are drained *before* the channel reports closed, even
    /// when the last sender is released while values remain enqueued.
    #[test]
    fn drain_then_close() {
        let id = lang_channel_new();
        unsafe { lang_chan_send(id, 1) };
        unsafe { lang_chan_send(id, 2) };
        unsafe { lang_chan_sender_release(id) }; // close while non-empty
        let mut g = 0i64;
        assert_eq!(
            unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut g) },
            1
        );
        assert_eq!(g, 1);
        assert_eq!(
            unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut g) },
            2
        );
        assert_eq!(g, 1);
        let _ = unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut g) };
        assert_eq!(g, 0, "now drained and closed");
    }

    /// Dropping the receiver closes the channel for sending; further sends are
    /// rejected with `ChannelClosed` (return `1`).
    #[test]
    fn receiver_drop_closes_for_sending() {
        let id = lang_channel_new();
        assert_eq!(unsafe { lang_chan_send(id, 1) }, 0, "open: send ok");
        unsafe { lang_chan_receiver_release(id) };
        assert_eq!(unsafe { lang_chan_send(id, 2) }, 1, "closed: ChannelClosed");
    }

    /// A private runtime-test receiver parked on an empty channel is woken and
    /// terminates when the last sender is released from another thread.
    #[test]
    fn runtime_test_recv_wakes_on_close() {
        let id = lang_channel_new();
        let h = std::thread::spawn(move || {
            let mut got = 0i64;
            let v = unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) };
            (got, v)
        });
        // Give the receiver a moment to park, then close.
        std::thread::sleep(std::time::Duration::from_millis(20));
        unsafe { lang_chan_sender_release(id) };
        assert_eq!(h.join().unwrap().0, 0, "woken → Done on close");
    }

    #[test]
    fn runtime_test_recv_wait_uses_gc_native_state_marker() {
        let src = include_str!("channels.rs");
        assert!(
            src.contains("g = gc::native_wait(|| wait_unpoison(&ch.not_empty, g));"),
            "private host-side channel waits must stay inside gc::native_wait(...)"
        );
    }

    #[test]
    fn registry_lock_recovers_after_poison() {
        let h = std::thread::spawn(|| {
            let _guard = registry().lock().unwrap();
            panic!("poison channel registry");
        });
        assert!(h.join().is_err());

        let id = lang_channel_new();
        assert_eq!(unsafe { lang_chan_send(id, 11) }, 0);
        let mut got = 0i64;
        assert_eq!(unsafe { lang_chan_try_recv(id, &mut got) }, 11);
        assert_eq!(got, 1);
    }

    #[test]
    fn channel_queue_lock_recovers_after_poison() {
        let id = lang_channel_new();
        let ch = channel(id);
        let h = std::thread::spawn(move || {
            let _guard = ch.inner.lock().unwrap();
            panic!("poison channel queue");
        });
        assert!(h.join().is_err());

        assert_eq!(unsafe { lang_chan_send(id, 23) }, 0);
        let mut got = 0i64;
        assert_eq!(
            unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) },
            23
        );
        assert_eq!(got, 1);
        unsafe { lang_chan_sender_release(id) };
        let _ = unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) };
        assert_eq!(got, 0);
    }

    #[test]
    fn duplicate_channel_waiters_are_coalesced() {
        extern "C" fn wake_noop(_: *mut u8) {}

        let mut waiters = Vec::new();
        let a = 1usize;
        let b = 2usize;

        register_waiter(&mut waiters, (a, wake_noop));
        register_waiter(&mut waiters, (a, wake_noop));
        register_waiter(&mut waiters, (b, wake_noop));

        assert_eq!(waiters.len(), 2, "exact duplicate wakers are coalesced");
        assert!(waiters.iter().any(|w| same_waker(*w, (a, wake_noop))));
        assert!(waiters.iter().any(|w| same_waker(*w, (b, wake_noop))));
    }

    #[test]
    fn duplicate_executor_task_waiters_replace_previous_registration() {
        let (data, wake) = crate::threads::test_executor_waker();
        let mut waiters = Vec::new();

        register_waiter(&mut waiters, (data as usize, wake));
        register_waiter(&mut waiters, (data as usize, wake));

        assert_eq!(
            waiters.len(),
            1,
            "a task repeatedly polling the same recv future keeps one waiter"
        );
    }

    #[test]
    fn stale_executor_task_waiters_are_pruned() {
        extern "C" fn wake_noop(_: *mut u8) {}

        let stale: Waker = (usize::MAX - 17, crate::threads::task_wake);
        let live: Waker = (3usize, wake_noop);
        let mut waiters = vec![stale, live];

        prune_dead_waiters(&mut waiters);

        assert_eq!(waiters.len(), 1, "dead executor task waiter is dropped");
        assert!(same_waker(waiters[0], live));
    }

    #[test]
    fn send_wakes_each_coalesced_waiter_once() {
        WOKEN.store(0, std::sync::atomic::Ordering::SeqCst);
        let id = lang_channel_new();
        {
            let ch = channel(id);
            let mut g = lock_unpoison(&ch.inner);
            register_waiter(&mut g.waiters, (42, count_wake));
            register_waiter(&mut g.waiters, (42, count_wake));
        }

        assert_eq!(unsafe { lang_chan_send(id, 99) }, 0);
        assert_eq!(
            WOKEN.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "duplicate waiters must not amplify wakeups"
        );
        assert!(channel(id).inner.lock().unwrap().waiters.is_empty());

        let mut got = 0;
        assert_eq!(
            unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) },
            99
        );
        assert_eq!(got, 1);
    }

    #[test]
    fn cancelling_recv_future_before_poll_releases_handoff_root_once() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        let id = lang_channel_new();
        let fut = unsafe { lang_chan_recv_future(id, 7, 9, 0, 11, 12) };
        let data = unsafe { ((fut as usize + 8) as *const usize).read() };
        assert_eq!(
            gc::extra_root_count_for(data),
            1,
            "recv future data is pinned across the generated-code handoff"
        );

        unsafe { crate::threads::lang_future_cancel(fut) };
        assert_eq!(
            gc::extra_root_count_for(data),
            0,
            "cancelling an unpolled recv future must release its handoff root"
        );

        unsafe { crate::threads::lang_future_cancel(fut) };
        assert_eq!(
            gc::extra_root_count_for(data),
            0,
            "recv cancellation must be repeatable"
        );
        unsafe { lang_chan_sender_release(id) };
    }

    #[test]
    fn cancelling_pending_recv_future_removes_waiter_and_root() {
        let _g = crate::gc::TEST_LOCK.lock().unwrap();
        WOKEN.store(0, std::sync::atomic::Ordering::SeqCst);
        let id = lang_channel_new();
        let fut = unsafe { lang_chan_recv_future(id, 7, 9, 0, 11, 12) };
        let data = unsafe { ((fut as usize + 8) as *const usize).read() };

        let pending = poll_recv_once(fut);
        assert_eq!(unsafe { (pending as *const i64).read() }, 9);
        assert_eq!(channel(id).inner.lock().unwrap().waiters.len(), 1);

        unsafe { crate::threads::lang_future_cancel(fut) };

        assert_eq!(
            gc::extra_root_count_for(data),
            0,
            "cancelling a pending recv future must release its handoff root"
        );
        assert!(
            channel(id).inner.lock().unwrap().waiters.is_empty(),
            "cancelling a pending recv future must remove its parked waiter"
        );

        assert_eq!(unsafe { lang_chan_send(id, 99) }, 0);
        assert_eq!(
            WOKEN.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a cancelled recv future must not leave a stale live waiter behind"
        );
        let mut got = 0;
        assert_eq!(
            unsafe { chan_recv_native_wait_for_runtime_tests(id, &mut got) },
            99
        );
        assert_eq!(got, 1);
        unsafe { lang_chan_sender_release(id) };
    }
}
