    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::span::FileId;

    fn check(src: &str) -> Vec<SemaError> {
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
    }

    #[test]
    fn non_bool_condition_rejected() {
        let errs = check("function f() { if 1 { } }");
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::NonBoolCondition { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::ArgCount { .. })));
    }

    #[test]
    fn call_arg_type_mismatch() {
        let errs = check(
            "function add(a: i64, b: i64): i64 { a + b }\n\
             function main() { add(1, true); }",
        );
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::UnknownValue { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::InvalidCast { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
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
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("propagate"))));
    }

    #[test]
    fn match_exhaustive_union_ok() {
        assert_ok(
            "function f(x: i64 | str): i64 { match x { i64 n => n, str s => 0 } }",
        );
    }

    #[test]
    fn match_non_exhaustive_union_errors() {
        let errs = check("function f(x: i64 | str | bool): i64 { match x { i64 n => n, str s => 0 } }");
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("non-exhaustive"))));
    }

    #[test]
    fn match_wildcard_is_exhaustive() {
        assert_ok("function f(n: i64): i64 { match n { 0 => 1, _ => 2 } }");
    }

    #[test]
    fn match_guard_must_be_bool() {
        let errs = check("function f(n: i64): i64 { match n { i64 x if 1 => x, _ => 0 } }");
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::NonBoolCondition { .. })));
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
        let baked_widen = ck.results.node_hir.values().any(|e| {
            matches!(&e.kind, crate::hir::ExprKind::Adjust {
                adjust: crate::sema::results::Adjust::Widen(_), ..
            })
        });
        assert!(baked_widen, "expected a widening `Adjust` baked into the HIR");
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
    }

    #[test]
    fn unknown_method_errors() {
        let errs = check(
            "struct P { x: i64 }\n\
             function f() { var p = P { x: 1 }; p.nope(); }",
        );
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("no method"))));
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
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("missing field"))));
    }

    #[test]
    fn struct_unknown_field_errors() {
        let errs = check("struct P { x: i64 }\nfunction f() { var p = P { x: 1, z: 2 }; }");
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("no field"))));
    }

    #[test]
    fn struct_field_wrong_type_errors() {
        let errs = check("struct P { x: i64 }\nfunction f() { var p = P { x: true }; }");
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::Message(_))));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
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
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::TypeMismatch { .. })));
    }

    #[test]
    fn async_fn_must_return_future() {
        let errs = check("function f(): i64 async { 42 }");
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("Future"))));
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
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("await"))));
    }

    #[test]
    fn await_non_future_errors() {
        let errs = check("function f(): Future<i64> async { var x: i64 = await 5; x }");
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("Future"))));
    }

    #[test]
    fn forgot_to_await_lint() {
        let errs = check(
            "function inner(): Future<i64> async { 1 }\n\
             function f(): Future<i64> async { inner(); 0 }",
        );
        assert!(errs.iter().any(|e| matches!(&e.kind, SemaErrorKind::Message(m) if m.contains("never used"))));
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
