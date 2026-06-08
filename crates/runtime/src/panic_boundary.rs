//! Worker-panic isolation (`docs/14`, `docs/20` §1, `docs/21` §11).
//!
//! A language `panic` inside a spawned worker must fail only that worker — it
//! surfaces as `Panicked { message }` to the joiner (or re-propagates at the
//! awaiter of a `spawn EXPR`) — instead of aborting the whole process. Because
//! generated (Cranelift) code carries no unwind tables, Rust's `catch_unwind`
//! cannot cross it; we use a `setjmp`/`longjmp` boundary instead (see
//! `panic_boundary.c`). [`lang_panic`](crate::lang_panic) checks
//! [`boundary_active`] and, on a worker thread, captures the message and
//! `longjmp`s back to the boundary installed at the worker's entry. The
//! `longjmp` abandons the worker's in-flight frames without running their
//! destructors — exactly the documented "no Drop calls while abandoning frames"
//! contract (`docs/16`) — so the boundary explicitly restores the two
//! invariants that matter: held `Shared` locks are released and transient GC
//! pins are dropped (see [`run_under_boundary`]).
//!
//! The main thread installs no boundary, so a panic there falls through to the
//! process-terminating path (exit 101), preserving top-level panic semantics.

use crate::gc;
use crate::strings::lang_str_from_utf8;
use std::cell::RefCell;

unsafe extern "C" {
    /// Run `body(ctx)` under a fresh panic boundary; returns 0 on normal return,
    /// 1 if a `panic` `longjmp`ed back. Defined in `panic_boundary.c`.
    fn otter_pb_run(body: extern "C" fn(*mut u8), ctx: *mut u8) -> i32;
    /// Whether a panic boundary is installed on the calling thread.
    fn otter_pb_active() -> i32;
    /// Unwind to the active boundary on this thread. Never returns.
    fn otter_pb_longjmp();
}

thread_local! {
    /// The message of the panic currently unwinding to this thread's boundary.
    /// Set by [`capture_panic`] just before the `longjmp`, taken by
    /// [`run_under_boundary`] once control lands at the boundary.
    static PANIC_MSG: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Whether the calling thread has an installed panic boundary (i.e. it is a
/// spawned worker, not the main thread).
pub fn boundary_active() -> bool {
    unsafe { otter_pb_active() != 0 }
}

/// Capture a panicking worker's message and unwind to its boundary. Called by
/// [`lang_panic`](crate::lang_panic) only when [`boundary_active`]; never
/// returns. The message bytes are copied out *before* the `longjmp` so they do
/// not depend on the abandoned `LangStr` object (whose stack roots vanish with
/// the unwound frames).
///
/// # Safety
/// `bytes` must be the panic message's UTF-8 bytes, valid for the call.
pub unsafe fn capture_panic(bytes: &[u8]) -> ! {
    PANIC_MSG.with(|m| *m.borrow_mut() = Some(bytes.to_vec()));
    unsafe { otter_pb_longjmp() };
    // `otter_pb_longjmp` never returns; this satisfies the `!` return type.
    unreachable!("otter_pb_longjmp returned")
}

/// The thunk handed to the C boundary: invoke the boxed worker body stored in
/// `ctx`, writing its `i64` result back. A panic inside the body `longjmp`s
/// past this frame straight to the boundary, so the assignment is skipped — the
/// result is read only on the normal-return path.
extern "C" fn trampoline(ctx: *mut u8) {
    // SAFETY: `ctx` points at the `Ctx` built in `run_under_boundary`, live for
    // the whole `otter_pb_run` call.
    let cx = unsafe { &mut *(ctx as *mut Ctx) };
    cx.result = (cx.body)();
}

/// Boundary context: the worker body plus a slot for its result.
struct Ctx<'a> {
    body: &'a mut dyn FnMut() -> i64,
    result: i64,
}

/// Run a worker `body` under this thread's panic boundary.
///
/// Returns `Ok(result)` if `body` completed normally, or `Err(message)` — a
/// GC-pinned `str` pointer (cast to `usize`) — if it panicked. On the panic
/// path the worker's normal cleanup was skipped by the `longjmp`, so this first
/// restores the invariants the spec requires after an unwind:
/// * every `Shared` lock the task still holds is released (no poisoning,
///   `docs/20` §4) via [`crate::shared::lang_shared_release_all`];
/// * every transient cross-`poll` GC pin is dropped via
///   [`gc::release_unwind_pins`], so the worker's abandoned objects become
///   collectable rather than pinned forever.
///
/// It then materialises the captured message as a managed `str` and pins it for
/// the cross-thread handoff to the joiner.
pub fn run_under_boundary(mut body: impl FnMut() -> i64) -> Result<i64, usize> {
    let mut cx = Ctx {
        body: &mut body,
        result: 0,
    };
    let panicked = unsafe { otter_pb_run(trampoline, &mut cx as *mut Ctx as *mut u8) };
    if panicked == 0 {
        return Ok(cx.result);
    }
    // --- panic path: the longjmp abandoned the worker's frames -------------
    gc::release_unwind_pins();
    unsafe { crate::shared::lang_shared_release_all() };
    Err(build_panic_message())
}

/// Build the `Panicked.message` `str` from the captured bytes (falling back to
/// a generic message if none was recorded), pin it as a global GC root for the
/// cross-thread handoff, and return its pointer as a `usize`. The joiner unpins
/// it once it has been copied into the traced `Panicked` box.
fn build_panic_message() -> usize {
    let bytes = PANIC_MSG
        .with(|m| m.borrow_mut().take())
        .unwrap_or_else(|| b"worker panicked".to_vec());
    // Building the `str` allocates; pause collection so the fresh object cannot
    // be swept before it is pinned, matching the runtime's alloc-then-pin idiom.
    gc::pause();
    let s = unsafe { lang_str_from_utf8(bytes.as_ptr(), bytes.len()) } as usize;
    gc::resume_with_return_root(s);
    gc::add_extra_root(s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strings::LangStr;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn normal_body_returns_ok_and_no_boundary_outside() {
        let _g = gc::TEST_LOCK.lock().unwrap();
        assert!(!boundary_active(), "no boundary before any run");
        let out = run_under_boundary(|| 42);
        assert_eq!(out, Ok(42));
        assert!(!boundary_active(), "boundary uninstalled after run");
    }

    #[test]
    fn boundary_is_active_inside_body() {
        let _g = gc::TEST_LOCK.lock().unwrap();
        static INSIDE: AtomicU32 = AtomicU32::new(0);
        let _ = run_under_boundary(|| {
            INSIDE.store(boundary_active() as u32, Ordering::SeqCst);
            0
        });
        assert_eq!(INSIDE.load(Ordering::SeqCst), 1, "boundary active in body");
    }

    #[test]
    fn longjmp_from_body_reports_panic() {
        // The boundary builds a managed message; serialize against other
        // heap-touching tests and reset the heap around it.
        let _g = gc::TEST_LOCK.lock().unwrap();
        unsafe { gc::free_all() };
        // Simulate a `lang_panic` deep in the body: capture a message and
        // longjmp. `run_under_boundary` must catch it and return `Err`.
        let out = run_under_boundary(|| {
            unsafe { capture_panic(b"kaboom") };
        });
        match out {
            Err(msg) => {
                let s = msg as *const LangStr;
                let bytes = unsafe { crate::strings::str_bytes(s) };
                assert_eq!(bytes, b"kaboom");
                // Unpin to leave the test heap tidy.
                gc::remove_extra_root(msg);
            }
            Ok(_) => panic!("expected the longjmp to be caught as a panic"),
        }
        unsafe { gc::free_all() };
    }

    #[test]
    fn nested_boundaries_catch_at_innermost() {
        let _g = gc::TEST_LOCK.lock().unwrap();
        unsafe { gc::free_all() };
        // An inner panic must not escape past the inner boundary to the outer.
        let outer = run_under_boundary(|| {
            let inner = run_under_boundary(|| unsafe { capture_panic(b"inner") });
            assert!(inner.is_err(), "inner boundary catches the inner panic");
            if let Err(m) = inner {
                gc::remove_extra_root(m);
            }
            7
        });
        assert_eq!(outer, Ok(7), "outer body completes normally");
        unsafe { gc::free_all() };
    }
}
