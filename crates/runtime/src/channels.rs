//! Typed message-passing channels (`docs/20` §2).
//!
//! A channel is an unbounded FIFO queue shared between `Sender` and `Receiver`
//! ends (the language-level structs carry only the integer channel id below).
//! `send` enqueues and wakes a waiting receiver; `recv` blocks (in GC *native*
//! state, so the blocked thread stays scannable) until a message arrives.
//!
//! Queued values may be managed pointers that, while sitting in the queue, are
//! not referenced by any thread stack — so each is pinned as a global GC root
//! (`gc::add_extra_root`) on `send` and unpinned on `recv`, keeping it alive
//! across the hand-off.

use crate::gc;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

struct Channel {
    queue: Mutex<VecDeque<i64>>,
    cv: Condvar,
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
    let ch = std::sync::Arc::new(Channel { queue: Mutex::new(VecDeque::new()), cv: Condvar::new() });
    registry().lock().unwrap().insert(id, ch);
    id
}

fn channel(id: u64) -> std::sync::Arc<Channel> {
    registry().lock().unwrap().get(&id).cloned().expect("invalid channel id")
}

/// Enqueue `value` (widened to a machine word) and wake a waiting receiver.
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_send(id: u64, value: i64) {
    // Pin the value while it sits in the queue (no thread stack references it).
    gc::add_extra_root(value as usize);
    let ch = channel(id);
    ch.queue.lock().unwrap().push_back(value);
    ch.cv.notify_one();
}

/// Block until a message is available, then return it (FIFO). The caller is in
/// native GC state while blocked, so a collection on another thread can proceed.
///
/// # Safety
/// `id` must be a live channel id from [`lang_channel_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_recv(id: u64) -> i64 {
    let ch = channel(id);
    gc::enter_native();
    let mut q = ch.queue.lock().unwrap();
    while q.is_empty() {
        q = ch.cv.wait(q).unwrap();
    }
    let value = q.pop_front().unwrap();
    drop(q);
    gc::leave_native();
    // The receiver now holds the value on its (scanned) stack; unpin it.
    gc::remove_extra_root(value as usize);
    value
}

/// Non-blocking receive. Writes `1` to `*has` and returns the value if one was
/// available, else writes `0` and returns `0`.
///
/// # Safety
/// `id` must be a live channel id; `has` must point to a writable `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_chan_try_recv(id: u64, has: *mut i64) -> i64 {
    let ch = channel(id);
    let mut q = ch.queue.lock().unwrap();
    match q.pop_front() {
        Some(value) => {
            drop(q);
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
