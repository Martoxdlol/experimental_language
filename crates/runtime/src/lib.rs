//! The language runtime: functions compiled programs call into.
//!
//! This is the provisional, pre-GC runtime. `str` is represented as a pointer
//! to a [`LangStr`] header (length + data pointer); the data and headers are
//! heap-allocated and **currently leaked** — there is no collector yet, so the
//! best-effort-drop contract of `docs/16` is trivially (if wastefully)
//! satisfied. When the tracing GC lands, these allocations move onto the
//! managed heap with the two-word object header and these functions become
//! thin shims over the collector's allocator.
//!
//! Every entry point uses the C ABI and a `lang_` prefix so the code generator
//! can reference them by a stable symbol name (JIT) or link against them
//! (object output).

pub mod async_rt;
pub mod atomic;
pub mod channels;
pub mod foreign;
pub mod fs;
pub mod gc;
pub mod gc_alloc;
pub mod hash;
pub mod list;
pub mod map;
pub mod net;
pub mod panic_boundary;
pub mod process;
pub mod rand;
pub mod shared;
pub mod strings;
pub mod threads;
pub mod time;
pub mod variadic;

// Re-export the `str`/`List`/`Map` intrinsics at the crate root so generated
// code and the backend keep referring to them as `runtime::lang_*` (and to
// `runtime::LangStr` / `runtime::str_bytes`).
pub use hash::*;
pub use list::*;
pub use map::*;
pub use strings::*;

/// Allocate a managed object described by `desc`, returning a pointer to its
/// field block. The two-word object header (`docs/16` §3) sits at negative
/// offsets, so field offsets are unaffected. Managed allocation is infallible
/// (aborts on OOM, `docs/16` §11).
///
/// `desc` is an inline descriptor blob (see [`gc`]); the code generator emits
/// one per managed type.
///
/// # Safety
/// `desc` must point to a valid descriptor blob that outlives all its objects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_alloc(desc: *const u8) -> *mut u8 {
    unsafe { gc::alloc(desc) }
}

/// Raise a language panic with `msg` (`docs/14`).
///
/// On a **spawned worker** (`Thread.spawn` / `spawn EXPR`), a panic is isolated
/// to that worker: a `setjmp`/`longjmp` boundary is installed at the worker's
/// entry (see [`panic_boundary`]), so this captures the message and unwinds
/// there — the worker surfaces as `Panicked { message }` to its joiner (or the
/// panic re-propagates at a `spawn` awaiter) without aborting the process. On
/// the **main thread** no boundary is installed, so the panic is fatal: print
/// to stderr and exit with code 101 (the conventional panic code).
///
/// Not catchable from language code in either case.
///
/// # Safety
/// `msg` must be a valid `LangStr` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_panic(msg: *const LangStr) -> ! {
    let bytes = unsafe { str_bytes(msg) };
    if panic_boundary::boundary_active() {
        // A spawned worker: unwind to its boundary instead of killing the
        // process. The boundary restores GC/lock invariants and reports the
        // panic to the joiner.
        unsafe { panic_boundary::capture_panic(bytes) };
    }
    eprintln!("panic: {}", String::from_utf8_lossy(bytes));
    std::process::exit(101);
}

/// Terminate the process with an explicit exit code (`docs/24`: `exit(code):
/// never`). Returns control to the OS; no `Drop` runs.
#[unsafe(no_mangle)]
pub extern "C" fn lang_exit(code: i32) -> ! {
    std::process::exit(code);
}

/// Abort the process immediately (`docs/24`: `abort(): never`). Skips unwinding
/// and finalizers — the hard stop.
#[unsafe(no_mangle)]
pub extern "C" fn lang_abort() -> ! {
    std::process::abort();
}
