//! The managed-heap allocator (`docs/16` §9), with per-thread allocation buffers
//! (TLABs).
//!
//! GC objects are *not* allocated with the system `malloc`/`free`. They come
//! from this slab-backed, size-segregated allocator, for two reasons:
//!
//!  1. **The collector must never contend with mutators on the system-malloc
//!     lock.** A stop-the-world sweep frees thousands of objects; if those frees
//!     went through `free`, and a mutator thread were parked *inside* `malloc`
//!     holding its lock, the collector would block on that lock forever. By
//!     reclaiming into our own free lists (a separate lock the parked mutators
//!     do not hold), the sweep is system-malloc-free and cannot deadlock. This
//!     is the documented prerequisite for concurrent reclamation
//!     (`docs/20`/`ROADMAP.md`).
//!  2. **Reuse.** Freed blocks are recycled by size class, so steady-state
//!     allocation touches the system allocator only when the live set grows —
//!     fewer syscalls, better locality than per-object `malloc`/`free`.
//!
//! ## Size classes
//!
//! Objects are rounded up to a *size class* (16-byte granularity up to 512 B,
//! then power-of-two up to `LARGE_MAX`). Blocks larger than `LARGE_MAX` are
//! served one-to-one from a dedicated exact-size free list — still never
//! returned to the system, so the sweep stays lock-clean. Memory is retained for
//! the process lifetime (bounded by the peak live set), the standard space/safety
//! trade for a tracing GC.
//!
//! ## Per-thread allocation buffers (TLABs)
//!
//! Every allocation used to take the one global allocator lock; under multiple
//! mutators that lock serialized all allocation (`ROADMAP.md` → "Per-thread
//! TLABs"). Each thread now owns a [`LocalCache`]:
//!
//!  * a **bump region** — a private chunk carved from the global slabs, sliced
//!    by a thread-local cursor with no lock; and
//!  * **per-class local free lists** — refilled in batches from the global free
//!    lists.
//!
//! `alloc` first pops a same-class block from the thread-local free list, then
//! tries the thread-local bump region, and only on a miss takes the global lock
//! to **refill** (pull a batch of recycled blocks, or carve a fresh bump chunk).
//! So the global lock is touched roughly once per [`REFILL_BATCH`] allocations or
//! once per bump chunk — not once per allocation.
//!
//! Mutators **never** call [`free`]: reclamation is the collector's job (sweep /
//! finalizer / `@RefCounted` release / teardown). [`free`] therefore always
//! returns the block to the *global* free lists, so freed memory is reusable by
//! every thread (not stranded on the freeing thread's cache); a thread's local
//! free lists are filled only by refilling *from* the global lists. On thread
//! exit the local free blocks are flushed back to the global lists; the unused
//! tail of the bump region is abandoned (bounded waste, ≤ one chunk per thread).
//!
//! All returned blocks are 16-byte aligned and zeroed (the collector relies on a
//! zero mark word and callers rely on zero-initialized fields). Fresh bump
//! memory is already zero (it comes from `alloc_zeroed`); recycled blocks are
//! re-zeroed when popped from a local free list.

use std::alloc::{Layout, alloc_zeroed};
use std::cell::RefCell;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Default slab size carved into bump chunks: 1 MiB.
const SLAB: usize = 1024 * 1024;
/// Alignment of every block (and of each slab base).
const ALIGN: usize = 16;
/// Largest size served from a power-of-two class; bigger requests use the
/// exact-size large-object free list (and bypass the per-thread cache).
const LARGE_MAX: usize = 1 << 20; // 1 MiB

/// Size of a thread's private bump region, refilled from the global slabs. Large
/// enough to amortize the global lock across many small allocations, small
/// enough that a thread's abandoned tail at exit is negligible.
const TLAB_CHUNK: usize = 256 * 1024; // 256 KiB
/// How many recycled blocks a thread pulls from the global free list per refill.
const REFILL_BATCH: usize = 64;

/// Number of cached size-class indices (see [`class_index`]): 32 small classes
/// (16 B … 512 B at 16 B granularity) + 11 power-of-two classes (1 KiB … 1 MiB).
const NUM_CLASSES: usize = 32 + 11;

// --- size classes ----------------------------------------------------------

/// Round a requested byte count up to its size class. The mapping is a pure
/// function of `total`, so [`free`] and [`alloc`] agree on the class.
fn size_class(total: usize) -> usize {
    let total = total.max(ALIGN);
    if total <= 512 {
        // 16-byte granularity.
        (total + 15) & !15
    } else if total <= LARGE_MAX {
        // Next power of two (≤ 2× internal fragmentation).
        total.next_power_of_two()
    } else {
        // Large objects: exact size rounded to alignment, one block per request.
        (total + ALIGN - 1) & !(ALIGN - 1)
    }
}

/// The cache slot index for a *class size* (the output of [`size_class`]), or
/// `None` for large objects (which bypass the per-thread cache). Inverse of the
/// `size_class` ranges:
///   * `16..=512` step 16  → `0..=31`
///   * `1024, 2048, … 2^20` → `32..=42`
#[inline]
fn class_index(class: usize) -> Option<usize> {
    if class <= 512 {
        Some(class / 16 - 1)
    } else if class <= LARGE_MAX {
        // class is a power of two in `2^10 ..= 2^20`.
        Some(32 + (class.trailing_zeros() as usize - 10))
    } else {
        None
    }
}

// --- global allocator -------------------------------------------------------

struct GlobalAlloc {
    /// `class size → stack of free block base addresses` for the cached classes,
    /// indexed by [`class_index`]. The sweep returns blocks here and threads
    /// refill from here.
    free: Vec<Vec<usize>>,
    /// Exact-size free lists for large objects (`> LARGE_MAX`), keyed by rounded
    /// size. Large blocks bypass the per-thread cache entirely.
    large_free: std::collections::HashMap<usize, Vec<usize>>,
    /// Retained slabs `(base, size)` — kept for teardown/reset only.
    slabs: Vec<(usize, usize)>,
    /// Bump region within the current slab, used to carve fresh per-thread
    /// chunks and large blocks.
    cursor: usize,
    end: usize,
}

/// The process-global managed-heap allocator (lazily initialized).
fn global() -> &'static Mutex<GlobalAlloc> {
    static A: OnceLock<Mutex<GlobalAlloc>> = OnceLock::new();
    A.get_or_init(|| {
        Mutex::new(GlobalAlloc {
            free: (0..NUM_CLASSES).map(|_| Vec::new()).collect(),
            large_free: std::collections::HashMap::new(),
            slabs: Vec::new(),
            cursor: 0,
            end: 0,
        })
    })
}

fn global_lock() -> MutexGuard<'static, GlobalAlloc> {
    global().lock().unwrap_or_else(|err| err.into_inner())
}

impl GlobalAlloc {
    /// Carve `size` bytes from the global bump region (replacing the slab when
    /// it cannot fit). Returns the base; the region is zeroed.
    fn bump(&mut self, size: usize) -> usize {
        if self.cursor + size > self.end {
            self.new_slab(size);
        }
        let base = self.cursor;
        self.cursor += size;
        base
    }

    /// Replace the bump region with a new slab large enough for `size` bytes.
    /// Any tail of the old slab is small and dropped (bounded waste per slab).
    fn new_slab(&mut self, size: usize) {
        let slab = SLAB.max(size).next_multiple_of(ALIGN);
        let layout = Layout::from_size_align(slab, ALIGN).expect("valid slab layout");
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            std::process::abort();
        }
        let base = base as usize;
        self.slabs.push((base, slab));
        self.cursor = base;
        self.end = base + slab;
    }
}

// --- per-thread cache -------------------------------------------------------

/// A thread's private allocation buffer: a bump region plus per-class free
/// lists refilled in batches from the global pool.
struct LocalCache {
    /// Bump region carved from the global slabs.
    cursor: usize,
    end: usize,
    /// Per-class free lists (indexed by [`class_index`]); filled only by
    /// refilling from the global free lists, drained by [`alloc`].
    free: Vec<Vec<usize>>,
}

impl LocalCache {
    fn new() -> Self {
        LocalCache {
            cursor: 0,
            end: 0,
            free: (0..NUM_CLASSES).map(|_| Vec::new()).collect(),
        }
    }
}

/// On thread exit, flush this thread's local free blocks back to the *global*
/// pool, so a short-lived thread's recycled blocks remain reusable by other
/// threads rather than being stranded. The unused bump tail is abandoned
/// (bounded waste, ≤ one chunk per thread).
///
/// The flush touches only `global()` (a `'static`), never another thread-local,
/// so it is safe to run during thread-local destruction.
impl Drop for LocalCache {
    fn drop(&mut self) {
        if self.free.iter().all(|l| l.is_empty()) {
            return;
        }
        let mut g = global_lock();
        for (idx, list) in self.free.iter_mut().enumerate() {
            if !list.is_empty() {
                g.free[idx].append(list);
            }
        }
    }
}

thread_local! {
    static CACHE: RefCell<LocalCache> = RefCell::new(LocalCache::new());
}

// --- allocation -------------------------------------------------------------

#[inline]
unsafe fn zero(base: usize, n: usize) {
    unsafe { std::ptr::write_bytes(base as *mut u8, 0, n) };
}

/// Allocate a zeroed, 16-byte-aligned block of at least `total` bytes.
///
/// Returns a pointer that stays valid until the block is handed back to
/// [`free`]. Aborts the process on slab-acquisition failure (out of memory).
pub fn alloc(total: usize) -> *mut u8 {
    let class = size_class(total);
    let Some(idx) = class_index(class) else {
        // Large object: straight to the global exact-size list / bump, no cache.
        return alloc_large(class);
    };
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        loop {
            // 1. Thread-local recycled block (re-zero — it carried stale data).
            if let Some(base) = c.free[idx].pop() {
                unsafe { zero(base, class) };
                return base as *mut u8;
            }
            // 2. Thread-local bump region (fresh slab memory — already zero).
            if c.cursor + class <= c.end {
                let base = c.cursor;
                c.cursor += class;
                return base as *mut u8;
            }
            // 3. Miss: take the global lock once and refill, then retry.
            refill(&mut c, class, idx);
        }
    })
}

/// Refill the thread-local cache for `class`/`idx` under the global lock: prefer
/// moving a batch of recycled blocks into the local free list; otherwise carve a
/// fresh bump chunk. After this returns, step 1 or 2 of [`alloc`] succeeds.
fn refill(c: &mut LocalCache, class: usize, idx: usize) {
    let mut g = global_lock();
    let gfree = &mut g.free[idx];
    if !gfree.is_empty() {
        // Move up to a batch of recycled blocks into the local list.
        let take = gfree.len().min(REFILL_BATCH);
        let start = gfree.len() - take;
        c.free[idx].extend(gfree.drain(start..));
        return;
    }
    // No recycled blocks of this class: carve a fresh bump chunk. Size it to hold
    // at least the requested class (so the retry's bump step always fits).
    let chunk = TLAB_CHUNK.max(class);
    let base = g.bump(chunk);
    // Abandon any tiny leftover of the previous local chunk (bounded waste) and
    // adopt the new one.
    c.cursor = base;
    c.end = base + chunk;
}

/// Allocate a large object (`> LARGE_MAX`): an exact-size global free list,
/// bypassing the per-thread cache. Re-zeroes a recycled block; fresh bump memory
/// is already zero.
fn alloc_large(class: usize) -> *mut u8 {
    let mut g = global_lock();
    if let Some(list) = g.large_free.get_mut(&class) {
        if let Some(base) = list.pop() {
            drop(g);
            unsafe { zero(base, class) };
            return base as *mut u8;
        }
    }
    let base = g.bump(class);
    base as *mut u8
}

/// Return a block previously obtained from [`alloc`] with the same `total`.
///
/// Called only by the collector (sweep / finalizer / `@RefCounted` release /
/// teardown) — never by a mutator's allocation path. The block is recycled into
/// the **global** free lists (never the freeing thread's local cache, so it stays
/// reusable by every thread) and never returned to the system, so a
/// stop-the-world sweep calling this never touches the system-malloc lock.
pub fn free(base: usize, total: usize) {
    let class = size_class(total);
    let mut g = global_lock();
    match class_index(class) {
        Some(idx) => g.free[idx].push(base),
        None => g.large_free.entry(class).or_default().push(base),
    }
}

// --- observability (tests only) --------------------------------------------

/// Bytes currently held in the global free lists (recyclable without touching
/// the system). Excludes blocks parked in threads' local caches.
/// Observability/testing only.
pub fn free_list_bytes() -> usize {
    let g = global_lock();
    let small: usize = g
        .free
        .iter()
        .enumerate()
        .map(|(idx, list)| class_size_of_index(idx) * list.len())
        .sum();
    let large: usize = g
        .large_free
        .iter()
        .map(|(class, list)| class * list.len())
        .sum();
    small + large
}

/// Inverse of [`class_index`] for the small/medium classes (observability only).
fn class_size_of_index(idx: usize) -> usize {
    if idx < 32 {
        (idx + 1) * 16
    } else {
        1usize << (idx - 32 + 10)
    }
}

/// Total bytes acquired from the system across all slabs (high-water mark of
/// managed-heap reservation). Observability/testing only.
pub fn reserved_bytes() -> usize {
    let g = global_lock();
    g.slabs.iter().map(|&(_, s)| s).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that depend on the *global* free lists / slab reservation
    /// (the allocator is process-global and cargo runs tests on parallel threads,
    /// so a concurrent allocation of the same size class would perturb them). The
    /// pure / distinct-pointer tests do not need it.
    static GLOBAL_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn size_classes_round_predictably() {
        assert_eq!(size_class(1), 16);
        assert_eq!(size_class(16), 16);
        assert_eq!(size_class(17), 32);
        assert_eq!(size_class(64), 64);
        assert_eq!(size_class(65), 80);
        assert_eq!(size_class(512), 512);
        assert_eq!(size_class(513), 1024); // power of two above 512
        assert_eq!(size_class(1000), 1024);
        // Large objects: exact, alignment-rounded.
        assert_eq!(size_class(LARGE_MAX + 1), (LARGE_MAX + 1 + 15) & !15);
    }

    #[test]
    fn class_index_round_trips_for_cached_classes() {
        // Every small/medium class maps to a distinct in-range index, and the
        // observability inverse recovers the class size.
        let mut seen = std::collections::HashSet::new();
        for total in (16..=512).step_by(16) {
            let class = size_class(total);
            let idx = class_index(class).unwrap();
            assert!(idx < NUM_CLASSES);
            assert!(seen.insert(idx), "indices must be distinct");
            assert_eq!(class_size_of_index(idx), class);
        }
        for p in 10..=20 {
            let class = 1usize << p;
            let idx = class_index(class).unwrap();
            assert!(idx < NUM_CLASSES);
            assert!(seen.insert(idx));
            assert_eq!(class_size_of_index(idx), class);
        }
        // Large objects have no cache slot.
        assert_eq!(class_index(size_class(LARGE_MAX + 1)), None);
        assert_eq!(seen.len(), NUM_CLASSES);
    }

    #[test]
    fn alloc_returns_distinct_zeroed_aligned_blocks() {
        let a = alloc(64);
        let b = alloc(64);
        assert_ne!(a, b, "distinct allocations must not alias");
        assert_eq!(a as usize % ALIGN, 0, "16-byte aligned");
        assert_eq!(b as usize % ALIGN, 0);
        // Zeroed.
        for i in 0..64 {
            assert_eq!(unsafe { *a.add(i) }, 0);
        }
        // Writable without clobbering the neighbor.
        unsafe { std::ptr::write_bytes(a, 0xAB, 64) };
        for i in 0..64 {
            assert_eq!(unsafe { *b.add(i) }, 0, "neighbor untouched");
        }
        free(a as usize, 64);
        free(b as usize, 64);
    }

    #[test]
    fn global_allocator_lock_recovers_after_poison() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        let poisoner = std::thread::spawn(|| {
            let _guard = global().lock().unwrap();
            panic!("poison global allocator");
        });
        assert!(poisoner.join().is_err());
        assert!(
            global().lock().is_err(),
            "test setup must leave the global allocator lock poisoned"
        );

        let p = alloc(128);
        assert_eq!(p as usize % ALIGN, 0);
        for i in 0..128 {
            assert_eq!(unsafe { *p.add(i) }, 0);
        }
        free(p as usize, 128);
        assert!(
            free_list_bytes() >= size_class(128),
            "allocator observability should also recover after poison"
        );
        assert!(
            reserved_bytes() >= SLAB,
            "reserved-byte accounting should remain readable after poison"
        );
    }

    #[test]
    fn freed_blocks_are_recycled_and_rezeroed() {
        // A freed block goes to the *global* pool; a later allocation that has
        // exhausted its local bump refills from the global pool and hands the
        // block back, re-zeroed. (With TLABs the reuse is not the *immediately*
        // next alloc — the thread must drain its bump region first.)
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        // Use an unusual class (496 B → idx 30) no other test exercises, so the
        // global free list for it holds only our blocks.
        let sz = 496usize;
        let class = size_class(sz);
        let chunk_cap = TLAB_CHUNK / class; // blocks one bump chunk yields
        let m = chunk_cap + REFILL_BATCH + 16;
        // Wave 1: allocate `m` distinct blocks, dirty them, free them all.
        let wave1: Vec<usize> = (0..m).map(|_| alloc(sz) as usize).collect();
        for &p in &wave1 {
            unsafe { std::ptr::write_bytes(p as *mut u8, 0xFF, class) };
        }
        let freed: std::collections::HashSet<usize> = wave1.iter().copied().collect();
        for &p in &wave1 {
            free(p, sz);
        }
        // Wave 2: after the leftover bump tail (< chunk_cap blocks) is consumed,
        // the rest must come back from the global free list — recycled, re-zeroed.
        let mut recycled = 0usize;
        let wave2: Vec<usize> = (0..m).map(|_| alloc(sz) as usize).collect();
        for &q in &wave2 {
            if freed.contains(&q) {
                recycled += 1;
                for i in 0..class {
                    assert_eq!(
                        unsafe { *(q as *const u8).add(i) },
                        0,
                        "recycled must be re-zeroed"
                    );
                }
            }
        }
        assert!(
            recycled >= REFILL_BATCH,
            "freed blocks must be recycled via the global pool (got {recycled})"
        );
        for q in wave2 {
            free(q, sz);
        }
    }

    #[test]
    fn large_objects_alloc_free_and_recycle() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        let big = LARGE_MAX + 4096;
        let p = alloc(big);
        assert_eq!(p as usize % ALIGN, 0);
        unsafe { std::ptr::write_bytes(p, 0x7, big) };
        free(p as usize, big);
        let q = alloc(big);
        assert_eq!(
            p, q,
            "large blocks recycle by exact size (no per-thread cache)"
        );
        for i in (0..big).step_by(4096) {
            assert_eq!(unsafe { *q.add(i) }, 0);
        }
        free(q as usize, big);
    }

    #[test]
    fn many_allocations_span_multiple_slabs() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        // Allocate well past one slab's worth to force slab growth.
        let before = reserved_bytes();
        let mut ptrs = Vec::new();
        for _ in 0..(SLAB / 256 + 100) {
            ptrs.push(alloc(256) as usize);
        }
        assert!(reserved_bytes() > before, "should have reserved more slabs");
        // All distinct.
        let mut sorted = ptrs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ptrs.len(), "no two live blocks alias");
        for p in ptrs {
            free(p, 256);
        }
    }

    #[test]
    fn concurrent_allocation_yields_distinct_blocks() {
        // Several threads allocate concurrently through their own TLABs; every
        // returned block across all threads must be distinct (no two threads ever
        // hand out the same address) and correctly aligned.
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        let n_threads = 8;
        let per = 2000usize;
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            handles.push(std::thread::spawn(move || {
                let mut v = Vec::with_capacity(per);
                for i in 0..per {
                    let sz = 16 + (i % 480); // spread across small classes
                    let p = alloc(sz) as usize;
                    assert_eq!(p % ALIGN, 0);
                    v.push((p, size_class(sz)));
                }
                v
            }));
        }
        let mut all: Vec<(usize, usize)> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        let mut addrs: Vec<usize> = all.iter().map(|&(p, _)| p).collect();
        addrs.sort_unstable();
        let before = addrs.len();
        addrs.dedup();
        assert_eq!(
            addrs.len(),
            before,
            "no two concurrently-live blocks may alias"
        );
        // Free everything back (exercises global free-list return from many threads).
        for (p, sz) in all {
            free(p, sz);
        }
    }
}
