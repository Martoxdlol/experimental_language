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
//!   [ size: u64 ][ kind: u64 ][ n_ptrs: u64 ][ off_0: u32 ] … [ off_{n-1}: u32 ]
//! ```
//!
//! * `size`   — field-block size in bytes.
//! * `kind`   — 0 = plain (scan the listed pointer-field offsets), 1 = `str`,
//!   2 = `List` (scan handled specially by the collector).
//! * `n_ptrs` / `off_i` — byte offsets, within the field block, of fields that
//!   hold managed pointers (for `kind == 0`).
//!
//! This module currently owns allocation and a registry of live objects; the
//! mark-sweep collector and precise-root scan build on it (see `ROADMAP.md`).

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Size of the object header in bytes: `[desc ptr][mark word]`.
pub const HEADER: usize = 16;

/// Descriptor `kind` values.
pub const KIND_PLAIN: u64 = 0;
pub const KIND_STR: u64 = 1;
pub const KIND_LIST: u64 = 2;
pub const KIND_MAP: u64 = 3;

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

/// The registry of live managed objects (base address + total byte size,
/// including the header). The collector sweeps this list.
struct Heap {
    objects: Vec<(usize, usize)>,
    /// Bytes allocated since the last collection (drives the GC trigger).
    bytes_since_gc: usize,
}

static HEAP: Mutex<Heap> = Mutex::new(Heap { objects: Vec::new(), bytes_since_gc: 0 });

/// A statically-allocated descriptor (its blob layout matches what the
/// collector reads: `[size][kind][type_id][n_ptrs]`). Builtin descriptors carry
/// `type_id == 0` (no `Drop`).
#[repr(C, align(8))]
pub struct StaticDesc {
    pub size: u64,
    pub kind: u64,
    pub type_id: u64,
    pub n_ptrs: u64,
}

/// Shared descriptor for `str` objects (variable size; bytes inline, leaf).
pub static STR_DESC: StaticDesc = StaticDesc { size: 0, kind: KIND_STR, type_id: 0, n_ptrs: 0 };
/// Shared descriptor for a `List` handle: `[len][cap][buf][elem_is_ptr]`.
/// The collector special-cases `kind == LIST` to trace the buffer's elements.
pub static LIST_HANDLE_DESC: StaticDesc = StaticDesc { size: 32, kind: KIND_LIST, type_id: 0, n_ptrs: 0 };
/// Shared descriptor for a `List` element buffer (variable size, leaf — it is
/// traced via its owning `List` handle, which knows the length/elem-kind).
pub static LIST_BUF_DESC: StaticDesc = StaticDesc { size: 0, kind: KIND_PLAIN, type_id: 0, n_ptrs: 0 };
/// Shared descriptor for a `Map` handle:
/// `[len][cap][buf][key_is_ptr][val_is_ptr][hash_fn][eq_fn]` (56 B). The
/// `hash_fn`/`eq_fn` slots are nullable function pointers; when non-null, the
/// runtime calls through them (used for user-typed keys implementing
/// `Eq + Hash`, `docs/15` §7). The collector special-cases `kind == MAP` to
/// trace each occupied slot's key/value as needed.
pub static MAP_HANDLE_DESC: StaticDesc = StaticDesc { size: 56, kind: KIND_MAP, type_id: 0, n_ptrs: 0 };
/// Shared descriptor for a `Map` slot buffer (variable size, leaf — traced via
/// its owning handle, which knows the capacity and key/value pointer-ness).
pub static MAP_BUF_DESC: StaticDesc = StaticDesc { size: 0, kind: KIND_PLAIN, type_id: 0, n_ptrs: 0 };

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
    let layout = Layout::from_size_align(total.max(1), 8).expect("valid layout");
    let base = unsafe { alloc_zeroed(layout) };
    if base.is_null() {
        std::process::abort();
    }
    // header.desc at +0; header.mark at +8 (already zero).
    unsafe { (base as *mut *const u8).write(desc) };

    let mut heap = HEAP.lock().unwrap();
    heap.objects.push((base as usize, total));
    heap.bytes_since_gc += total;
    drop(heap);

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
    let mut heap = HEAP.lock().unwrap();

    // Objects awaiting finalization are kept alive (and their referents kept
    // alive) until their `drop` runs — include them so marking traverses them.
    let pending: Vec<(usize, usize, u64)> = FINALIZE_PENDING.lock().unwrap().clone();
    let mut bases: HashSet<usize> = heap.objects.iter().map(|&(b, _)| b).collect();
    for &(b, _, _) in &pending {
        bases.insert(b);
    }

    // --- mark -------------------------------------------------------------
    // Stack roots, globally-pinned roots (`EXTRA_ROOTS`), and the graphs of
    // objects still pending finalization.
    let is_obj = |fb: usize| fb != 0 && fb >= HEADER && bases.contains(&(fb - HEADER));
    let mut work: Vec<usize> = roots.iter().copied().filter(|&p| is_obj(p)).collect();
    work.extend(EXTRA_ROOTS.lock().unwrap().iter().copied().filter(|&p| is_obj(p)));
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
        for &(base, total) in &heap.objects {
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
    let _newly_set: HashSet<usize> = newly.iter().map(|&(b, _, _)| b).collect();

    // --- sweep ------------------------------------------------------------
    let mut freed = 0usize;
    let mut survivors = Vec::with_capacity(heap.objects.len());
    let mut new_pending: Vec<(usize, usize, u64)> = Vec::new();
    for (base, total) in heap.objects.drain(..) {
        let mark = (base + 8) as *mut u64;
        if let Some(&(_, _, tid)) = newly.iter().find(|&&(b, _, _)| b == base) {
            // Unreachable but finalizable: hand off to the finalizer queue.
            unsafe { *mark = 0 };
            new_pending.push((base, total, tid));
        } else if unsafe { *mark } != 0 {
            unsafe { *mark = 0 }; // clear for next cycle
            survivors.push((base, total));
        } else {
            let layout = Layout::from_size_align(total, 8).unwrap();
            unsafe { dealloc(base as *mut u8, layout) };
            freed += total;
        }
    }
    let kept = survivors.len();
    heap.objects = survivors;
    heap.bytes_since_gc = 0;
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
            KIND_PLAIN => {
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
        let Some((base, total, tid)) = item else { break };
        let f = drop_fns().lock().unwrap().get(&tid).copied();
        if let Some(f) = f {
            f((base + HEADER) as *mut u8); // user `drop(self)`
        }
        let layout = Layout::from_size_align(total, 8).unwrap();
        unsafe { dealloc(base as *mut u8, layout) };
    }
}

/// Number of live objects (for tests / introspection).
pub fn live_count() -> usize {
    HEAP.lock().unwrap().objects.len()
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
    let env = *E.get_or_init(|| matches!(std::env::var("OTTER_FUSION_GC").as_deref(), Ok(v) if v != "off"));
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
    state: AtomicU8,
    /// Frame pointer to scan this thread's roots from, valid when not running.
    fp: AtomicUsize,
    /// Roots this thread scanned from its *own* stack when it parked/blocked.
    /// The collector unions these instead of walking foreign stacks — a thread
    /// scanning its own consistent frame chain is reliable, whereas one thread
    /// reconstructing another's frames is not.
    roots: Mutex<Vec<usize>>,
}

/// All live mutator threads. Registered on a thread's first GC interaction and
/// removed when the thread exits (via [`MutatorHandle`]'s drop).
static MUTATORS: Mutex<Vec<Arc<Mutator>>> = Mutex::new(Vec::new());
/// Set while a collection is in progress; mutators that observe it park.
static STOP: AtomicBool = AtomicBool::new(false);

/// Globally-pinned GC roots: field-block pointers kept alive regardless of any
/// thread's stack. A spawned thread's closure environment and (eventual) result
/// live here so they survive collection even during the cross-thread handoff
/// window, when they may not yet be on any scanned stack (`docs/20`).
static EXTRA_ROOTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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

/// Pin `p` as a global root until [`remove_extra_root`].
pub fn add_extra_root(p: usize) {
    EXTRA_ROOTS.lock().unwrap().push(p);
}

/// Unpin one occurrence of `p` from the global roots.
pub fn remove_extra_root(p: usize) {
    let mut r = EXTRA_ROOTS.lock().unwrap();
    if let Some(i) = r.iter().position(|&x| x == p) {
        r.swap_remove(i);
    }
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
/// Serializes collectors: only one thread runs the stop-the-world protocol.
static GC_TURN: Mutex<()> = Mutex::new(());
/// Generation bumped after each collection; parked threads wait on it.
static RESUME_GEN: Mutex<u64> = Mutex::new(0);
static RESUME_CV: Condvar = Condvar::new();

/// Drops a thread's [`Mutator`] registration when the thread exits.
struct MutatorHandle(Arc<Mutator>);
impl Drop for MutatorHandle {
    fn drop(&mut self) {
        let mut muts = MUTATORS.lock().unwrap();
        muts.retain(|m| !Arc::ptr_eq(m, &self.0));
    }
}

thread_local! {
    static ME: MutatorHandle = {
        let m = Arc::new(Mutator {
            state: AtomicU8::new(M_RUNNING),
            fp: AtomicUsize::new(0),
            roots: Mutex::new(Vec::new()),
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
        h.0.state.store(M_RUNNING, Ordering::SeqCst);
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
    let still_over = {
        let heap = HEAP.lock().unwrap();
        heap.bytes_since_gc >= gc_threshold()
    };
    if !still_over {
        return;
    }
    let roots = stop_the_world();
    unsafe { collect(&roots) };
    resume_the_world();
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

/// Leave a blocking runtime call. If a collection is in progress, wait for it
/// before resuming mutation (our stack was already scanned in native state).
pub fn leave_native() {
    ME.with(|h| {
        if STOP.load(Ordering::Acquire) {
            wait_for_resume();
        }
        h.0.state.store(M_RUNNING, Ordering::SeqCst);
    });
}

/// Stop every other mutator and gather the precise roots of all threads. Caller
/// must hold [`GC_TURN`]. Returns once the world is stopped.
fn stop_the_world() -> Vec<usize> {
    STOP.store(true, Ordering::Release);
    let me_ptr = ME.with(|h| Arc::as_ptr(&h.0));
    // Wait for every other mutator to reach a safepoint or native state.
    loop {
        let pending = {
            let muts = MUTATORS.lock().unwrap();
            muts.iter().any(|m| {
                Arc::as_ptr(m) != me_ptr && m.state.load(Ordering::SeqCst) == M_RUNNING
            })
        };
        if !pending {
            break;
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

/// Re-entrant pause count. While non-zero, `maybe_collect` is a no-op. Runtime
/// helpers that allocate several managed objects while holding unrooted
/// intermediates (e.g. `Map.keys`) bracket themselves with [`pause`]/[`resume`]
/// so a stress-mode collection cannot free a half-built result.
static PAUSE_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Suspend collection until the matching [`resume`].
pub fn pause() {
    PAUSE_DEPTH.fetch_add(1, Ordering::SeqCst);
}

/// Resume collection (undo one [`pause`]).
pub fn resume() {
    PAUSE_DEPTH.fetch_sub(1, Ordering::SeqCst);
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
    if PAUSE_DEPTH.load(Ordering::SeqCst) != 0 {
        return;
    }
    // Concurrent *reclamation* remains gated: while more than one mutator is live
    // we do not collect (garbage retention grows, reclaimed once the spawned
    // threads join). This is memory-safe — a live object is never freed. Enabling
    // collection under concurrency was attempted (the stop-the-world machinery,
    // exercised by `stop_the_world_coordinates_mutator_threads`, is correct for
    // light workloads and all the concurrency e2e cases pass under it), but a
    // use-after-free surfaces under *heavy* concurrent allocation (a live object
    // reachable only through a not-yet-published cross-thread root is swept). The
    // precise root-scanning hardening that closes this — or the move to MMTk — is
    // the production path (`docs/16`/`docs/20`/`ROADMAP.md`).
    if MUTATORS.lock().unwrap().len() > 1 {
        return;
    }
    if STOP.load(Ordering::Acquire) {
        // A collection by the (sole) prior collector is wrapping up; cooperate.
        park_self(current_fp());
        return;
    }
    let over = {
        let heap = HEAP.lock().unwrap();
        heap.bytes_since_gc >= gc_threshold()
    };
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
    let mut heap = HEAP.lock().unwrap();
    for (base, total) in heap.objects.drain(..) {
        let layout = Layout::from_size_align(total, 8).unwrap();
        unsafe { dealloc(base as *mut u8, layout) };
    }
    heap.bytes_since_gc = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global heap, so they must not interleave.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

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
        b
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
}
