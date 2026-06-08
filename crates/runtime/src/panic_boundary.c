/*
 * Worker-panic isolation boundary (`docs/14`, `docs/20` §1, `docs/21` §11).
 *
 * A language `panic` raised inside generated (Cranelift) code cannot be caught
 * with Rust's unwinder: Cranelift frames carry no DWARF unwind tables, so a
 * Rust `panic!`/`catch_unwind` would have to walk frames it cannot describe.
 * `setjmp`/`longjmp`, by contrast, restore the saved register context directly
 * — they never walk intervening frames — so they cross the C ABI / Cranelift
 * boundary soundly. The cost is that no destructors run for the abandoned
 * frames, which exactly matches the documented contract ("Panicked thread:
 * stack roots released; no Drop calls while abandoning frames", `docs/16`); the Rust
 * side restores GC/lock invariants explicitly at the boundary instead.
 *
 * The boundary is installed once per worker OS thread, around the call into
 * generated code. `lang_panic` (Rust) checks `otter_pb_active()` and, on a
 * worker, `longjmp`s back here instead of terminating the process.
 */

#include <setjmp.h>
#include <stddef.h>

/*
 * The active boundary's jump buffer for the calling thread, or NULL when no
 * boundary is installed (e.g. the main thread, where a panic terminates the
 * program). Thread-local so each worker has its own landing pad.
 */
static _Thread_local jmp_buf *otter_pb_current = NULL;

/* Whether a panic boundary is installed on the calling thread. */
int otter_pb_active(void) {
    return otter_pb_current != NULL;
}

/*
 * Run `body(ctx)` under a fresh panic boundary. Returns 0 if `body` returned
 * normally, or 1 if a `panic` `longjmp`ed back out of it. Boundaries nest: the
 * enclosing boundary (if any) is saved and restored, so a panic inside a worker
 * that is itself awaiting a nested worker lands at the innermost boundary.
 *
 * `prev` is `volatile` because it is live across `setjmp`: after a `longjmp`
 * the non-volatile automatic-storage rule (C11 7.13.2.1p3) would otherwise
 * leave it indeterminate. `buf` is the landing pad and stays in this frame,
 * which remains live for the whole call.
 */
int otter_pb_run(void (*body)(void *), void *ctx) {
    jmp_buf buf;
    jmp_buf *volatile prev = otter_pb_current;
    if (setjmp(buf) != 0) {
        otter_pb_current = prev;
        return 1;
    }
    otter_pb_current = &buf;
    body(ctx);
    otter_pb_current = prev;
    return 0;
}

/*
 * Unwind to the active panic boundary on this thread. Only called by
 * `lang_panic` after it has confirmed `otter_pb_active()` and captured the
 * panic message, so `otter_pb_current` is non-NULL. Never returns.
 */
void otter_pb_longjmp(void) {
    longjmp(*otter_pb_current, 1);
}
