use super::*;
use crate::lexer::lex;
use crate::parser::parse;
use crate::span::FileId;

/// A broad import of the toolchain surface, prepended to every test program
/// so the near-empty-prelude rule (`docs/17` §17.8) is satisfied without
/// hand-editing each inline program. Importing an unused name is harmless
/// (no unused-import lint); a program's own definition silently shadows an
/// import (`docs/17` §17.9).
const PRELUDE: &str = "import { List, Map, Set, Entry } from \"core:collections\";\n\
        import { print, println } from \"std:io\";\n\
        import { panic, panic_with, exit, abort } from \"core:prelude\";\n\
        import { Clone, ToStr, Eq, Ord, Hash, Iterator, Item, Done, Try, FromResidual, Drop, Future, Ready, Pending, Context } from \"core:prelude\";\n\
        import { Shared, LockBusy, Sender, Receiver, ChannelClosed, MpmcSender, MpmcReceiver, channel, channel_bounded, channel_mpmc, channel_mpmc_bounded } from \"std:sync\";\n\
        import { Thread, JoinHandle, Joined, Panicked } from \"std:thread\";\n\
        import { AsyncIterator, TimedOut, yield_now, sleep, timeout } from \"std:async\";\n\
        import { Foreign, CString, CStr, Buffer } from \"core:ffi\";\n";

fn check(src: &str) -> Vec<SemaError> {
    let src = &format!("{PRELUDE}{src}");
    let src = src.as_str();
    let (tokens, le) = lex(src, FileId(0));
    assert!(le.is_empty(), "lex: {le:?}");
    let (module, pe) = parse(src, &tokens);
    assert!(pe.is_empty(), "parse: {pe:?}");
    let prog = Program::collect(&module);
    assert!(prog.errors.is_empty(), "collect: {:?}", prog.errors);
    let mut tcx = TyCtxt::new();
    let mut errors = Vec::new();
    let mut ck = Checker::new(&prog, &mut tcx, &mut errors);
    ck.check_program();
    errors
}

fn assert_ok(src: &str) {
    let errs = check(src);
    assert!(errs.is_empty(), "unexpected errors: {errs:?}");
}

#[test]
fn arithmetic_ok() {
    assert_ok("function main() { var x: i64 = 40 + 2; }");
}

#[test]
fn int_literal_defaults_to_i64() {
    assert_ok("function f(): i64 { 42 }");
}

#[test]
fn int_suffix_respected() {
    assert_ok("function f(): u8 { 42u8 }");
}

#[test]
fn int_overflow_for_type_is_error() {
    let errs = check("function f(): i8 { 300 }");
    assert!(!errs.is_empty());
}

#[test]
fn type_mismatch_is_reported() {
    let errs = check("function f(): i64 { true }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn non_bool_condition_rejected() {
    let errs = check("function f() { if 1 { } }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::NonBoolCondition { .. }))
    );
}

#[test]
fn comparison_yields_bool() {
    assert_ok("function f(): bool { 1 < 2 }");
}

#[test]
fn if_else_unions_branch_types() {
    // Result is i64 | str; assignable to that declared return.
    assert_ok("function f(c: bool): i64 | str { if c { 1 } else { \"x\" } }");
}

#[test]
fn direct_call_checks_arguments() {
    assert_ok(
        "function add(a: i64, b: i64): i64 { a + b }\n\
             function main() { var r: i64 = add(1, 2); }",
    );
}

#[test]
fn call_arg_count_mismatch() {
    let errs = check(
        "function add(a: i64, b: i64): i64 { a + b }\n\
             function main() { add(1); }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::ArgCount { .. }))
    );
}

#[test]
fn call_arg_type_mismatch() {
    let errs = check(
        "function add(a: i64, b: i64): i64 { a + b }\n\
             function main() { add(1, true); }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn widening_to_union_ok() {
    assert_ok("function f(): i64 | str { var x: i64 = 1; x }");
}

#[test]
fn widening_to_dynamic_ok() {
    assert_ok("function f(): dynamic { 42 }");
}

#[test]
fn unknown_value_errors() {
    let errs = check("function f(): i64 { nope }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::UnknownValue { .. }))
    );
}

#[test]
fn print_builtin_accepts_str() {
    assert_ok("function main() { print(\"hi\") }");
    assert_ok("function main() { println(42 as str) }");
}

#[test]
fn num_to_str_cast_ok() {
    assert_ok("function f(): str { 42 as str }");
    assert_ok("function f(c: char): str { c as str }");
}

#[test]
fn union_narrowing_cast_ok() {
    assert_ok("function f(x: i64 | str): i64 { x as i64 }");
}

#[test]
fn invalid_cast_reported() {
    let errs = check("function f(): i64 { \"hi\" as i64 }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::InvalidCast { .. }))
    );
}

#[test]
fn is_yields_bool() {
    assert_ok("function f(x: i64 | str): bool { x is i64 }");
}

#[test]
fn generic_call_infers_and_checks() {
    assert_ok("function id<T>(x: T): T { x }\nfunction f(): i64 { id(42) }");
    assert_ok("function id<T>(x: T): T { x }\nfunction f(): str { id(\"hi\") }");
}

#[test]
fn generic_explicit_args_ok() {
    assert_ok("function id<T>(x: T): T { x }\nfunction f(): i64 { id<i64>(42) }");
}

#[test]
fn generic_return_substituted() {
    // The result of `id(true)` is `bool`, so it can't satisfy `i64`.
    let errs = check("function id<T>(x: T): T { x }\nfunction f(): i64 { id(true) }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn try_operator_ok() {
    assert_ok(
        "function parse(ok: bool): i64 | str { if ok { 1 } else { \"e\" } }\n\
             function f(ok: bool): str { var n: i64 = parse(ok)?; \"ok\" }",
    );
}

#[test]
fn try_nothing_to_propagate_errors() {
    // `parse` returns only `i64`, which the `str`-returning function can't
    // propagate — `?` has nothing to do.
    let errs = check(
        "function parse(): i64 { 1 }\n\
             function f(): str { var n: i64 = parse()?; \"ok\" }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("propagate")))
    );
}

#[test]
fn match_exhaustive_union_ok() {
    assert_ok("function f(x: i64 | str): i64 { match x { i64 n => n, str s => 0 } }");
}

#[test]
fn match_non_exhaustive_union_errors() {
    let errs = check("function f(x: i64 | str | bool): i64 { match x { i64 n => n, str s => 0 } }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::NonExhaustiveMatch { .. }))
    );
}

#[test]
fn match_wildcard_is_exhaustive() {
    assert_ok("function f(n: i64): i64 { match n { 0 => 1, _ => 2 } }");
}

#[test]
fn match_guard_must_be_bool() {
    let errs = check("function f(n: i64): i64 { match n { i64 x if 1 => x, _ => 0 } }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::NonBoolCondition { .. }))
    );
}

#[test]
fn union_widen_and_narrow_ok() {
    assert_ok("function f(): i64 { var x: i64 | str = 1; x as i64 }");
    assert_ok("function f(): i64 | null { if true { 1 } else { null } }");
}

#[test]
fn union_records_widen_adjustment() {
    // `var x: i64 | str = 1` widens the i64 literal — the checker bakes a
    // `Widen` `Adjust` directly onto the HIR node (was the `adjustments`
    // side table) so codegen boxes it.
    let src = "function f() { var x: i64 | str = 1; }";
    let (tokens, _) = lex(src, FileId(0));
    let (module, _) = parse(src, &tokens);
    let prog = Program::collect(&module);
    let mut tcx = TyCtxt::new();
    let mut errors = Vec::new();
    let mut ck = Checker::new(&prog, &mut tcx, &mut errors);
    ck.check_program();
    let baked_widen = ck.node_hir.values().any(|e| {
        matches!(
            &e.kind,
            crate::hir::ExprKind::Adjust {
                adjust: crate::sema::results::Adjust::Widen(_),
                ..
            }
        )
    });
    assert!(
        baked_widen,
        "expected a widening `Adjust` baked into the HIR"
    );
}

#[test]
fn method_call_ok() {
    assert_ok(
        "struct P { x: i64 }\n\
             extend P { function get(self): i64 { self.x } }\n\
             function f(): i64 { var p = P { x: 1 }; p.get() }",
    );
}

#[test]
fn method_arg_checked() {
    let errs = check(
        "struct P { x: i64 }\n\
             extend P { function add(self, k: i64): i64 { self.x + k } }\n\
             function f() { var p = P { x: 1 }; p.add(true); }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn unknown_method_errors() {
    let errs = check(
        "struct P { x: i64 }\n\
             function f() { var p = P { x: 1 }; p.nope(); }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::NoMethod { .. }))
    );
}

#[test]
fn self_outside_method_errors() {
    let errs = check("function f(): i64 { self }");
    assert!(!errs.is_empty());
}

#[test]
fn struct_construct_and_field_ok() {
    assert_ok(
        "struct P { x: i64, y: i64 }\n\
             function f(): i64 { var p = P { x: 1, y: 2 }; p.x + p.y }",
    );
}

#[test]
fn struct_missing_field_errors() {
    let errs = check("struct P { x: i64, y: i64 }\nfunction f() { var p = P { x: 1 }; }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::MissingField { .. }))
    );
}

#[test]
fn struct_unknown_field_errors() {
    let errs = check("struct P { x: i64 }\nfunction f() { var p = P { x: 1, z: 2 }; }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::UnknownStructField { .. }))
    );
}

#[test]
fn struct_field_wrong_type_errors() {
    let errs = check("struct P { x: i64 }\nfunction f() { var p = P { x: true }; }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn tuple_struct_and_index_ok() {
    assert_ok(
        "struct Pair(i64, str)\n\
             function f(): i64 { var p = Pair(1, \"x\"); p.0 }",
    );
}

#[test]
fn while_loop_ok() {
    assert_ok("function f() { var i: i64 = 0; while i < 3 { i = i + 1; } }");
}

#[test]
fn loop_break_value_typed() {
    assert_ok("function f(): i64 { loop { break 42 } }");
}

#[test]
fn break_outside_loop_errors() {
    let errs = check("function f() { break }");
    assert!(!errs.is_empty());
}

#[test]
fn while_break_with_value_errors() {
    let errs = check("function f() { while true { break 1 } }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::Message(_)))
    );
}

#[test]
fn continue_outside_loop_errors() {
    let errs = check("function f() { continue }");
    assert!(!errs.is_empty());
}

#[test]
fn return_checks_against_return_type() {
    assert_ok("function f(c: bool): i64 { if c { return 0 } 1 }");
    let errs = check("function f(): i64 { return true }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

// -- async (docs/21) -----------------------------------------------------

#[test]
fn async_fn_body_yields_output() {
    // The body's trailing value is the future's Output, not the Future.
    assert_ok("function f(): Future<i64> async { 42 }");
}

#[test]
fn async_fn_body_output_mismatch_errors() {
    let errs = check("function f(): Future<i64> async { true }");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. }))
    );
}

#[test]
fn async_fn_must_return_future() {
    let errs = check("function f(): i64 async { 42 }");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("Future")))
    );
}

#[test]
fn await_yields_future_output() {
    assert_ok(
        "function inner(): Future<i64> async { 1 }\n\
             function f(): Future<i64> async { var x: i64 = await inner(); x }",
    );
}

#[test]
fn await_outside_async_errors() {
    let errs = check(
        "function inner(): Future<i64> async { 1 }\n\
             function f(): i64 { await inner() }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("await")))
    );
}

#[test]
fn await_non_future_errors() {
    let errs = check("function f(): Future<i64> async { var x: i64 = await 5; x }");
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("Future")))
    );
}

#[test]
fn forgot_to_await_lint() {
    let errs = check(
        "function inner(): Future<i64> async { 1 }\n\
             function f(): Future<i64> async { inner(); 0 }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("never used")))
    );
}

#[test]
fn bound_future_is_not_linted() {
    assert_ok(
        "function inner(): Future<i64> async { 1 }\n\
             function f(): Future<i64> async { var _ = inner(); 0 }",
    );
}

#[test]
fn await_in_async_block_ok() {
    // A bare async { … } block is a zero-arg inline future literal whose
    // Output is its trailing-expression type.
    assert_ok(
        "function inner(): Future<i64> async { 1 }\n\
             function f(): Future<i64> { async { await inner() } }",
    );
}

#[test]
fn async_fallible_await_yields_union() {
    // await of Future<T | E> yields T | E (docs/21 §4).
    assert_ok(
        "function inner(): Future<i64 | str> async { 1 }\n\
             function f(): Future<i64 | str> async { await inner() }",
    );
}

#[test]
fn shared_lock_is_async_awaitable() {
    // `lock` returns `Future<R>`; awaiting it in an async body yields `R`.
    assert_ok(
        "function f(): Future<i64> async {\n\
             \tvar s: Shared<i64> = Shared.new(0);\n\
             \tawait s.lock((c) => c)\n\
             }",
    );
}

#[test]
fn shared_try_lock_is_async_union() {
    // `try_lock` returns `Future<R | LockBusy>`.
    assert_ok(
        "function f(): Future<i64 | LockBusy> async {\n\
             \tvar s: Shared<i64> = Shared.new(0);\n\
             \tawait s.try_lock((c) => c)\n\
             }",
    );
}

#[test]
fn shared_lock_async_body_flattens() {
    // An `async` body's awaited value is the lock's `R` — never a nested
    // `Future` (docs/20 §4 flattening).
    assert_ok(
        "function fetch(): Future<i64> async { 7 }\n\
             function f(): Future<i64> async {\n\
             \tvar s: Shared<i64> = Shared.new(0);\n\
             \tawait s.lock((c) async => { await fetch() })\n\
             }",
    );
}

#[test]
fn shared_lock_in_sync_function_errors() {
    // Locking is await-only; a synchronous context cannot lock.
    let errs = check(
        "function f() {\n\
             \tvar s: Shared<i64> = Shared.new(0);\n\
             \ts.lock((c) => c);\n\
             }",
    );
    assert!(errs.iter().any(|e| matches!(&e.kind,
            SemaErrorKind::Message(m) if m.contains("async") && m.contains("spawn"))));
}

#[test]
fn shared_lock_escape_to_outer_var_rejected() {
    // Detachment rule (docs/20 §4): a live reference may not be stored where
    // it outlives the lock body.
    let errs = check(
        "struct X { v: i64 }\n\
             struct C { x: X }\n\
             function f(): Future<null> async {\n\
             \tvar s: Shared<C> = Shared.new(C { x: X { v: 0 } });\n\
             \tvar escaped: X | null = null;\n\
             \tawait s.lock((c) => { escaped = c.x; 0 });\n\
             }",
    );
    assert!(errs.iter().any(|e| matches!(&e.kind,
            SemaErrorKind::Message(m) if m.contains("escape"))));
}

#[test]
fn shared_lock_escape_into_call_arg_rejected() {
    // Passing a live reference to a call that could retain it is an escape.
    let errs = check(
        "import { List } from \"core:collections\";\n\
             struct X { v: i64 }\n\
             struct C { x: X }\n\
             function f(): Future<null> async {\n\
             \tvar s: Shared<C> = Shared.new(C { x: X { v: 0 } });\n\
             \tvar sink: List<X> = [];\n\
             \tawait s.lock((c) => { sink.push(c.x); 0 });\n\
             }",
    );
    assert!(errs.iter().any(|e| matches!(&e.kind,
            SemaErrorKind::Message(m) if m.contains("escape"))));
}

#[test]
fn shared_lock_clone_escape_hatch_ok() {
    // A `.clone()` detaches — it may leave the lock freely.
    assert_ok(
        "struct X { v: i64 }\n\
             struct C { x: X }\n\
             extend X: Clone { function clone(self): X { X { v: self.v } } }\n\
             function f(): Future<null> async {\n\
             \tvar s: Shared<C> = Shared.new(C { x: X { v: 0 } });\n\
             \tvar escaped: X | null = null;\n\
             \tawait s.lock((c) => { escaped = c.x.clone(); 0 });\n\
             }",
    );
}

#[test]
fn shared_lock_mutating_cell_and_return_ok() {
    // Mutating through the borrow (writing into the cell) and returning a
    // reference (cloned at the boundary by codegen) are both allowed.
    assert_ok(
        "struct X { v: i64 }\n\
             struct C { x: X }\n\
             function f(): Future<X> async {\n\
             \tvar s: Shared<C> = Shared.new(C { x: X { v: 0 } });\n\
             \tawait s.lock((c) => { c.x.v = c.x.v + 1; c.x })\n\
             }",
    );
}

#[test]
fn shared_lock_in_thread_spawn_worker_errors() {
    // A *synchronous* `Thread.spawn` closure cannot lock (docs/20 §1/§4).
    let errs = check(
        "function f(): Future<null> async {\n\
             \tvar s: Shared<i64> = Shared.new(0);\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() => { s.lock((c) => c); 0 });\n\
             \tvar _ = await h.join();\n\
             }",
    );
    assert!(errs.iter().any(|e| matches!(&e.kind,
            SemaErrorKind::Message(m) if m.contains("spawn"))));
}

#[test]
fn async_thread_spawn_worker_yields_awaited_output() {
    // An async `Thread.spawn` closure `() => Future<R>` joins on the awaited
    // `R`, so the handle is `JoinHandle<R>` — not `JoinHandle<Future<R>>`
    // (docs/20 §1). The explicit annotation type-checks only if `R` (here
    // `i64`) was unwrapped from the closure's `Future<i64>` return.
    assert_ok(
        "function f(): Future<null> async {\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() async => { 41 + 1 });\n\
             \tvar _ = await h.join();\n\
             }",
    );
}

#[test]
fn async_thread_spawn_worker_not_future_handle() {
    // The awaited-output rule means `JoinHandle<Future<i64>>` is the wrong
    // annotation for an async worker (docs/20 §1) — it must be a type error.
    let errs = check(
        "function f(): Future<null> async {\n\
             \tvar h: JoinHandle<Future<i64>> = Thread.spawn(() async => { 7 });\n\
             \tvar _ = await h.join();\n\
             }",
    );
    assert!(
        !errs.is_empty(),
        "expected a JoinHandle<R> vs JoinHandle<Future<R>> mismatch"
    );
}

#[test]
fn async_thread_spawn_worker_can_lock() {
    // An *async* `Thread.spawn` worker drives its future with a real
    // executor, so it MAY `await` and lock a `Shared<T>` (docs/20 §1/§4).
    assert_ok(
        "struct C { value: i64 }\n\
             function f(): Future<null> async {\n\
             \tvar state: Shared<C> = Shared.new(C { value: 0 });\n\
             \tvar s: Shared<C> = state.clone();\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() async => {\n\
             \t\tawait s.lock((c) => { c.value = c.value + 1; c.value })\n\
             \t});\n\
             \tvar _ = await h.join();\n\
             }",
    );
}

#[test]
fn async_thread_spawn_worker_can_await() {
    // An async worker may `await` an arbitrary future, like a `spawn` task.
    assert_ok(
        "function fetch(): Future<i64> async { 7 }\n\
             function f(): Future<null> async {\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() async => { await fetch() });\n\
             \tvar _ = await h.join();\n\
             }",
    );
}

#[test]
fn async_thread_spawn_trailing_closure_form() {
    // The documented trailing-closure spelling `Thread.spawn { async => … }`
    // (docs/20 §1) parses as an async worker and yields `JoinHandle<R>`.
    assert_ok(
        "function f(): Future<null> async {\n\
             \tvar h: JoinHandle<i64> = Thread.spawn { async => 7 };\n\
             \tvar _ = await h.join();\n\
            }",
    );
}

#[test]
fn task_spawn_sync_and_async_closures_return_joinhandle() {
    // `Task.spawn` mirrors `Thread.spawn`'s surface, but the backend schedules
    // it on the shared executor. Both sync and async worker closures join on
    // the final `R`.
    assert_ok(
        "import { Task, JoinHandle, Cancelled } from \"std:task\";\n\
             function f(): Future<null> async {\n\
             \tvar a: JoinHandle<i64> = Task.spawn(() => 7);\n\
             \tvar b: JoinHandle<i64> = Task.spawn(() async => { 8 });\n\
             \ta.cancel();\n\
             \tb.abort();\n\
             \tvar r: Joined<i64> | Panicked | Cancelled = await a.join();\n\
             \tvar _ = await b.join();\n\
             }",
    );
}

#[test]
fn task_spawn_rejects_non_shareable_mutable_capture() {
    let errs = check(
        "import { Task, JoinHandle } from \"std:task\";\n\
             struct Counter { n: i64 }\n\
             function f() {\n\
             \tvar c: Counter = Counter { n: 0 };\n\
             \tvar h: JoinHandle<i64> = Task.spawn(() => c.n);\n\
             }",
    );
    assert!(
        errs.iter().any(
            |e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("Task.spawn")
                && m.contains("`Clone` values that can be snapshotted"))
        ),
        "expected Task.spawn mutable-capture rejection, got {errs:?}"
    );
}

#[test]
fn task_spawn_accepts_concrete_clone_capture() {
    assert_ok(
        "import { Task, JoinHandle } from \"std:task\";\n\
             @Derive(Clone)\n\
             struct Counter { n: i64 }\n\
             function f(): Future<null> async {\n\
             \tvar c: Counter = Counter { n: 1 };\n\
             \tvar h: JoinHandle<i64> = Task.spawn(() async => c.n);\n\
             \tvar _ = await h.join();\n\
             }",
    );
}

#[test]
fn task_spawn_accepts_generic_clone_bound_capture() {
    assert_ok(
        "import { Clone, Future } from \"core:prelude\";\n\
             import { Task, JoinHandle } from \"std:task\";\n\
             function f<T: Clone>(value: T): Future<null> async {\n\
             \tvar h: JoinHandle<T> = Task.spawn(() async => value);\n\
             \tvar _ = await h.join();\n\
             }",
    );
}

#[test]
fn thread_joinhandle_has_no_cancel_method() {
    let errs = check(
        "function f() {\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() => 7);\n\
             \th.cancel();\n\
             }",
    );
    assert!(
        errs.iter().any(
            |e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("no method `cancel`"))
        ),
        "expected Thread JoinHandle cancel rejection, got {errs:?}"
    );
}

#[test]
fn thread_joinhandle_has_no_abort_method() {
    let errs = check(
        "function f() {\n\
             \tvar h: JoinHandle<i64> = Thread.spawn(() => 7);\n\
             \th.abort();\n\
             }",
    );
    assert!(
        errs.iter().any(
            |e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("no method `abort`"))
        ),
        "expected Thread JoinHandle abort rejection, got {errs:?}"
    );
}

#[test]
fn ffi_handle_types_typecheck() {
    // `docs/19` §6 / `docs/18` §9: the boundary types' methods have the
    // documented signatures — `CString.from_str: CString`, `as_ptr: *u8`,
    // `to_str: str`, `byte_len: u64`, `as_cstr: CStr`, `CStr.from_ptr: CStr`,
    // `Buffer.alloc: Buffer | null`.
    assert_ok(
        "extern function strlen(s: *u8): u64;\n\
             function main() {\n\
               var cs: CString = CString.from_str(\"x\");\n\
               var n: u64 = strlen(cs.as_ptr());\n\
               var s: str = cs.to_str();\n\
               var bl: u64 = cs.byte_len();\n\
               var v: CStr = cs.as_cstr();\n\
               var w: CStr = CStr.from_ptr(cs.as_ptr());\n\
               var m: Buffer | null = Buffer.alloc(8u64);\n\
             }",
    );
}

#[test]
fn cstring_from_str_yields_owning_handle_not_raw_ptr() {
    // `from_str` now returns an owning `CString`, not a raw `*u8` — assigning
    // it to a `*u8` is a type error (the buffer is owned, freed on Drop).
    let errs = check("function main() { var p: *u8 = CString.from_str(\"x\"); }");
    assert!(
        !errs.is_empty(),
        "expected a type error binding CString to *u8"
    );
}

// -- @Variadic (`docs/19` §13) ------------------------------------------

fn has_msg(errs: &[SemaError], needle: &str) -> bool {
    errs.iter()
        .any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains(needle)))
}

#[test]
fn variadic_call_accepts_extra_promotable_args() {
    // A direct call may pass any number of C-passable args after the fixed
    // prefix (int, double, char, pointer) — no exact-arity error.
    assert_ok(
        "@Variadic\n\
             extern function snprintf(buf: *u8, size: u64, fmt: *u8): i32;\n\
             function main() {\n\
               var b: Buffer = Buffer.alloc(64u64) as Buffer;\n\
               var f: CString = CString.from_str(\"x\");\n\
               var n: i32 = snprintf(b.data, 64u64, f.as_ptr(), 1i32, 2.0f64, 65i32, f.as_ptr());\n\
             }",
    );
}

#[test]
fn variadic_on_ordinary_function_rejected() {
    let errs = check("@Variadic\nfunction f(x: i32): i32 { x }");
    assert!(has_msg(&errs, "only valid on an `extern function`"));
}

#[test]
fn variadic_on_extern_with_body_rejected() {
    let errs = check("@Variadic\nextern function f(fmt: *u8): i32 { 0 }");
    assert!(has_msg(&errs, "not a definition with a body"));
}

#[test]
fn variadic_without_fixed_param_rejected() {
    let errs = check("@Variadic\nextern function f(): i32;");
    assert!(has_msg(&errs, "needs at least one fixed parameter"));
}

#[test]
fn variadic_with_decorator_arg_rejected() {
    let errs = check("@Variadic(\"x\")\nextern function f(fmt: *u8): i32;");
    assert!(has_msg(&errs, "takes no arguments"));
}

#[test]
fn variadic_call_below_fixed_arity_rejected() {
    let errs = check(
        "@Variadic\n\
             extern function snprintf(buf: *u8, size: u64, fmt: *u8): i32;\n\
             function main() { snprintf(); }",
    );
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, SemaErrorKind::ArgCount { expected: 3, .. }))
    );
}

#[test]
fn variadic_str_argument_rejected() {
    let errs = check(
        "@Variadic\n\
             extern function printf(fmt: *u8): i32;\n\
             function main() { var p: CString = CString.from_str(\"%s\"); printf(p.as_ptr(), \"hi\"); }",
    );
    assert!(has_msg(&errs, "cannot be passed as a variadic argument"));
}

#[test]
fn variadic_struct_argument_rejected() {
    let errs = check(
        "struct P { x: i32 }\n\
             @Variadic\n\
             extern function printf(fmt: *u8): i32;\n\
             function main() { var p: CString = CString.from_str(\"%d\"); printf(p.as_ptr(), P { x: 1 }); }",
    );
    assert!(has_msg(&errs, "cannot be passed as a variadic argument"));
}

#[test]
fn variadic_transparent_newtype_argument_ok() {
    // A @Transparent scalar wrapper is passable by its inner representation.
    assert_ok(
        "@Transparent\n\
             struct Cents(i32)\n\
             @Variadic\n\
             extern function snprintf(buf: *u8, size: u64, fmt: *u8): i32;\n\
             function main() {\n\
               var b: Buffer = Buffer.alloc(64u64) as Buffer;\n\
               var f: CString = CString.from_str(\"%d\");\n\
               var n: i32 = snprintf(b.data, 64u64, f.as_ptr(), Cents(5i32));\n\
             }",
    );
}
