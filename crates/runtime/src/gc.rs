//! The managed heap and (eventually) the tracing collector.
//!
//! Every managed object carries a two-word header immediately before its field
//! block (`docs/16` §3):
//!
//! ```text
//!   base ──▶ [ desc: *const u8 ][ mark: u64 ][ field 0 ][ field 1 ] …
//!            └────────── header (16 B) ──────┘└──── field block ────┘
//!   field-block pointer (what managed code holds) = base + 16
//! ```
//!
//! The `desc` word points to a **type descriptor** — an inline blob the code
//! generator emits once per managed type. Its layout (little-endian):
//!
//! ```text
//!   [ size: u64 ][ kind: u64 ][ type_id: u64 ][ n_ptrs: u64 ][ off_0: u32 ] … [ off_{n-1}: u32 ]
//!   [ n_rc: u32 ][ rcoff_0: u32 ] … [ rcoff_{m-1}: u32 ]            ← optional trailer
//! ```
//!
//! * `size`    — field-block size in bytes.
//! * `kind`    — 0 = plain, 1 = `str`, 2 = `List`, 3 = `Map`, 4 = `@RefCounted`
//!   (scan handled specially by the collector; see [`KIND_REFCOUNTED`]).
//! * `type_id` — `0`, or the type's id used to find a registered `Drop`/finalizer.
//! * `n_ptrs` / `off_i` — byte offsets, within the field block, of fields that
//!   hold managed pointers (the GC trace map, for `kind == 0`/`4`).
//! * `n_rc` / `rcoff_j` — *trailer* listing the offsets of the fields that hold
//!   `@RefCounted` pointers (always a subset of `off_i`). When such an object is
//!   destroyed — by `lang_rc_release` reaching zero, or swept by the GC — these
//!   stored strong references are released so the count cascade reaches the whole
//!   owned graph. The GC mark reader stops after the `off_i` block, so the
//!   trailer is invisible to it (older descriptors simply omit it).
//!
//! This module currently owns allocation and a registry of live objects; the
//! mark-sweep collector and precise-root scan build on it (see `ROADMAP.md`).

// Managed objects are allocated from `gc_alloc` (the GC's own slab allocator),
// not the system allocator — see that module for the rationale.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Size of the object header in bytes: `[desc ptr][mark word]`.
pub const HEADER: usize = 16;

/// Descriptor `kind` values.
pub const KIND_PLAIN: u64 = 0;
pub const KIND_STR: u64 = 1;
pub const KIND_LIST: u64 = 2;
pub const KIND_MAP: u64 = 3;
/// A `@RefCounted` object (`docs/16` §8.1): laid out exactly like a plain object
/// for GC tracing, but carrying a hidden **atomic strong-count** word at
/// field-block offset 0 (user fields shift up by 8). The compiler emits
/// retain/release (`lang_rc_retain`/`lang_rc_release`) around every binding; the
/// count reaching zero runs the type's `Drop` synchronously and frees the object
/// without waiting for a collection. The tracing GC is retained only as the
/// **cycle-collector backstop** (refcounting alone leaks reference cycles).
pub const KIND_REFCOUNTED: u64 = 4;

#[inline]
unsafe fn read_u64(p: *const u8, byte_off: usize) -> u64 {
    unsafe { (p.add(byte_off) as *const u64).read_unaligned() }
}

/// Field-block size recorded in a descriptor blob.
///
/// # Safety
/// `desc` must point to a valid descriptor blob.
#[inline]
pub unsafe fn desc_size(desc: *const u8) -> usize {
    unsafe { read_u64(desc, 0) as usize }
}

/// The `kind` word of a descriptor blob.
///
/// # Safety
/// `desc` must point to a valid descriptor blob.
#[inline]
pub unsafe fn desc_kind(desc: *const u8) -> u64 {
    unsafe { read_u64(desc, 8) }
}

/// The type id recorded in a descriptor (`0` for builtins and types without a
/// `Drop` impl). Used to find a registered finalizer (`docs/16` §8).
#[inline]
pub unsafe fn desc_type_id(desc: *const u8) -> u64 {
    unsafe { read_u64(desc, 16) }
}

#[inline]
unsafe fn desc_n_ptrs(desc: *const u8) -> usize {
    unsafe { read_u64(desc, 24) as usize }
}

/// The `i`-th pointer-field offset listed in a plain descriptor.
#[inline]
unsafe fn desc_ptr_offset(desc: *const u8, i: usize) -> usize {
    unsafe { (desc.add(32 + i * 4) as *const u32).read_unaligned() as usize }
}

/// The byte offset where the optional `@RefCounted`-child trailer begins (right
/// after the `n_ptrs` pointer-offset entries).
#[inline]
unsafe fn desc_rc_trailer_off(desc: *const u8) -> usize {
    32 + unsafe { desc_n_ptrs(desc) } * 4
}

/// The offsets of the fields holding `@RefCounted` strong references, read from
/// the descriptor trailer. Empty for the vast majority of types (`n_rc == 0`).
///
/// Every descriptor — both the compiler-emitted blobs and the static builtin
/// [`StaticDesc`]s — carries an `n_rc` word at `32 + n_ptrs*4`, so this read is
/// always in-bounds. The builtins set it to `0` (their `rc_trailer` field).
#[inline]
unsafe fn desc_rc_offsets(desc: *const u8) -> Vec<usize> {
    let base = unsafe { desc_rc_trailer_off(desc) };
    let n = unsafe { (desc.add(base) as *const u32).read_unaligned() as usize };
    (0..n)
        .map(|j| unsafe { (desc.add(base + 4 + j * 4) as *const u32).read_unaligned() as usize })
        .collect()
}

/// The registry of *globally-tracked* managed objects (base address + total
/// byte size, including the header).
///
/// This holds: collection survivors, every `@RefCounted` object (which needs
/// global findability — see [`alloc_raw`]), and — only transiently, while a
/// collection runs — the contents drained from every mutator's private alloc
/// log. Most freshly-allocated objects live in their allocating thread's
/// [`Mutator::alloc_log`] and are merged in here at the next stop-the-world
/// collection (see [`drain_alloc_logs_into`]); that is what keeps the global
/// lock off the allocation hot path (`ROADMAP.md` → "Per-thread TLABs").
struct Heap {
    /// Globally-tracked managed objects: base address → total byte size
    /// (including the header). A map (not a vector) so `lang_rc_release` can
    /// remove a deterministically freed object in O(1) without a linear scan.
    objects: HashMap<usize, usize>,
}

/// The global object registry. Behind a `OnceLock` because `HashMap::new` is not
/// `const`; initialized empty on first allocation.
fn heap() -> &'static Mutex<Heap> {
    static H: OnceLock<Mutex<Heap>> = OnceLock::new();
    H.get_or_init(|| {
        Mutex::new(Heap {
            objects: HashMap::new(),
        })
    })
}

/// Bytes allocated since the last collection — the GC trigger. A lock-free
/// global counter bumped by every allocation (on the hot path, instead of a
/// field updated under the heap lock) and reset to `0` by each collection.
static BYTES_SINCE_GC: AtomicUsize = AtomicUsize::new(0);

/// A statically-allocated descriptor (its blob layout matches what the
/// collector reads: `[size][kind][type_id][n_ptrs]`). Builtin descriptors carry
/// `type_id == 0` (no `Drop`).
#[repr(C, align(8))]
pub struct StaticDesc {
    pub size: u64,
    pub kind: u64,
    pub type_id: u64,
    pub n_ptrs: u64,
    /// The `n_rc` trailer word (always `0` for builtins — they own no
    /// `@RefCounted` fields directly). Sits at byte offset `32` = `32 + 0*4`
    /// since every builtin has `n_ptrs == 0`, so [`desc_rc_offsets`] reads it
    /// in-bounds and returns an empty list. See the module-level descriptor doc.
    pub rc_trailer: u64,
}

/// Shared descriptor for `str` objects (variable size; bytes inline, leaf).
pub static STR_DESC: StaticDesc = StaticDesc {
    size: 0,
    kind: KIND_STR,
    type_id: 0,
    n_ptrs: 0,
    rc_trailer: 0,
};
/// Shared descriptor for a `List` handle: `[len][cap][buf][elem_is_ptr]`.
/// The collector special-cases `kind == LIST` to trace the buffer's elements.
pub static LIST_HANDLE_DESC: StaticDesc = StaticDesc {
    size: 32,
    kind: KIND_LIST,
    type_id: 0,
    n_ptrs: 0,
    rc_trailer: 0,
};
/// Shared descriptor for a `List` element buffer (variable size, leaf — it is
/// traced via its owning `List` handle, which knows the length/elem-kind).
pub static LIST_BUF_DESC: StaticDesc = StaticDesc {
    size: 0,
    kind: KIND_PLAIN,
    type_id: 0,
    n_ptrs: 0,
    rc_trailer: 0,
};
/// Shared descriptor for a `Map` handle:
/// `[len][cap][buf][key_is_ptr][val_is_ptr][hash_fn][eq_fn]` (56 B). The
/// `hash_fn`/`eq_fn` slots are nullable function pointers; when non-null, the
/// runtime calls through them (used for user-typed keys implementing
/// `Eq + Hash`, `docs/15` §7). The collector special-cases `kind == MAP` to
/// trace each occupied slot's key/value as needed.
pub static MAP_HANDLE_DESC: StaticDesc = StaticDesc {
    size: 56,
    kind: KIND_MAP,
    type_id: 0,
    n_ptrs: 0,
    rc_trailer: 0,
};
/// Shared descriptor for a `Map` slot buffer (variable size, leaf — traced via
/// its owning handle, which knows the capacity and key/value pointer-ness).
pub static MAP_BUF_DESC: StaticDesc = StaticDesc {
    size: 0,
    kind: KIND_PLAIN,
    type_id: 0,
    n_ptrs: 0,
    rc_trailer: 0,
};

#[inline]
pub fn str_desc() -> *const u8 {
    &STR_DESC as *const StaticDesc as *const u8
}
#[inline]
pub fn list_handle_desc() -> *const u8 {
    &LIST_HANDLE_DESC as *const StaticDesc as *const u8
}
#[inline]
pub fn list_buf_desc() -> *const u8 {
    &LIST_BUF_DESC as *const StaticDesc as *const u8
}
#[inline]
pub fn map_handle_desc() -> *const u8 {
    &MAP_HANDLE_DESC as *const StaticDesc as *const u8
}
#[inline]
pub fn map_buf_desc() -> *const u8 {
    &MAP_BUF_DESC as *const StaticDesc as *const u8
}

unsafe fn alloc_raw(desc: *const u8, size: usize) -> *mut u8 {
    let total = HEADER + size;
    // Managed objects come from the GC's own slab allocator, never the system
    // allocator — so a stop-the-world sweep (which frees back into it) can never
    // contend with a mutator parked inside `malloc` (`gc_alloc`).
    let base = crate::gc_alloc::alloc(total);
    // header.desc at +0; header.mark at +8 (already zero).
    unsafe { (base as *mut *const u8).write(desc) };

    // Record the object so the collector can find it later. Two registries:
    //
    //  * `@RefCounted` objects go straight into the **global** registry. They are
    //    freed *deterministically* by `lang_rc_release` from any thread at any
    //    time (not only at a stop-the-world collection), so they must be globally
    //    findable for O(1) removal.
    //  * Every other object is reclaimed only by the collector, and only at a
    //    stop-the-world point where every mutator's log is drained into the
    //    global registry first. So it is recorded in **this thread's private
    //    alloc log** — no global lock on the allocation hot path. The log mutex is
    //    this thread's own and is read by the collector only while this thread is
    //    stopped, so it is effectively uncontended.
    if unsafe { desc_kind(desc) } == KIND_REFCOUNTED {
        heap().lock().unwrap().objects.insert(base as usize, total);
    } else {
        ME.with(|h| h.0.alloc_log.lock().unwrap().push((base as usize, total)));
    }
    BYTES_SINCE_GC.fetch_add(total, Ordering::Relaxed);

    unsafe { base.add(HEADER) }
}

/// Allocate a fixed-size object whose size comes from its descriptor.
///
/// # Safety
/// `desc` must point to a valid, sufficiently long-lived descriptor blob.
pub unsafe fn alloc(desc: *const u8) -> *mut u8 {
    maybe_collect();
    let size = unsafe { desc_size(desc) };
    unsafe { alloc_raw(desc, size) }
}

/// Allocate a variable-size object (`str` bytes, list buffers) of `size` bytes.
///
/// # Safety
/// `desc` must point to a valid, sufficiently long-lived descriptor blob.
pub unsafe fn alloc_var(desc: *const u8, size: usize) -> *mut u8 {
    maybe_collect();
    unsafe { alloc_raw(desc, size) }
}

/// Mark-sweep collection from an explicit, precise root set (field-block
/// pointers). Anything not transitively reachable from `roots` is freed.
///
/// Returns the number of bytes reclaimed. Marking follows each object's
/// descriptor trace map; the mark bit lives in the object's header word.
///
/// # Safety
/// `roots` must contain every live managed pointer reachable by the program
/// (the caller — the stack-map root scan — guarantees this). Stray non-pointer
/// values are tolerated: only addresses of registered objects are followed.
pub unsafe fn collect(roots: &[usize]) -> usize {
    let mut heap = heap().lock().unwrap();

    // Merge every mutator's private alloc log into the global registry first, so
    // `heap.objects` enumerates *every* live object (the precise-root scan and the
    // sweep both rely on this completeness). Safe because the world is stopped.
    drain_alloc_logs_into(&mut heap.objects);

    // Objects awaiting finalization are kept alive (and their referents kept
    // alive) until their `drop` runs — include them so marking traverses them.
    let pending: Vec<(usize, usize, u64)> = FINALIZE_PENDING.lock().unwrap().clone();
    let mut bases: HashSet<usize> = heap.objects.keys().copied().collect();
    for &(b, _, _) in &pending {
        bases.insert(b);
    }

    // --- mark -------------------------------------------------------------
    // Stack roots, globally-pinned roots (`EXTRA_ROOTS`), and the graphs of
    // objects still pending finalization.
    let is_obj = |fb: usize| fb != 0 && fb >= HEADER && bases.contains(&(fb - HEADER));
    let mut work: Vec<usize> = roots.iter().copied().filter(|&p| is_obj(p)).collect();
    work.extend(
        extra_roots()
            .lock()
            .unwrap()
            .keys()
            .copied()
            .filter(|&p| is_obj(p)),
    );
    for &(b, _, _) in &pending {
        work.push(b + HEADER);
    }
    unsafe { mark_reachable(&bases, work) };

    // --- finalization: resurrect newly-unreachable objects with a `Drop` ---
    // (`docs/16` §8). They stay alive one more cycle (their graph re-marked) so
    // `drop(self)` can run after the collection, then they are freed.
    let mut newly: Vec<(usize, usize, u64)> = Vec::new();
    if any_finalizers() {
        let dfs = drop_fns().lock().unwrap();
        for (&base, &total) in &heap.objects {
            if unsafe { *((base + 8) as *const u64) } == 0 {
                let desc = unsafe { (base as *const *const u8).read() };
                let tid = unsafe { desc_type_id(desc) };
                if tid != 0 && dfs.contains_key(&tid) {
                    newly.push((base, total, tid));
                }
            }
        }
        drop(dfs);
        if !newly.is_empty() {
            let work2: Vec<usize> = newly.iter().map(|&(b, _, _)| b + HEADER).collect();
            unsafe { mark_reachable(&bases, work2) };
        }
    }
    let newly_set: HashSet<usize> = newly.iter().map(|&(b, _, _)| b).collect();

    // --- sweep ------------------------------------------------------------
    // Classify first; we need the full set of dying bases before adjusting any
    // `@RefCounted` counts, so an edge *into* the dying set is never decremented
    // (that object is reclaimed regardless), while an edge into a survivor is.
    let objects = std::mem::take(&mut heap.objects);
    let mut survivors: HashMap<usize, usize> = HashMap::new();
    let mut new_pending: Vec<(usize, usize, u64)> = Vec::new();
    let mut to_free: Vec<(usize, usize)> = Vec::new();
    for (base, total) in objects {
        let mark = (base + 8) as *mut u64;
        if newly_set.contains(&base) {
            // Unreachable but finalizable: hand off to the finalizer queue. Its
            // owned `@RefCounted` edges are released when `run_finalizers` frees it.
            unsafe { *mark = 0 };
            let tid = newly.iter().find(|&&(b, _, _)| b == base).unwrap().2;
            new_pending.push((base, total, tid));
        } else if unsafe { *mark } != 0 {
            unsafe { *mark = 0 }; // clear for next cycle
            survivors.insert(base, total);
        } else {
            to_free.push((base, total));
        }
    }
    // An object that owned `@RefCounted` strong references and is now being
    // reclaimed loses those edges; decrement the strong count of any *surviving*
    // referent so the refcount stays exact (CPython-style cyclic adjustment).
    // Never frees here — survivors are reachable, so a correct count stays ≥ 1.
    let dying: HashSet<usize> = to_free.iter().map(|&(b, _)| b).collect();
    for &(base, _) in &to_free {
        let desc = unsafe { (base as *const *const u8).read() };
        for off in unsafe { desc_rc_offsets(desc) } {
            let child = unsafe { ((base + HEADER + off) as *const usize).read() };
            if child >= HEADER && !dying.contains(&(child - HEADER)) {
                unsafe { rc_dec_no_free(child) };
            }
        }
    }
    let mut freed = 0usize;
    for (base, total) in &to_free {
        crate::gc_alloc::free(*base, *total);
        freed += *total;
    }
    let kept = survivors.len();
    heap.objects = survivors;
    BYTES_SINCE_GC.store(0, Ordering::Relaxed);
    drop(heap);
    if !new_pending.is_empty() {
        FINALIZE_PENDING.lock().unwrap().extend(new_pending);
    }
    if gc_debug() {
        eprintln!("[gc] collected: freed {freed} bytes, {kept} object(s) kept");
    }
    freed
}

/// Mark-traverse every object reachable from `work` (field-block pointers),
/// following each object's descriptor trace map. `bases` is the set of valid
/// object base addresses (heap + pending-finalize).
unsafe fn mark_reachable(bases: &HashSet<usize>, mut work: Vec<usize>) {
    let is_obj = |fb: usize| fb != 0 && fb >= HEADER && bases.contains(&(fb - HEADER));
    while let Some(fb) = work.pop() {
        let base = fb - HEADER;
        let mark = (base + 8) as *mut u64;
        if unsafe { *mark } != 0 {
            continue; // already marked
        }
        unsafe { *mark = 1 };

        let desc = unsafe { (base as *const *const u8).read() };
        match unsafe { desc_kind(desc) } {
            // A `@RefCounted` object traces exactly like a plain object — its
            // hidden strong-count word at offset 0 is not a pointer and is never
            // listed in the trace map; only the user pointer fields are scanned.
            KIND_PLAIN | KIND_REFCOUNTED => {
                let n = unsafe { desc_n_ptrs(desc) };
                for i in 0..n {
                    let off = unsafe { desc_ptr_offset(desc, i) };
                    let child = unsafe { ((fb + off) as *const usize).read() };
                    if is_obj(child) {
                        work.push(child);
                    }
                }
            }
            KIND_LIST => {
                // handle layout: [len][cap][buf][elem_is_ptr]
                let len = unsafe { (fb as *const u64).read() } as usize;
                let buf = unsafe { ((fb + 16) as *const usize).read() };
                let elem_is_ptr = unsafe { ((fb + 24) as *const u64).read() } != 0;
                if is_obj(buf) {
                    work.push(buf);
                }
                if elem_is_ptr && buf != 0 {
                    for i in 0..len {
                        let child = unsafe { ((buf + i * 8) as *const usize).read() };
                        if is_obj(child) {
                            work.push(child);
                        }
                    }
                }
            }
            KIND_MAP => {
                // handle layout: [len][cap][buf][key_is_ptr][val_is_ptr]
                let cap = unsafe { ((fb + 8) as *const u64).read() } as usize;
                let buf = unsafe { ((fb + 16) as *const usize).read() };
                let key_is_ptr = unsafe { ((fb + 24) as *const u64).read() } != 0;
                let val_is_ptr = unsafe { ((fb + 32) as *const u64).read() } != 0;
                if is_obj(buf) {
                    work.push(buf);
                }
                if buf != 0 && (key_is_ptr || val_is_ptr) {
                    // slot = [state: u64][key: u64][val: u64] (24 B)
                    for i in 0..cap {
                        let slot = buf + i * 24;
                        let state = unsafe { (slot as *const u64).read() };
                        if state != 1 {
                            continue; // 0 = empty, 2 = tombstone
                        }
                        if key_is_ptr {
                            let k = unsafe { ((slot + 8) as *const usize).read() };
                            if is_obj(k) {
                                work.push(k);
                            }
                        }
                        if val_is_ptr {
                            let v = unsafe { ((slot + 16) as *const usize).read() };
                            if is_obj(v) {
                                work.push(v);
                            }
                        }
                    }
                }
            }
            // KIND_STR and any leaf: no outgoing pointers.
            _ => {}
        }
    }
}

/// Run pending finalizers (`drop(self)`), then free their objects. Called after
/// a collection with the world resumed, so `drop` bodies run as ordinary code.
/// The collector still holds the GC turn, so no nested collection runs here.
fn run_finalizers() {
    loop {
        let item = FINALIZE_PENDING.lock().unwrap().pop();
        let Some((base, total, tid)) = item else {
            break;
        };
        let f = drop_fns().lock().unwrap().get(&tid).copied();
        if let Some(f) = f {
            f((base + HEADER) as *mut u8); // user `drop(self)`
        }
        // Release any `@RefCounted` strong references this object owned, to
        // referents that are still live (a survivor, or another not-yet-run
        // finalizable). Counts stay exact even when a GC reclaims an object that
        // held the only counted reference to a surviving refcounted value.
        let desc = unsafe { (base as *const *const u8).read() };
        for off in unsafe { desc_rc_offsets(desc) } {
            let child = unsafe { ((base + HEADER + off) as *const usize).read() };
            if child >= HEADER {
                let live = {
                    heap()
                        .lock()
                        .unwrap()
                        .objects
                        .contains_key(&(child - HEADER))
                };
                if live {
                    unsafe { rc_dec_no_free(child) };
                }
            }
        }
        crate::gc_alloc::free(base, total);
    }
}

/// Number of live objects (for tests / introspection): the globally-tracked
/// objects plus those still sitting in every mutator's not-yet-drained alloc
/// log (which are equally live — they just haven't been merged into the global
/// registry by a collection yet).
pub fn live_count() -> usize {
    let global = heap().lock().unwrap().objects.len();
    let logged: usize = MUTATORS
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.alloc_log.lock().unwrap().len())
        .sum();
    global + logged
}

// --- precise roots via Cranelift stack maps --------------------------------
//
// The code generator registers, for each call safepoint, the function's
// FP→bottom-of-frame offset and the SP-relative byte offsets of the live GC
// references recorded by Cranelift. At collection we walk the frame-pointer
// chain, match each return address to its safepoint, and read those slots.

/// A safepoint's `frame_to_fp` offset and its SP-relative live-ref offsets.
type SafepointInfo = (u32, Vec<u32>);

/// `return-address pc → safepoint info`.
fn safepoints() -> &'static Mutex<HashMap<usize, SafepointInfo>> {
    static SP: OnceLock<Mutex<HashMap<usize, SafepointInfo>>> = OnceLock::new();
    SP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a call safepoint's stack map. Called by the backend after linking,
/// with `pc` = absolute address of the call instruction.
///
/// # Safety
/// `offsets` must point to `n` valid `u32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_gc_register_safepoint(
    pc: usize,
    frame_to_fp: u32,
    offsets: *const u32,
    n: usize,
) {
    let offs = unsafe { std::slice::from_raw_parts(offsets, n) }.to_vec();
    if gc_debug() {
        eprintln!("[gc] register safepoint pc={pc:#x} frame_to_fp={frame_to_fp} offs={offs:?}");
    }
    safepoints().lock().unwrap().insert(pc, (frame_to_fp, offs));
}

fn gc_debug() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("OTTER_FUSION_GC_DEBUG").is_ok())
}

/// Runtime collection toggle. Off by default so the in-process, multi-threaded
/// unit-test harness (one shared heap, single-thread root scan) never collects;
/// `lang run` turns it on for real single-threaded programs. (Concurrent
/// collection lands with threads — see `ROADMAP.md`.)
static GC_ON: AtomicBool = AtomicBool::new(false);

/// Enable or disable collection.
#[unsafe(no_mangle)]
pub extern "C" fn lang_gc_set_enabled(on: bool) {
    GC_ON.store(on, Ordering::Relaxed);
}

/// Whether collection runs: the runtime toggle, or the `OTTER_FUSION_GC` env override.
fn gc_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    let env = *E
        .get_or_init(|| matches!(std::env::var("OTTER_FUSION_GC").as_deref(), Ok(v) if v != "off"));
    env || GC_ON.load(Ordering::Relaxed)
}

/// Collection threshold in bytes; `OTTER_FUSION_GC=stress` collects on every alloc.
fn gc_threshold() -> usize {
    static T: OnceLock<usize> = OnceLock::new();
    *T.get_or_init(|| match std::env::var("OTTER_FUSION_GC").as_deref() {
        Ok("stress") => 0,
        _ => 1 << 20, // 1 MiB
    })
}

/// Read the current frame pointer (aarch64: `x29`, x86-64: `rbp`).
#[inline(always)]
fn current_fp() -> usize {
    let fp: usize;
    unsafe {
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack));
        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack));
    }
    fp
}

/// Walk the frame-pointer chain starting at `fp`, collecting precise GC roots
/// from each matched safepoint. `fp` must belong to a frame that is live and
/// will stay live for the duration of the walk (the current thread's own frame,
/// or a stopped thread's recorded frame during stop-the-world).
unsafe fn scan_stack_roots_from(mut fp: usize) -> Vec<usize> {
    let maps = safepoints().lock().unwrap();
    let mut roots = Vec::new();
    // Bound the walk defensively against a corrupt chain.
    for _ in 0..1_000_000 {
        if fp == 0 || fp % 8 != 0 {
            break;
        }
        // Cranelift records each call's stack map at the return address, so the
        // return address read from the frame is the safepoint key directly.
        let ret = unsafe { ((fp + 8) as *const usize).read() };
        let caller_fp = unsafe { (fp as *const usize).read() };
        if let Some((frame_to_fp, offs)) = maps.get(&ret) {
            let sp = caller_fp.wrapping_sub(*frame_to_fp as usize);
            for &off in offs {
                let slot = sp + off as usize;
                let root = unsafe { (slot as *const usize).read() };
                roots.push(root);
            }
            if gc_debug() {
                eprintln!("[gc] frame ret={ret:#x} matched, {} ref(s)", offs.len());
            }
        }
        if caller_fp <= fp {
            break; // the chain must climb toward `main`
        }
        fp = caller_fp;
    }
    roots
}

// --- mutator threads & stop-the-world (`docs/20`) --------------------------
//
// With multiple OS threads sharing one managed heap, a collection must observe
// *every* live thread's precise roots. We use cooperative safepoints: generated
// code polls [`lang_gc_safepoint`] at loop back-edges, and blocking runtime
// calls bracket themselves with [`enter_native`]/[`leave_native`]. To collect, a
// thread sets the global stop flag and waits until every *other* mutator is
// parked at a safepoint or sitting in native code — its frame pointer recorded
// either way — then scans all stacks, mark-sweeps, and releases the world.

const M_RUNNING: u8 = 0;
const M_PARKED: u8 = 1; // stopped at a safepoint; `fp` valid
const M_NATIVE: u8 = 2; // inside a blocking runtime call; `fp` valid

/// Per-thread mutator record. The collector reads `state`/`fp` of every thread.
struct Mutator {
    id: u64,
    state: AtomicU8,
    /// Frame pointer to scan this thread's roots from, valid when not running.
    fp: AtomicUsize,
    /// Roots this thread scanned from its *own* stack when it parked/blocked.
    /// The collector unions these instead of walking foreign stacks — a thread
    /// scanning its own consistent frame chain is reliable, whereas one thread
    /// reconstructing another's frames is not.
    roots: Mutex<Vec<usize>>,
    /// This thread's private registry of objects it has allocated since the last
    /// time the collector drained it: `(base, total)`. Written on the allocation
    /// hot path (its own, effectively-uncontended mutex), drained into the global
    /// [`Heap::objects`] at the start of every collection while this thread is
    /// stopped (see [`drain_alloc_logs_into`]). This is the per-thread allocation
    /// buffer for the object *registry* (the memory itself comes from a
    /// `gc_alloc` TLAB), keeping the global heap lock off the hot path.
    alloc_log: Mutex<Vec<(usize, usize)>>,
}

/// Drain every mutator's private alloc log into `objects` (the global registry).
///
/// Called at the start of a collection. At a stop-the-world collection the world
/// is quiescent — every other mutator is parked at a safepoint or sitting in a
/// native call, and the collector is the only running thread — so no thread is
/// pushing to its log concurrently; for a direct (test-mode) `collect`, the
/// caller serializes. After this returns, `objects` holds every live managed
/// object: survivors, `@RefCounted` objects, and everything allocated since the
/// previous collection.
fn drain_alloc_logs_into(objects: &mut HashMap<usize, usize>) {
    let muts = MUTATORS.lock().unwrap();
    for m in muts.iter() {
        for (base, total) in m.alloc_log.lock().unwrap().drain(..) {
            objects.insert(base, total);
        }
    }
}

/// All live mutator threads. Registered on a thread's first GC interaction and
/// removed when the thread exits (via [`MutatorHandle`]'s drop).
static MUTATORS: Mutex<Vec<Arc<Mutator>>> = Mutex::new(Vec::new());
static NEXT_MUTATOR_ID: AtomicU64 = AtomicU64::new(1);
/// Set while a collection is in progress; mutators that observe it park.
static STOP: AtomicBool = AtomicBool::new(false);

/// The **world barrier**. The collector holds this across the entire
/// stop→mark→sweep→resume, and every transition *into* `M_RUNNING` (resuming
/// from a park, returning from a native call, or a freshly-spawned thread
/// starting) must acquire it and re-check `STOP` first. This is what makes the
/// stop-the-world sound: a snapshot-and-proceed scheme alone cannot prevent a
/// mutator from re-entering `RUNNING` after the collector's quiescence check and
/// mutating the heap concurrently with marking (dropping a freshly-stored
/// pointer the collector has already scanned past — a use-after-free no
/// stack/register scan can recover). Holding this lock across the collection
/// gives true mutual exclusion: no mutator executes program code while the
/// collector runs. Because a parking thread does **not** take this lock (only a
/// *resuming* one does), the collector's quiescence wait never deadlocks against
/// it.
static WORLD: Mutex<()> = Mutex::new(());

/// Globally-pinned GC roots: field-block pointers kept alive regardless of any
/// thread's stack. A spawned thread's closure environment and (eventual) result
/// live here so they survive collection even during the cross-thread handoff
/// window, when they may not yet be on any scanned stack (`docs/20`).
fn extra_roots() -> &'static Mutex<HashMap<usize, usize>> {
    static R: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Finalizers: `type id → drop function`. A type with an `extend T: Drop` impl
/// registers its monomorphized `drop(self)` here at startup (`docs/16` §8).
pub type DropFn = extern "C" fn(*mut u8);
fn drop_fns() -> &'static Mutex<HashMap<u64, DropFn>> {
    static D: OnceLock<Mutex<HashMap<u64, DropFn>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Objects found unreachable that carry a finalizer: kept alive (and their
/// referents kept alive — they are scanned as roots) until their `drop` runs,
/// then freed. Entries are `(base, total, type_id)`.
static FINALIZE_PENDING: Mutex<Vec<(usize, usize, u64)>> = Mutex::new(Vec::new());

/// Register a finalizer for `type_id` (called once per `Drop` type at startup).
///
/// # Safety
/// `f` must be the type's compiled `drop(self)` with a field-block pointer arg.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_gc_register_drop(type_id: u64, f: DropFn) {
    drop_fns().lock().unwrap().insert(type_id, f);
}

/// Whether any finalizers are registered (fast path: skip finalization work).
fn any_finalizers() -> bool {
    !drop_fns().lock().unwrap().is_empty()
}

// --- `@RefCounted` deterministic ARC (`docs/16` §8.1) ----------------------
//
// A `@RefCounted` object carries a hidden **atomic** strong-count word at
// field-block offset 0. The compiler emits balanced [`lang_rc_retain`] /
// [`lang_rc_release`] around every binding (bind/copy/param/return/capture +
// heap stores). When the count reaches zero the object is destroyed *now*: its
// `Drop` (if any) runs synchronously, its owned refcounted children are
// released (cascading the destruction through the owned graph), and its memory
// is reclaimed — no wait for a collection. Reference cycles keep their counts
// above zero and are left to the tracing GC backstop (`collect`).

/// View a refcounted object's hidden strong-count word (field-block offset 0).
///
/// # Safety
/// `obj` must be a live `@RefCounted` field-block pointer.
#[inline]
unsafe fn rc_count(obj: usize) -> &'static AtomicU64 {
    unsafe { &*(obj as *const AtomicU64) }
}

/// The initial strong count stamped into a fresh `@RefCounted` object by the
/// allocator path (the creating binding owns the one reference).
pub const RC_INITIAL: u64 = 1;

/// Increment a `@RefCounted` object's strong count (retain). A null pointer
/// (e.g. an absent union member) is ignored.
///
/// # Safety
/// `obj` is null or a live `@RefCounted` field-block pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_rc_retain(obj: *mut u8) {
    if obj.is_null() {
        return;
    }
    debug_assert_eq!(
        unsafe { desc_kind((((obj as usize) - HEADER) as *const *const u8).read()) },
        KIND_REFCOUNTED,
        "lang_rc_retain on a non-@RefCounted object",
    );
    unsafe { rc_count(obj as usize) }.fetch_add(1, Ordering::Relaxed);
}

/// Decrement a `@RefCounted` object's strong count (release); at zero, finalize
/// and free it. A null pointer is ignored.
///
/// # Safety
/// `obj` is null or a live `@RefCounted` field-block pointer that the caller
/// will not use again unless it holds another (retained) reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_rc_release(obj: *mut u8) {
    if obj.is_null() {
        return;
    }
    let obj = obj as usize;
    // `Release` so all prior writes through this reference happen-before the
    // destructor; the `Acquire` fence on the final decrement pairs with it.
    let prev = unsafe { rc_count(obj) }.fetch_sub(1, Ordering::Release);
    debug_assert!(prev != 0, "lang_rc_release strong-count underflow");
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe { rc_finalize(obj) };
    }
}

/// Decrement a strong count *without* the zero-transition free. Used by the GC
/// (sweep / finalizer) to keep a surviving referent's count exact after the
/// object that held the edge is reclaimed; the survivor is reachable, so a
/// correct count never reaches zero here.
///
/// # Safety
/// `obj` must be a live `@RefCounted` field-block pointer.
#[inline]
unsafe fn rc_dec_no_free(obj: usize) {
    let c = unsafe { rc_count(obj) };
    // Only adjust genuine refcounted objects (the offset list is built from the
    // descriptor, so this always holds; guarded for robustness under a debug GC).
    if c.load(Ordering::Relaxed) > 0 {
        c.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Destroy a refcounted object whose count just hit zero: run its `Drop`,
/// release the strong references it owned, then reclaim its memory.
///
/// # Safety
/// `obj` is a live `@RefCounted` field-block pointer with strong count `0`.
unsafe fn rc_finalize(obj: usize) {
    let base = obj - HEADER;
    let desc = unsafe { (base as *const *const u8).read() };
    // Pin `self` (and thus its whole graph) for the duration of `drop` + child
    // release, so a nested collection triggered by `drop` cannot reclaim it.
    add_extra_root(obj);
    let tid = unsafe { desc_type_id(desc) };
    if tid != 0 {
        let f = drop_fns().lock().unwrap().get(&tid).copied();
        if let Some(f) = f {
            f(obj as *mut u8); // user `drop(self)`, run synchronously
        }
    }
    // Release the refcounted strong references this object owned (cascade).
    for off in unsafe { desc_rc_offsets(desc) } {
        let child = unsafe { ((obj + off) as *const usize).read() };
        if child != 0 {
            unsafe { lang_rc_release(child as *mut u8) };
        }
    }
    // Reclaim. Remove from the live set first so a (now impossible) concurrent
    // sweep never double-frees; then unpin and free.
    let total = heap().lock().unwrap().objects.remove(&base);
    remove_extra_root(obj);
    if let Some(total) = total {
        crate::gc_alloc::free(base, total);
    }
}

/// Pin `p` as a global root until [`remove_extra_root`].
pub fn add_extra_root(p: usize) {
    let mut roots = extra_roots().lock().unwrap();
    *roots.entry(p).or_insert(0) += 1;
}

/// Unpin one occurrence of `p` from the global roots.
pub fn remove_extra_root(p: usize) {
    let mut roots = extra_roots().lock().unwrap();
    if let Some(count) = roots.get_mut(&p) {
        *count -= 1;
        if *count == 0 {
            roots.remove(&p);
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn extra_root_count_for(p: usize) -> usize {
    extra_roots().lock().unwrap().get(&p).copied().unwrap_or(0)
}

/// C entry point: pin a managed field-block pointer as a global GC root. Used by
/// generated code to keep a `JoinHandle` alive across threads for its lifetime
/// (`docs/20`), regardless of which thread's stack currently references it.
#[unsafe(no_mangle)]
pub extern "C" fn lang_gc_pin(p: *mut u8) {
    add_extra_root(p as usize);
}

/// C entry point: unpin a previously [`lang_gc_pin`]ned pointer.
#[unsafe(no_mangle)]
pub extern "C" fn lang_gc_unpin(p: *mut u8) {
    remove_extra_root(p as usize);
}

thread_local! {
    /// Transient global pins this thread holds across a `poll` (e.g. the future
    /// `block_on` is driving). On the normal path they are released by the
    /// matching [`unpin_for_unwind`]; if a worker panics, the `longjmp` skips
    /// that release, so the panic boundary calls [`release_unwind_pins`] to drop
    /// them — otherwise the worker's abandoned objects would stay pinned (and
    /// thus uncollectable) forever.
    static UNWIND_PINS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Pin `p` globally *and* record it as an unwind-scoped pin on this thread, so a
/// worker panic boundary can release it if the matching [`unpin_for_unwind`] is
/// skipped by a `longjmp`. Use for pins held across a `poll` on a thread that
/// may panic (the async executor's driven future).
pub fn pin_for_unwind(p: usize) {
    add_extra_root(p);
    UNWIND_PINS.with(|v| v.borrow_mut().push(p));
}

/// Unpin a pointer pinned by [`pin_for_unwind`] (normal, non-panicking path).
pub fn unpin_for_unwind(p: usize) {
    remove_extra_root(p);
    UNWIND_PINS.with(|v| {
        let mut b = v.borrow_mut();
        if let Some(i) = b.iter().rposition(|&x| x == p) {
            b.remove(i);
        }
    });
}

/// Release every unwind-scoped pin still held by this thread (worker
/// panic-boundary cleanup, `docs/16`). A no-op when none are outstanding.
pub fn release_unwind_pins() {
    let pins: Vec<usize> = UNWIND_PINS.with(|v| std::mem::take(&mut *v.borrow_mut()));
    for p in pins {
        remove_extra_root(p);
    }
}
/// Serializes collectors: only one thread runs the stop-the-world protocol.
static GC_TURN: Mutex<()> = Mutex::new(());
/// Generation bumped after each collection; parked threads wait on it.
static RESUME_GEN: Mutex<u64> = Mutex::new(0);
static RESUME_CV: Condvar = Condvar::new();

/// Drops a thread's [`Mutator`] registration when the thread exits.
struct MutatorHandle(Arc<Mutator>);
impl Drop for MutatorHandle {
    fn drop(&mut self) {
        // Hand off any objects still in this thread's alloc log to the **global**
        // registry before deregistering. Once this thread is gone, no future
        // collection would otherwise drain its log, so those objects would be
        // invisible to the collector forever — neither traced through nor
        // reclaimed (a leak, and unsound for anything reachable only through
        // them). A pinned worker result and its graph live exactly here.
        {
            let mut heap = heap().lock().unwrap();
            for (base, total) in self.0.alloc_log.lock().unwrap().drain(..) {
                heap.objects.insert(base, total);
            }
        }
        let mut muts = MUTATORS.lock().unwrap();
        muts.retain(|m| !Arc::ptr_eq(m, &self.0));
    }
}

thread_local! {
    static ME: MutatorHandle = {
        let m = Arc::new(Mutator {
            id: NEXT_MUTATOR_ID.fetch_add(1, Ordering::Relaxed),
            state: AtomicU8::new(M_RUNNING),
            fp: AtomicUsize::new(0),
            roots: Mutex::new(Vec::new()),
            alloc_log: Mutex::new(Vec::new()),
        });
        MUTATORS.lock().unwrap().push(m.clone());
        MutatorHandle(m)
    };
}

/// Block until the in-progress collection releases the world.
fn wait_for_resume() {
    let mut g = RESUME_GEN.lock().unwrap();
    while STOP.load(Ordering::Acquire) {
        g = RESUME_CV.wait(g).unwrap();
    }
}

/// Park the calling thread for an in-progress collection: scan its own roots
/// from `fp`'s frame chain (matching only safepoint-keyed frames — intervening
/// runtime frames are skipped), publish them for the collector to union, mark
/// the thread parked, and wait until the world resumes. `fp` must be a frame
/// pointer whose chain climbs through a generated function with a registered
/// safepoint (a loop back-edge, or the generated caller of `lang_alloc`).
#[inline(never)]
fn park_self(fp: usize) {
    let roots = unsafe { scan_stack_roots_from(fp) };
    ME.with(|h| {
        h.0.fp.store(fp, Ordering::SeqCst);
        *h.0.roots.lock().unwrap() = roots;
        h.0.state.store(M_PARKED, Ordering::SeqCst);
        wait_for_resume();
        // Resume only through the world barrier: take `WORLD` and re-check
        // `STOP`, so we cannot re-enter `RUNNING` while a (new) collection holds
        // the barrier. Parking itself never takes `WORLD`, so this cannot
        // deadlock the collector's quiescence wait.
        loop {
            let _world = WORLD.lock().unwrap();
            if STOP.load(Ordering::SeqCst) {
                drop(_world);
                wait_for_resume();
                continue;
            }
            h.0.state.store(M_RUNNING, Ordering::SeqCst);
            break;
        }
    });
}

/// Attempt a stop-the-world collection from the *current* (clean, generated)
/// frame: grab the collector turn, re-check the threshold, stop the world, scan
/// every thread's precise roots, mark-sweep, resume, and run finalizers. A
/// no-op if another thread already holds the turn (it is collecting) or the
/// threshold is no longer met. Caller must be at a point whose frame chain
/// reaches a registered safepoint (a generated loop back-edge, or the generated
/// caller of `lang_alloc`) so the collector's own roots are scannable.
fn run_collection() {
    let _turn = match GC_TURN.try_lock() {
        Ok(turn) => turn,
        Err(_) => {
            // Another collector is active; cooperate by parking if it has begun
            // stopping the world, else just return.
            if STOP.load(Ordering::Acquire) {
                park_self(current_fp());
            }
            return;
        }
    };
    let still_over = BYTES_SINCE_GC.load(Ordering::Relaxed) >= gc_threshold();
    if !still_over {
        return;
    }
    {
        // Hold the world barrier across the whole stop→collect→resume: no mutator
        // can enter `RUNNING` (and run program code) until we clear `STOP` and
        // release this. Combined with the quiescence wait, this guarantees the
        // collector marks/sweeps with the world truly stopped.
        let _world = WORLD.lock().unwrap();
        let roots = stop_the_world();
        unsafe { collect(&roots) };
        resume_the_world();
    }
    // Run finalizers with the world resumed (so `drop` bodies are ordinary code)
    // but still under the GC turn (so no nested collection runs).
    run_finalizers();
}

/// Cooperative safepoint poll emitted by generated code at loop back-edges. When
/// a collection is pending, record this (clean, generated) frame and park until
/// it completes. (Collection is *initiated* from `maybe_collect`; this is purely
/// the cooperation point so a stopping collector can scan this thread's roots.)
#[unsafe(no_mangle)]
pub extern "C" fn lang_gc_safepoint() {
    if !STOP.load(Ordering::Acquire) {
        return;
    }
    park_self(current_fp());
}

/// Cooperative safepoint for runtime scheduler loops that may run generated
/// poll functions back-to-back without entering a blocking native wait.
///
/// The scheduler pins task inputs as extra roots before polling them, so a
/// runtime-frame-only stack scan is sufficient here: language state lives in the
/// pinned task/future graph, while this call simply lets the mutator publish a
/// non-running state for an in-progress stop-the-world collection.
pub fn runtime_safepoint() {
    if !STOP.load(Ordering::Acquire) {
        return;
    }
    park_self(current_fp());
}

/// Register a freshly-spawned worker as a mutator and gate its start on the
/// world barrier, so it cannot begin running program code while a collection is
/// in progress. Call once at the very top of a spawned thread, before any
/// managed code. Its only live managed state here (the closure environment) is
/// pinned by the spawner, so blocking on the barrier is safe even though no
/// generated frame yet exists to scan.
pub fn thread_start() {
    ME.with(|h| {
        // Present as parked (not running) while we contend for the barrier, so a
        // collection in progress does not wait on us — we hold no scannable roots
        // yet, and our env is pinned by the spawner. (Were we to wait on `WORLD`
        // while `M_RUNNING`, the collector — which holds `WORLD` — would spin in
        // its quiescence wait waiting for us: deadlock.)
        h.0.state.store(M_PARKED, Ordering::SeqCst);
        loop {
            if STOP.load(Ordering::SeqCst) {
                wait_for_resume();
            }
            let _world = WORLD.lock().unwrap();
            if STOP.load(Ordering::SeqCst) {
                drop(_world);
                continue;
            }
            h.0.state.store(M_RUNNING, Ordering::SeqCst);
            break;
        }
    });
}

/// Mark the current thread as entering a blocking runtime call (channel recv,
/// thread join, …). Its stack is scannable from the recorded frame while
/// blocked, so a collection on another thread need not wait for it. Pair with
/// [`leave_native`].
///
/// We record the **caller's** frame pointer, not this function's: `enter_native`
/// returns before the caller blocks, so its own frame is gone by collection
/// time. The caller's frame (the runtime function that then waits) stays live,
/// and its return address is the safepoint key carrying the language-level
/// roots held across the blocking call.
#[inline(never)]
pub fn enter_native() {
    let fp = current_fp();
    // The saved caller frame pointer sits at `[fp]` (AAPCS / SysV frame record).
    let caller_fp = unsafe { (fp as *const usize).read() };
    // Scan from the caller's (still-live) frame and publish; the collector unions
    // these rather than walking this thread's frames while it is blocked.
    let roots = unsafe { scan_stack_roots_from(caller_fp) };
    ME.with(|h| {
        h.0.fp.store(caller_fp, Ordering::SeqCst);
        *h.0.roots.lock().unwrap() = roots;
        h.0.state.store(M_NATIVE, Ordering::SeqCst);
    });
}

/// Mark the current thread as blocked in runtime-only code with no language
/// roots on its stack. Executor workers use this while waiting for runnable
/// tasks: any task/future state they may later poll is pinned in heap/runtime
/// structures, and while idle they hold no generated frame to scan.
pub fn enter_runtime_native_no_roots() {
    ME.with(|h| {
        h.0.fp.store(0, Ordering::SeqCst);
        h.0.roots.lock().unwrap().clear();
        h.0.state.store(M_NATIVE, Ordering::SeqCst);
    });
}

/// Leave a blocking runtime call. If a collection is in progress, wait for it
/// before resuming mutation (our stack was already scanned in native state).
pub fn leave_native() {
    ME.with(|h| {
        // Resume through the world barrier (see [`WORLD`]). While a collection
        // holds it, we stay in native (our published roots remain valid) and
        // wait; we run no program code until we hold the barrier with `STOP`
        // clear, so we can never mutate the heap concurrently with marking.
        loop {
            if STOP.load(Ordering::SeqCst) {
                wait_for_resume();
            }
            let _world = WORLD.lock().unwrap();
            if STOP.load(Ordering::SeqCst) {
                drop(_world);
                continue;
            }
            h.0.state.store(M_RUNNING, Ordering::SeqCst);
            break;
        }
    });
}

/// Stop every other mutator and gather the precise roots of all threads. Caller
/// must hold [`GC_TURN`]. Returns once the world is stopped.
fn stop_the_world() -> Vec<usize> {
    // SeqCst pairs with the SeqCst `STOP` re-checks in the `→ RUNNING`
    // transitions (all performed while holding `WORLD`, which this collector
    // also holds) — together they make the barrier handoff race-free.
    STOP.store(true, Ordering::SeqCst);
    let me_ptr = ME.with(|h| Arc::as_ptr(&h.0));
    // Wait for every other mutator to reach a safepoint or native state.
    let mut spins = 0usize;
    loop {
        let pending = {
            let muts = MUTATORS.lock().unwrap();
            muts.iter()
                .any(|m| Arc::as_ptr(m) != me_ptr && m.state.load(Ordering::SeqCst) == M_RUNNING)
        };
        if !pending {
            break;
        }
        if gc_debug() {
            spins = spins.wrapping_add(1);
            if spins == 1 || spins % 1_000_000 == 0 {
                let (states, detail) = {
                    let muts = MUTATORS.lock().unwrap();
                    let mut running = 0usize;
                    let mut parked = 0usize;
                    let mut native = 0usize;
                    let mut detail = Vec::new();
                    for m in muts.iter() {
                        match m.state.load(Ordering::SeqCst) {
                            M_RUNNING => {
                                running += 1;
                                detail.push(format!("#{}:running", m.id));
                            }
                            M_PARKED => {
                                parked += 1;
                                detail.push(format!("#{}:parked", m.id));
                            }
                            M_NATIVE => {
                                native += 1;
                                detail.push(format!("#{}:native", m.id));
                            }
                            _ => {}
                        }
                    }
                    ((running, parked, native), detail.join(" "))
                };
                eprintln!(
                    "[gc] waiting for world: running={} parked={} native={} [{}]",
                    states.0, states.1, states.2, detail
                );
            }
        }
        std::thread::yield_now();
    }
    // This thread's own roots (scanned here, from its own live frames), plus the
    // roots every other thread published when it parked or went native.
    let mut roots = unsafe { scan_stack_roots_from(current_fp()) };
    let muts = MUTATORS.lock().unwrap();
    for m in muts.iter() {
        if Arc::as_ptr(m) == me_ptr {
            continue;
        }
        roots.extend(m.roots.lock().unwrap().iter().copied());
    }
    roots
}

/// Release all parked threads after a collection.
fn resume_the_world() {
    STOP.store(false, Ordering::Release);
    let mut g = RESUME_GEN.lock().unwrap();
    *g = g.wrapping_add(1);
    RESUME_CV.notify_all();
}

thread_local! {
    /// Re-entrant pause count for this mutator. While non-zero, this thread's
    /// `maybe_collect` is a no-op. Runtime helpers that allocate several
    /// managed objects while holding unrooted intermediates (e.g. `Map.keys`)
    /// bracket themselves with [`pause`]/[`resume`] so a stress-mode collection
    /// cannot free a half-built result.
    ///
    /// This must be thread-local: one executor worker's half-built result must
    /// never make another worker ignore a pending stop-the-world request.
    static PAUSE_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Suspend collection until the matching [`resume`].
pub fn pause() {
    PAUSE_DEPTH.with(|depth| depth.set(depth.get() + 1));
}

/// Resume collection (undo one [`pause`]).
pub fn resume() {
    let should_park = PAUSE_DEPTH.with(|depth| {
        let n = depth.get();
        debug_assert!(n > 0, "gc::resume without matching gc::pause");
        let next = n.saturating_sub(1);
        depth.set(next);
        next == 0 && STOP.load(Ordering::Acquire)
    });
    if should_park {
        park_self(current_fp());
    }
}

/// Resume collection while keeping a freshly-built managed return value rooted
/// across a possible stop-the-world park at the pause boundary.
///
/// Runtime helpers commonly build a small graph of result boxes under
/// [`pause`] and then return the outer box to generated code. If another
/// mutator requested a collection while the helper was paused, [`resume`] can
/// park this thread before the generated caller has the return value in a
/// stack-map-visible slot. Pinning the outer box for just this boundary lets the
/// collector trace the whole result graph without leaving a long-lived root.
pub fn resume_with_return_root(p: usize) {
    if p != 0 {
        add_extra_root(p);
    }
    resume();
    if p != 0 {
        remove_extra_root(p);
    }
}

/// If collection is enabled and the allocation budget is exhausted, run a
/// stop-the-world collection using the precise roots of every mutator thread.
/// Call *before* allocating the new (not-yet-rooted) object.
fn maybe_collect() {
    // Ensure this thread is registered as a mutator (idempotent).
    ME.with(|_| {});
    if !gc_enabled() {
        return;
    }
    // A `pause`d thread holds unrooted intermediates (e.g. a half-built `Map`),
    // so it must neither collect nor be collected through. It does not park here;
    // its `pause` sections are bounded and followed by a generated safepoint,
    // where it parks if a collection is then pending — so a waiting collector
    // makes progress as soon as the pause ends. Until then it allocates freely.
    if PAUSE_DEPTH.with(|depth| depth.get() != 0) {
        return;
    }
    // Concurrent reclamation is ENABLED. Two prerequisites are in place:
    // (1) the custom slab allocator (`gc_alloc`) means the sweep reclaims into
    // our own free lists and never deadlocks against a mutator parked inside the
    // system `malloc`; (2) the world barrier (`WORLD`) gives the collector true
    // mutual exclusion — held across the whole stop→mark→sweep→resume, with every
    // `→ RUNNING` transition (park-resume, native-return, thread-start) gated on
    // it — so no mutator ever runs program code, and thus never mutates the heap,
    // while the collector marks/sweeps (`docs/16`/`docs/20`).
    if STOP.load(Ordering::SeqCst) {
        // A collection by the (sole) prior collector is wrapping up; cooperate.
        park_self(current_fp());
        return;
    }
    let over = BYTES_SINCE_GC.load(Ordering::Relaxed) >= gc_threshold();
    if over {
        run_collection();
    }
}

/// Free every registered object (used at process teardown in tests; the real
/// collector reclaims incrementally). Exposed for completeness.
///
/// # Safety
/// No managed pointers may be used after this call.
pub unsafe fn free_all() {
    let mut heap = heap().lock().unwrap();
    // Include objects still in per-thread alloc logs, so nothing leaks.
    drain_alloc_logs_into(&mut heap.objects);
    for (base, total) in heap.objects.drain() {
        crate::gc_alloc::free(base, total);
    }
    BYTES_SINCE_GC.store(0, Ordering::Relaxed);
}

/// Serializes tests that mutate the process-global heap. The collector, the
/// `@RefCounted` primitives, the threads runtime, and the panic boundary all
/// share one heap, so any test that allocates managed objects or asserts heap
/// state must hold this lock (and `free_all` to reset) — otherwise concurrent
/// test threads interleave allocations and corrupt each other's assertions.
/// Shared across modules (`gc::tests`, `threads::tests`, `panic_boundary::tests`).
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // Build a plain descriptor blob with the given size and pointer offsets.
    fn plain_desc(size: u64, ptrs: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&KIND_PLAIN.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes()); // type_id (no Drop)
        b.extend_from_slice(&(ptrs.len() as u64).to_le_bytes());
        for o in ptrs {
            b.extend_from_slice(&o.to_le_bytes());
        }
        b.extend_from_slice(&0u32.to_le_bytes()); // n_rc trailer (no refcounted fields)
        b
    }

    // -- `@RefCounted` ARC primitives ------------------------------------

    /// Build a `@RefCounted` descriptor blob: hidden count word at offset 0,
    /// `ptrs` the GC trace map, `rc_offs` the owned-refcounted-children trailer.
    fn rc_desc(size: u64, ptrs: &[u32], rc_offs: &[u32], type_id: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&KIND_REFCOUNTED.to_le_bytes());
        b.extend_from_slice(&type_id.to_le_bytes());
        b.extend_from_slice(&(ptrs.len() as u64).to_le_bytes());
        for o in ptrs {
            b.extend_from_slice(&o.to_le_bytes());
        }
        b.extend_from_slice(&(rc_offs.len() as u32).to_le_bytes());
        for o in rc_offs {
            b.extend_from_slice(&o.to_le_bytes());
        }
        b
    }

    /// Drops record the object's first user field (a `u64` id at offset 8) so a
    /// test can assert deterministic drop ordering.
    fn dropped() -> &'static Mutex<Vec<u64>> {
        static D: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
        D.get_or_init(|| Mutex::new(Vec::new()))
    }
    extern "C" fn record_drop(obj: *mut u8) {
        let id = unsafe { ((obj as usize + 8) as *const u64).read() };
        dropped().lock().unwrap().push(id);
    }

    /// Allocate a refcounted object, stamp its initial strong count + id field.
    unsafe fn rc_alloc(desc: &[u8], id: u64) -> usize {
        let p = unsafe { alloc(desc.as_ptr()) } as usize;
        unsafe { rc_count(p) }.store(RC_INITIAL, Ordering::Relaxed);
        unsafe { ((p + 8) as *mut u64).write(id) };
        p
    }

    #[test]
    fn rc_retain_release_balance_and_deterministic_drop() {
        let _g = TEST_LOCK.lock().unwrap();
        unsafe { free_all() };
        dropped().lock().unwrap().clear();
        let tid = 90_001u64;
        unsafe { lang_gc_register_drop(tid, record_drop) };
        let desc = rc_desc(16, &[], &[], tid);
        unsafe {
            let p = rc_alloc(&desc, 42);
            assert_eq!(live_count(), 1);
            lang_rc_retain(p as *mut u8); // count 2
            lang_rc_release(p as *mut u8); // count 1 — still live, no drop
            assert_eq!(live_count(), 1);
            assert!(
                dropped().lock().unwrap().is_empty(),
                "no drop while count > 0"
            );
            lang_rc_release(p as *mut u8); // count 0 — drop runs synchronously, freed
            assert_eq!(dropped().lock().unwrap().as_slice(), &[42]);
            assert_eq!(live_count(), 0, "freed at count zero without a collection");
        }
    }

    #[test]
    fn rc_release_cascades_through_owned_children() {
        let _g = TEST_LOCK.lock().unwrap();
        unsafe { free_all() };
        dropped().lock().unwrap().clear();
        let tid = 90_002u64;
        unsafe { lang_gc_register_drop(tid, record_drop) };
        let child_desc = rc_desc(16, &[], &[], tid); // [count][id]
        let parent_desc = rc_desc(24, &[16], &[16], tid); // [count][id][child]
        unsafe {
            let child = rc_alloc(&child_desc, 1);
            let parent = rc_alloc(&parent_desc, 2);
            // parent owns child: store + retain (the heap-store ARC discipline).
            ((parent + 16) as *mut usize).write(child);
            lang_rc_retain(child as *mut u8); // child count 2
            lang_rc_release(child as *mut u8); // drop the local ref → count 1
            assert_eq!(live_count(), 2);
            assert!(dropped().lock().unwrap().is_empty());
            // Last reference to the root dies → parent freed, then its owned
            // child released to zero and freed too. Owner drops before owned.
            lang_rc_release(parent as *mut u8);
            assert_eq!(dropped().lock().unwrap().as_slice(), &[2, 1]);
            assert_eq!(live_count(), 0);
        }
    }

    #[test]
    fn rc_shared_child_survives_until_last_owner() {
        let _g = TEST_LOCK.lock().unwrap();
        unsafe { free_all() };
        dropped().lock().unwrap().clear();
        let tid = 90_003u64;
        unsafe { lang_gc_register_drop(tid, record_drop) };
        let child_desc = rc_desc(16, &[], &[], tid);
        let parent_desc = rc_desc(24, &[16], &[16], tid);
        unsafe {
            let child = rc_alloc(&child_desc, 1);
            let p1 = rc_alloc(&parent_desc, 2);
            let p2 = rc_alloc(&parent_desc, 3);
            // Both parents own the child.
            ((p1 + 16) as *mut usize).write(child);
            lang_rc_retain(child as *mut u8);
            ((p2 + 16) as *mut usize).write(child);
            lang_rc_retain(child as *mut u8);
            lang_rc_release(child as *mut u8); // drop local → child count 2 (p1, p2)
            // Free p1 → child still held by p2, not dropped.
            lang_rc_release(p1 as *mut u8);
            assert_eq!(dropped().lock().unwrap().as_slice(), &[2]);
            assert_eq!(live_count(), 2, "child + p2 remain");
            // Free p2 → child finally drops.
            lang_rc_release(p2 as *mut u8);
            assert_eq!(dropped().lock().unwrap().as_slice(), &[2, 3, 1]);
            assert_eq!(live_count(), 0);
        }
    }

    #[test]
    fn rc_cycle_is_reclaimed_by_gc_backstop() {
        let _g = TEST_LOCK.lock().unwrap();
        unsafe { free_all() };
        dropped().lock().unwrap().clear();
        let tid = 90_004u64;
        unsafe { lang_gc_register_drop(tid, record_drop) };
        let desc = rc_desc(24, &[16], &[16], tid); // [count][id][peer]
        unsafe {
            let a = rc_alloc(&desc, 10);
            let b = rc_alloc(&desc, 11);
            // a <-> b cycle; each owns the other (store + retain).
            ((a + 16) as *mut usize).write(b);
            lang_rc_retain(b as *mut u8);
            ((b + 16) as *mut usize).write(a);
            lang_rc_retain(a as *mut u8);
            // Drop both external references; counts stay at 1 (the cycle edges).
            lang_rc_release(a as *mut u8);
            lang_rc_release(b as *mut u8);
            assert_eq!(live_count(), 2, "refcounting alone leaks the cycle");
            assert!(dropped().lock().unwrap().is_empty());
            // The tracing GC backstop reclaims the unreachable cycle.
            collect(&[]);
            run_finalizers();
            assert_eq!(live_count(), 0, "GC reclaimed the cycle");
            let mut got = dropped().lock().unwrap().clone();
            got.sort();
            assert_eq!(got, vec![10, 11], "both cycle members dropped");
        }
    }

    #[test]
    fn rc_cross_thread_retain_release_is_atomic() {
        let _g = TEST_LOCK.lock().unwrap();
        unsafe { free_all() };
        dropped().lock().unwrap().clear();
        let tid = 90_005u64;
        unsafe { lang_gc_register_drop(tid, record_drop) };
        let desc = rc_desc(16, &[], &[], tid);
        unsafe {
            let p = rc_alloc(&desc, 7);
            // Hand N extra references to N threads; each retains then releases
            // many times, net zero, then releases its handed-in reference.
            let n = 8usize;
            let iters = 5000usize;
            for _ in 0..n {
                lang_rc_retain(p as *mut u8);
            }
            assert_eq!(rc_count(p).load(Ordering::Relaxed), 1 + n as u64);
            let addr = p;
            let mut handles = Vec::new();
            for _ in 0..n {
                handles.push(std::thread::spawn(move || {
                    let q = addr as *mut u8;
                    for _ in 0..iters {
                        lang_rc_retain(q);
                        lang_rc_release(q);
                    }
                    lang_rc_release(q); // release the handed-in reference
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            // The original reference remains.
            assert_eq!(rc_count(p).load(Ordering::Relaxed), 1);
            assert!(dropped().lock().unwrap().is_empty());
            lang_rc_release(p as *mut u8);
            assert_eq!(dropped().lock().unwrap().as_slice(), &[7]);
            assert_eq!(live_count(), 0);
        }
    }

    #[test]
    fn collect_reclaims_unreachable_keeps_reachable() {
        let _g = TEST_LOCK.lock().unwrap();
        // A.field0 -> B ; C is unreachable. Root = {A}.
        let a_desc = plain_desc(8, &[0]); // one pointer field at offset 0
        let leaf = plain_desc(8, &[]);
        unsafe {
            free_all(); // start clean
            let a = alloc(a_desc.as_ptr());
            let b = alloc(leaf.as_ptr());
            let _c = alloc(leaf.as_ptr());
            // A.field0 = B
            (a as *mut usize).write(b as usize);
            assert_eq!(live_count(), 3);

            let freed = collect(&[a as usize]);
            assert!(freed > 0);
            assert_eq!(live_count(), 2, "A and B reachable; C collected");

            // A and B still valid: re-read the link.
            assert_eq!((a as *const usize).read(), b as usize);
            free_all();
        }
    }

    #[test]
    fn stop_the_world_coordinates_mutator_threads() {
        // Exercises the multi-thread paths: worker threads poll safepoints while
        // the main thread runs several stop-the-world cycles. Each cycle must
        // complete (every worker parks, its stack is scanned, then it resumes)
        // without hanging or crashing.
        let _g = TEST_LOCK.lock().unwrap();
        use std::sync::Arc as StdArc;
        let go = StdArc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let go = go.clone();
            handles.push(std::thread::spawn(move || {
                thread_start();
                let mut spins = 0u64;
                while go.load(Ordering::Relaxed) {
                    lang_gc_safepoint();
                    spins = spins.wrapping_add(1);
                    if spins % 4096 == 0 {
                        std::thread::yield_now();
                    }
                }
            }));
        }
        // Let the workers start and register as mutators.
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..5 {
            let turn = GC_TURN.lock().unwrap();
            let _roots = stop_the_world();
            // While stopped, every other mutator must be parked or native.
            let me_ptr = ME.with(|h| StdArc::as_ptr(&h.0));
            {
                let muts = MUTATORS.lock().unwrap();
                for m in muts.iter() {
                    if StdArc::as_ptr(m) == me_ptr {
                        continue;
                    }
                    assert_ne!(m.state.load(Ordering::SeqCst), M_RUNNING);
                }
            }
            resume_the_world();
            drop(turn);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        go.store(false, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn stop_the_world_coordinates_runtime_only_safepoints() {
        // Executor workers can briefly spin or loop in runtime code without a
        // generated language frame. They still register as mutators and must
        // publish a non-running state when a collection starts.
        let _g = TEST_LOCK.lock().unwrap();
        use std::sync::Arc as StdArc;
        let go = StdArc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let go = go.clone();
            handles.push(std::thread::spawn(move || {
                thread_start();
                while go.load(Ordering::Relaxed) {
                    runtime_safepoint();
                    std::thread::yield_now();
                }
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..5 {
            let turn = GC_TURN.lock().unwrap();
            let _roots = stop_the_world();
            let me_ptr = ME.with(|h| StdArc::as_ptr(&h.0));
            {
                let muts = MUTATORS.lock().unwrap();
                for m in muts.iter() {
                    if StdArc::as_ptr(m) == me_ptr {
                        continue;
                    }
                    assert_ne!(m.state.load(Ordering::SeqCst), M_RUNNING);
                }
            }
            resume_the_world();
            drop(turn);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        go.store(false, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn stop_the_world_coordinates_runtime_idle_native_threads() {
        // Idle executor workers block in runtime code with no language roots.
        // They must be visible as native, not running, while asleep.
        let _g = TEST_LOCK.lock().unwrap();
        use std::sync::Arc as StdArc;
        let go = StdArc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        for _ in 0..3 {
            let go = go.clone();
            handles.push(std::thread::spawn(move || {
                thread_start();
                while go.load(Ordering::Relaxed) {
                    enter_runtime_native_no_roots();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    leave_native();
                }
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..5 {
            let turn = GC_TURN.lock().unwrap();
            let _roots = stop_the_world();
            let me_ptr = ME.with(|h| StdArc::as_ptr(&h.0));
            {
                let muts = MUTATORS.lock().unwrap();
                for m in muts.iter() {
                    if StdArc::as_ptr(m) == me_ptr {
                        continue;
                    }
                    assert_ne!(m.state.load(Ordering::SeqCst), M_RUNNING);
                }
            }
            resume_the_world();
            drop(turn);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        go.store(false, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn collect_frees_everything_with_no_roots() {
        let _g = TEST_LOCK.lock().unwrap();
        let leaf = plain_desc(8, &[]);
        unsafe {
            free_all();
            let _a = alloc(leaf.as_ptr());
            let _b = alloc(leaf.as_ptr());
            assert_eq!(live_count(), 2);
            collect(&[]);
            assert_eq!(live_count(), 0);
        }
    }

    #[test]
    fn alloc_writes_header_and_returns_field_block() {
        let _g = TEST_LOCK.lock().unwrap();
        let desc = plain_desc(24, &[8]);
        unsafe {
            let p = alloc(desc.as_ptr());
            // The descriptor pointer is stored 16 bytes before the field block.
            let stored = (p.sub(HEADER) as *const *const u8).read();
            assert_eq!(stored, desc.as_ptr());
            assert_eq!(desc_size(stored), 24);
            assert_eq!(desc_kind(stored), KIND_PLAIN);
            // Field block is zeroed.
            assert_eq!((p as *const u64).read(), 0);
            free_all();
        }
    }

    #[test]
    fn extra_roots_are_counted_and_removed_in_constant_time_shape() {
        let _g = TEST_LOCK.lock().unwrap();
        let p = 0xE_7120_0001usize;

        add_extra_root(p);
        add_extra_root(p);
        assert_eq!(extra_root_count_for(p), 2);

        remove_extra_root(p);
        assert_eq!(extra_root_count_for(p), 1);

        remove_extra_root(p);
        assert_eq!(extra_root_count_for(p), 0);

        remove_extra_root(p);
        assert_eq!(extra_root_count_for(p), 0);
    }
}
