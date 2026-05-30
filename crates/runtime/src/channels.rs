//! Typed message-passing channels (`docs/20` §2) — **async, non-blocking, and
//! deterministically closeable**.
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
//! a drained `recv` resolves to `ChannelClosed` and a `Receiver: Iterator`
//! (`for n in rx`) terminates, so we wake every parked receiver. When the last
//! **receiver** is released the channel is *closed for sending*: `send` returns
//! `ChannelClosed` instead of enqueuing (the message would never be observed).
//!
//! Queued values may be managed pointers that, while sitting in the queue, are
//! not referenced by any thread stack — so each is pinned as a global GC root
//! (`gc::add_extra_root`) on `send` and unpinned once a `recv` moves it into a
//! traced result slot, keeping it alive across the hand-off.

use crate::gc;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// A waker captured from a poll [`Context`]: the `(waker_data, wake_fn)` pair a
/// `send` invokes to re-poll a suspended receiver.
type Waker = (usize, extern "C" fn(*mut u8));

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
    /// Signalled when a message is enqueued or the channel becomes closed —
    /// wakes a synchronous blocking receiver (`Receiver: Iterator`, `docs/20`
    /// §2). The asynchronous future path uses `waiters` instead.
    not_empty: Condvar,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<Channel>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Channel>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
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
    registry().lock().unwrap().insert(id, ch);
    id
}

fn channel(id: u64) -> std::sync::Arc<Channel> {
    registry().lock().unwrap().get(&id).cloned().expect("invalid channel id")
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
    ch.inner.lock().unwrap().senders += 1;
}

/// Release a live `Sender`. When the count reaches zero the channel is *closed
/// for receiving*: a drained `recv` yields `ChannelClosed` and a blocking
/// `Receiver: Iterator` terminates, so we wake every parked receiver.
///
/// # Safety
/// `id` must be a live channel id; called once per acquired sender.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_sender_release(id: u64) {
    let ch = channel(id);
    let wakers = {
        let mut g = ch.inner.lock().unwrap();
        debug_assert!(g.senders > 0, "sender release underflow");
        g.senders = g.senders.saturating_sub(1);
        if g.senders == 0 {
            ch.not_empty.notify_all();
            std::mem::take(&mut g.waiters)
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
    ch.inner.lock().unwrap().receivers += 1;
}

/// Release a live `Receiver`. When the count reaches zero the channel is
/// *closed for sending*: subsequent `send`s return `ChannelClosed`.
///
/// # Safety
/// `id` must be a live channel id; called once per acquired receiver.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_receiver_release(id: u64) {
    let ch = channel(id);
    let mut g = ch.inner.lock().unwrap();
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
        let mut g = ch.inner.lock().unwrap();
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
        std::mem::take(&mut g.waiters)
    };
    // Wake a synchronous blocking receiver (`Receiver: Iterator`).
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

fn recv_box_desc() -> *const u8 {
    // Future box: [vtable @0][data @8 (managed)][type_id @16].
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(24, &[8]) as usize) as *const u8
}
fn recv_data_desc() -> *const u8 {
    // State: [chan_id][ready_tid][pending_tid][elem_is_ptr][value_tid][closed_tid]
    // — no managed ptrs.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(48, &[]) as usize) as *const u8
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

/// Build a `T | ChannelClosed` union box `{type_id, payload}` tagged with
/// `variant_tid`. `is_ptr` selects a traced payload slot for a managed value.
unsafe fn variant_box(variant_tid: i64, value: i64, is_ptr: bool) -> *mut u8 {
    let desc = if is_ptr { union_managed_desc() } else { union_plain_desc() };
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

    let mut g = ch.inner.lock().unwrap();
    if let Some(value) = g.queue.pop_front() {
        drop(g);
        // The result boxes must not be collected mid-build.
        gc::pause();
        let inner = unsafe { variant_box(value_tid, value, elem_is_ptr) };
        let r = unsafe { ready_box(ready_tid, inner) };
        gc::resume();
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
        gc::resume();
        return r;
    }
    // Queue empty but senders remain: register the executor's waker (under the
    // same lock `send` takes), then report Pending so the task suspends.
    let c = unsafe { &*ctx };
    g.waiters.push((c.waker_data as usize, c.wake_fn));
    drop(g);
    gc::pause();
    let r = unsafe { pending_box(pending_tid) };
    gc::resume();
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
    }
    let bx = unsafe { gc::alloc(recv_box_desc()) };
    unsafe {
        (bx as *mut usize).write(recv_vtable() as usize); // vtable @0
        ((bx as usize + 8) as *mut usize).write(data as usize); // data @8
        ((bx as usize + 16) as *mut i64).write(0); // type_id @16
    }
    gc::resume();
    bx
}

/// Blocking receive for `Receiver: Iterator` (`for n in rx`, `docs/20` §2).
/// Parks the calling OS thread (cooperating with the GC via `enter_native`)
/// until a message is available or the channel is closed-and-drained. Writes
/// `1` to `*got` and returns the value if one arrived, or writes `0` (the
/// channel is closed and empty → the iterator yields `Done`).
///
/// # Safety
/// `id` must be a live channel id; `got` must point to a writable `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_recv_blocking(id: u64, got: *mut i64) -> i64 {
    let ch = channel(id);
    let mut g = ch.inner.lock().unwrap();
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
        // Block until a send or the last-sender release notifies us. Tell the
        // GC we are parked in native code so a collection can proceed.
        gc::enter_native();
        g = ch.not_empty.wait(g).unwrap();
        gc::leave_native();
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
    let mut g = ch.inner.lock().unwrap();
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

    /// Acquiring and releasing senders drives the close-for-receiving flag; a
    /// drained blocking recv then terminates instead of parking forever.
    #[test]
    fn last_sender_release_closes_for_receiving() {
        let id = lang_channel_new();
        // Two producers: the initial sender plus one clone.
        unsafe { lang_chan_sender_acquire(id) };
        unsafe { lang_chan_send(id, 7) };
        // One producer drops — still open.
        unsafe { lang_chan_sender_release(id) };
        let mut got = 0i64;
        let v = unsafe { lang_chan_recv_blocking(id, &mut got) };
        assert_eq!((got, v), (1, 7), "drains the queued value");
        // Last producer drops — closed; blocking recv now reports Done.
        unsafe { lang_chan_sender_release(id) };
        let mut got2 = 0i64;
        let _ = unsafe { lang_chan_recv_blocking(id, &mut got2) };
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
        assert_eq!(unsafe { lang_chan_recv_blocking(id, &mut g) }, 1);
        assert_eq!(g, 1);
        assert_eq!(unsafe { lang_chan_recv_blocking(id, &mut g) }, 2);
        assert_eq!(g, 1);
        let _ = unsafe { lang_chan_recv_blocking(id, &mut g) };
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

    /// A blocking receiver parked on an empty channel is woken (and terminates)
    /// when the last sender is released from another thread.
    #[test]
    fn blocking_recv_wakes_on_close() {
        let id = lang_channel_new();
        let h = std::thread::spawn(move || {
            let mut got = 0i64;
            let v = unsafe { lang_chan_recv_blocking(id, &mut got) };
            (got, v)
        });
        // Give the receiver a moment to park, then close.
        std::thread::sleep(std::time::Duration::from_millis(20));
        unsafe { lang_chan_sender_release(id) };
        assert_eq!(h.join().unwrap().0, 0, "woken → Done on close");
    }
}
