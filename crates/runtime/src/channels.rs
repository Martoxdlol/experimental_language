//! Typed message-passing channels (`docs/20` §2) — **async and non-blocking**.
//!
//! A channel is an unbounded FIFO queue shared between `Sender` and `Receiver`
//! ends (the language-level structs carry only the integer channel id below).
//! `send` enqueues a message and wakes any task awaiting a value; it never
//! blocks (the queue is unbounded). `recv` is **asynchronous**: it builds a
//! `Future<T>` (`docs/21`) rather than parking the calling OS thread. Polling
//! that future pops the next message if one is ready, otherwise it registers
//! the executor's waker (from the poll [`Context`]) and reports `Pending`, so
//! the *task* suspends while the thread is free to do other work. A later
//! `send` invokes the stored waker, which re-polls the awaiting task.
//!
//! Queued values may be managed pointers that, while sitting in the queue, are
//! not referenced by any thread stack — so each is pinned as a global GC root
//! (`gc::add_extra_root`) on `send` and unpinned once a `recv` poll moves it
//! into the `Ready<T>` result box (a traced slot), keeping it alive across the
//! hand-off.

use crate::gc;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

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

/// Queue + parked-receiver wakers, guarded by one mutex so a receiver's
/// "queue empty → register waker" check and a sender's "enqueue → wake" are
/// atomic with respect to each other (no lost wakeups).
struct Inner {
    queue: VecDeque<i64>,
    waiters: Vec<Waker>,
}

struct Channel {
    inner: Mutex<Inner>,
}

fn registry() -> &'static Mutex<HashMap<u64, std::sync::Arc<Channel>>> {
    static R: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Channel>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Create a channel; returns its id (stored in the `Sender`/`Receiver` structs).
#[unsafe(no_mangle)]
pub extern "C" fn lang_channel_new() -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ch = std::sync::Arc::new(Channel {
        inner: Mutex::new(Inner { queue: VecDeque::new(), waiters: Vec::new() }),
    });
    registry().lock().unwrap().insert(id, ch);
    id
}

fn channel(id: u64) -> std::sync::Arc<Channel> {
    registry().lock().unwrap().get(&id).cloned().expect("invalid channel id")
}

/// Enqueue `value` (widened to a machine word) and wake every task awaiting a
/// message. Non-blocking: the queue is unbounded, so `send` always returns at
/// once.
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_send(id: u64, value: i64) {
    // Pin the value while it sits in the queue (no thread stack references it).
    gc::add_extra_root(value as usize);
    let ch = channel(id);
    let wakers = {
        let mut g = ch.inner.lock().unwrap();
        g.queue.push_back(value);
        // Take the parked wakers under the same lock the receiver registered
        // them under; each will re-poll and re-register if still empty.
        std::mem::take(&mut g.waiters)
    };
    for (data, wake) in wakers {
        wake(data as *mut u8);
    }
}

// -- recv: an asynchronous future --------------------------------------------
//
// `recv()` returns a `Future<T>` interface-object box (`docs/11` §5 / `docs/21`)
// `[vtable @0][data @8][type_id @16]`, vtable slot 0 = `chan_recv_poll`. Polling
// pops the next message (→ `Ready<T>`) or registers the executor waker and
// reports `Pending`.

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
    // State: [chan_id][ready_tid][pending_tid][elem_is_ptr] — no managed ptrs.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(32, &[]) as usize) as *const u8
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
fn value_managed_desc() -> *const u8 {
    // Ready<T>.value slot holding a managed pointer.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[0]) as usize) as *const u8
}
fn value_plain_desc() -> *const u8 {
    // Ready<T>.value slot holding a scalar.
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| make_desc(8, &[]) as usize) as *const u8
}
fn recv_vtable() -> *const u8 {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let f: extern "C" fn(*mut u8, *mut Context) -> *mut u8 = chan_recv_poll;
        Box::leak(Box::new([f as usize])) as *const [usize; 1] as usize
    }) as *const u8
}

/// Build a `Ready<T>` union box carrying `value` (widened); `is_ptr` selects a
/// traced value slot for a managed message.
unsafe fn ready_value_box(ready_tid: i64, value: i64, is_ptr: bool) -> *mut u8 {
    let desc = if is_ptr { value_managed_desc() } else { value_plain_desc() };
    let payload = unsafe { gc::alloc(desc) };
    unsafe { (payload as *mut i64).write(value) };
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

extern "C" fn chan_recv_poll(data: *mut u8, ctx: *mut Context) -> *mut u8 {
    // data: [chan_id @0][ready_tid @8][pending_tid @16][elem_is_ptr @24].
    let id = unsafe { (data as *const i64).read() } as u64;
    let ready_tid = unsafe { ((data as usize + 8) as *const i64).read() };
    let pending_tid = unsafe { ((data as usize + 16) as *const i64).read() };
    let elem_is_ptr = unsafe { ((data as usize + 24) as *const i64).read() } != 0;
    let ch = channel(id);

    let mut g = ch.inner.lock().unwrap();
    if let Some(value) = g.queue.pop_front() {
        drop(g);
        // The result box + payload must not be collected mid-build.
        gc::pause();
        let r = unsafe { ready_value_box(ready_tid, value, elem_is_ptr) };
        gc::resume();
        // The value now lives in the (traced) Ready slot; unpin the queue root.
        if elem_is_ptr {
            gc::remove_extra_root(value as usize);
        }
        return r;
    }
    // Queue empty: register the executor's waker (under the same lock `send`
    // takes), then report Pending so the task suspends.
    let c = unsafe { &*ctx };
    g.waiters.push((c.waker_data as usize, c.wake_fn));
    drop(g);
    gc::pause();
    let r = unsafe { pending_box(pending_tid) };
    gc::resume();
    r
}

/// Construct a `recv()` future (`docs/20` §2): a `Future<T>` that resolves to
/// the next message. `ready_tid` / `pending_tid` are the code generator's
/// `Ready<T>` and `Pending` type ids; `elem_is_ptr` is non-zero when `T` is a
/// managed (heap) type so the result slot is GC-traced.
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
) -> *mut u8 {
    gc::pause();
    let data = unsafe { gc::alloc(recv_data_desc()) };
    unsafe {
        (data as *mut i64).write(id as i64);
        ((data as usize + 8) as *mut i64).write(ready_tid);
        ((data as usize + 16) as *mut i64).write(pending_tid);
        ((data as usize + 24) as *mut i64).write(elem_is_ptr);
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

/// Non-blocking receive. Writes `1` to `*has` and returns the value if one was
/// available, else writes `0` and returns `0`.
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
