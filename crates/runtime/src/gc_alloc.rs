//! The managed-heap allocator (`docs/16` §9).
//!
//! GC objects are *not* allocated with the system `malloc`/`free`. They come
//! from this slab-backed, size-segregated free-list allocator, for two reasons:
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
//! Design: objects are rounded up to a *size class* (16-byte granularity up to
//! 512 B, then power-of-two up to `LARGE_MAX`). Each class keeps an intrusive
//! free list. A class miss bump-allocates a fresh block from the current slab;
//! a slab miss acquires another (zeroed) slab from the system. Blocks larger
//! than `LARGE_MAX` are served one-to-one from a dedicated free list keyed by
//! their exact rounded size — still never returned to the system, so the sweep
//! stays lock-clean. Memory is retained for the process lifetime (bounded by the
//! peak live set), which is the standard space/safety trade for a tracing GC.
//!
//! All returned blocks are 16-byte aligned and zeroed (the collector relies on a
//! zero mark word and callers rely on zero-initialized fields).

use std::alloc::{alloc_zeroed, Layout};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Default slab size carved into blocks: 1 MiB.
const SLAB: usize = 1024 * 1024;
/// Alignment of every block (and of each slab base).
const ALIGN: usize = 16;
/// Largest size served from a power-of-two class; bigger requests use the
/// exact-size large-object free list.
const LARGE_MAX: usize = 1 << 20; // 1 MiB

struct Allocator {
    /// `class size → stack of free block base addresses` (small/medium classes
    /// and the exact-size large-object lists share this map).
    free: HashMap<usize, Vec<usize>>,
    /// Retained slabs `(base, size)` — kept for teardown/reset only.
    slabs: Vec<(usize, usize)>,
    /// Bump region within the current slab.
    cursor: usize,
    end: usize,
}

/// The process-global managed-heap allocator (lazily initialized — `HashMap`
/// has no const constructor).
fn allocator() -> &'static Mutex<Allocator> {
    static A: OnceLock<Mutex<Allocator>> = OnceLock::new();
    A.get_or_init(|| {
        Mutex::new(Allocator { free: HashMap::new(), slabs: Vec::new(), cursor: 0, end: 0 })
    })
}

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

/// Allocate a zeroed, 16-byte-aligned block of at least `total` bytes.
///
/// Returns a pointer that stays valid until the block is handed back to
/// [`free`]. Aborts the process on slab-acquisition failure (out of memory),
/// matching the previous `alloc_zeroed` behavior.
pub fn alloc(total: usize) -> *mut u8 {
    let class = size_class(total);
    let mut a = allocator().lock().unwrap();

    // 1. Reuse a same-class freed block (must be re-zeroed).
    if let Some(list) = a.free.get_mut(&class) {
        if let Some(base) = list.pop() {
            drop(a);
            unsafe { std::ptr::write_bytes(base as *mut u8, 0, class) };
            return base as *mut u8;
        }
    }

    // 2. Bump a fresh block from the current slab (already zeroed; no re-zero).
    if a.cursor + class > a.end {
        a.new_slab(class);
    }
    let base = a.cursor;
    a.cursor += class;
    base as *mut u8
}

/// Return a block previously obtained from [`alloc`] with the same `total`.
///
/// The block is recycled into its size class — never returned to the system —
/// so a stop-the-world sweep calling this never touches the system-malloc lock.
pub fn free(base: usize, total: usize) {
    let class = size_class(total);
    let mut a = allocator().lock().unwrap();
    a.free.entry(class).or_default().push(base);
}

impl Allocator {
    /// Replace the bump region with a new slab large enough for `class` bytes.
    /// Any tail of the old slab is small (< one class) and is dropped — its
    /// waste is bounded by one class size per slab.
    fn new_slab(&mut self, class: usize) {
        let size = SLAB.max(class).next_multiple_of(ALIGN);
        let layout = Layout::from_size_align(size, ALIGN).expect("valid slab layout");
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            std::process::abort();
        }
        let base = base as usize;
        self.slabs.push((base, size));
        self.cursor = base;
        self.end = base + size;
    }
}

/// Bytes currently held in free lists (recyclable without touching the system).
/// Observability/testing only.
pub fn free_list_bytes() -> usize {
    let a = allocator().lock().unwrap();
    a.free.iter().map(|(class, list)| class * list.len()).sum()
}

/// Total bytes acquired from the system across all slabs (high-water mark of
/// managed-heap reservation). Observability/testing only.
pub fn reserved_bytes() -> usize {
    let a = allocator().lock().unwrap();
    a.slabs.iter().map(|&(_, s)| s).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn freed_blocks_are_reused_and_rezeroed() {
        let p = alloc(48);
        unsafe { std::ptr::write_bytes(p, 0xFF, 48) };
        free(p as usize, 48);
        // The very next same-class allocation should hand the block back, zeroed.
        let q = alloc(48);
        assert_eq!(p, q, "same-class free block should be recycled");
        for i in 0..48 {
            assert_eq!(unsafe { *q.add(i) }, 0, "recycled block must be re-zeroed");
        }
        free(q as usize, 48);
    }

    #[test]
    fn large_objects_alloc_free_and_recycle() {
        let big = LARGE_MAX + 4096;
        let p = alloc(big);
        assert_eq!(p as usize % ALIGN, 0);
        unsafe { std::ptr::write_bytes(p, 0x7, big) };
        free(p as usize, big);
        let q = alloc(big);
        assert_eq!(p, q, "large blocks recycle by exact size");
        for i in (0..big).step_by(4096) {
            assert_eq!(unsafe { *q.add(i) }, 0);
        }
        free(q as usize, big);
    }

    #[test]
    fn many_allocations_span_multiple_slabs() {
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
}
