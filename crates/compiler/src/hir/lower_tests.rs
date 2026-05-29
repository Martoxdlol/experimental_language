//! Stage 2 lowering tests: parse → analyse → lower real programs, then assert
//! the HIR is a *lossless* translation — every node preserves its source span,
//! every expression's type matches the checker's `expr_types` table, and each
//! retired side-table fact lands on the right node.

use super::*;
use crate::lexer::lex;
use crate::parser::parse;
use crate::sema::{analyze, Analysis};
use crate::span::{FileId, Span};

/// Parse + run the full semantic pipeline, asserting a clean program.
fn analyzed(src: &str) -> Analysis {
    let (tokens, le) = lex(src, FileId(0));
    assert!(le.is_empty(), "lex: {le:?}");
    let (module, pe) = parse(src, &tokens);
    assert!(pe.is_empty(), "parse: {pe:?}");
    let a = analyze(&module);
    assert!(a.errors.is_empty(), "analysis errors: {:?}", a.errors);
    a
}

fn lower(src: &str) -> (Analysis, Hir) {
    let a = analyzed(src);
    let hir = lower_program(&a);
    (a, hir)
}

/// The lowered body of the (uniquely-named) user function `name`. Analysis
/// injects a core prelude (≈97 defs, some with bodies), so tests select the
/// body they mean by name rather than by iteration order.
fn body_named<'h>(a: &Analysis, hir: &'h Hir, name: &str) -> &'h Body {
    hir.bodies
        .iter()
        .find(|(def, _)| a.program.def(**def).name == name)
        .map(|(_, b)| b)
        .unwrap_or_else(|| panic!("no lowered body named `{name}`"))
}

// ---------------------------------------------------------------------------
// Recursive HIR walker
// ---------------------------------------------------------------------------

fn walk_block(b: &Block, f: &mut dyn FnMut(&Expr)) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { init, .. } => walk_expr(init, f),
            StmtKind::Assign { target, value } => {
                walk_expr(target, f);
                walk_expr(value, f);
            }
            StmtKind::Expr(e) => walk_expr(e, f),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &b.trailing {
        walk_expr(t, f);
    }
}

fn walk_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Char(_)
        | ExprKind::Name(_)
        | ExprKind::Discard
        | ExprKind::Continue
        | ExprKind::Error => {}
        ExprKind::Str(parts) => {
            for p in parts {
                if let StrPart::Interp { expr, .. } = p {
                    walk_expr(expr, f);
                }
            }
        }
        ExprKind::Tuple(xs) | ExprKind::List(xs) => xs.iter().for_each(|x| walk_expr(x, f)),
        ExprKind::Map(items) => {
            for it in items {
                match it {
                    MapEntry::Kv { key, value } => {
                        walk_expr(key, f);
                        walk_expr(value, f);
                    }
                    MapEntry::Spread(e) => walk_expr(e, f),
                }
            }
        }
        ExprKind::Struct { fields, spread, .. } => {
            for fi in fields {
                walk_expr(&fi.value, f);
            }
            if let Some(s) = spread {
                walk_expr(s, f);
            }
        }
        ExprKind::Field { receiver, .. } | ExprKind::TupleIndex { receiver, .. } => {
            walk_expr(receiver, f)
        }
        ExprKind::Index { receiver, index } => {
            walk_expr(receiver, f);
            walk_expr(index, f);
        }
        ExprKind::Call { args, kind, .. } => {
            if let CallKind::Closure { callee } = kind {
                walk_expr(callee, f);
            }
            args.iter().for_each(|a| walk_expr(a, f));
        }
        ExprKind::Intrinsic { args, .. } => args.iter().for_each(|a| walk_expr(a, f)),
        ExprKind::Unary { operand, .. } => walk_expr(operand, f),
        ExprKind::Binary { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        ExprKind::Cast { expr, .. }
        | ExprKind::Ref(expr)
        | ExprKind::Deref(expr)
        | ExprKind::Adjust { expr, .. } => walk_expr(expr, f),
        ExprKind::Try { expr, .. } | ExprKind::Await { expr, .. } | ExprKind::Spawn { expr, .. } => {
            walk_expr(expr, f)
        }
        ExprKind::If { cond, then_block, else_branch } => {
            walk_expr(cond, f);
            walk_block(then_block, f);
            if let Some(e) = else_branch {
                walk_expr(e, f);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(g, f);
                }
                walk_expr(&a.body, f);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => walk_block(b, f),
        ExprKind::While { cond, body } => {
            walk_expr(cond, f);
            walk_block(body, f);
        }
        ExprKind::For { iter, body, .. } => {
            walk_expr(iter, f);
            walk_block(body, f);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(e) = v {
                walk_expr(e, f);
            }
        }
        ExprKind::Closure { body, .. } => walk_expr(body, f),
        ExprKind::AsyncBlock { body, .. } => walk_block(body, f),
    }
}

/// Visit every expression in every body of a lowered program.
fn for_each_expr(hir: &Hir, f: &mut dyn FnMut(&Expr)) {
    for body in hir.bodies.values() {
        walk_block(&body.block, f);
    }
}

// ---------------------------------------------------------------------------
// Core losslessness invariants
// ---------------------------------------------------------------------------

/// A representative program exercising most expression forms.
const RICH: &str = r#"
struct Point { x: i64, y: i64 }
function dist(p: Point): i64 {
    var sum = p.x * p.x + p.y * p.y;
    if sum > 0 { sum } else { 0 }
}
function classify(n: i64): str {
    var msg = "n";
    var i = 0;
    while i < n {
        i = i + 1;
    }
    msg
}
function main() {
    var p = Point { x: 3, y: 4 };
    var d = dist(p);
    println("done");
}
"#;

#[test]
fn every_node_preserves_a_real_span() {
    let (_a, hir) = lower(RICH);
    let dummy = Span::dummy();
    let mut n = 0;
    for_each_expr(&hir, &mut |e| {
        n += 1;
        assert_ne!(e.span, dummy, "node lost its span: {:?}", e.kind);
        assert!(e.span.lo.0 <= e.span.hi.0, "inverted span on {:?}", e.kind);
    });
    assert!(n > 20, "expected many nodes, saw {n}");
}

#[test]
fn expr_types_match_the_checker_table() {
    let (a, hir) = lower(RICH);
    let mut checked = 0;
    for_each_expr(&hir, &mut |e| {
        // The `Adjust` wrapper holds the post-coercion type; the inner node (at
        // the same span) holds the raw `expr_types` entry. So only assert table
        // equality on a non-adjusted node whose span the checker typed.
        if matches!(e.kind, ExprKind::Adjust { .. }) {
            return;
        }
        if let Some(t) = a.results.expr_ty(e.span) {
            assert_eq!(e.ty, t, "type mismatch at {:?} for {:?}", e.span, e.kind);
            checked += 1;
        }
    });
    assert!(checked > 10, "expected to verify many node types, saw {checked}");
}

#[test]
fn program_level_tables_lowered() {
    let src = r#"
        struct Pair(i64, i64)
        extern function abs(x: i32): i32;
        function id(n: i64): i64 { n }
        function main() { var p = Pair(1, 2); }
    "#;
    let (a, hir) = lower(src);

    // struct layouts and extern sigs mirror the checker tables.
    assert_eq!(hir.structs.len(), a.results.struct_fields.len());
    assert_eq!(hir.extern_sigs.len(), a.results.extern_sigs.len());
    assert!(hir.extern_sigs.values().any(|s| s.params.len() == 1));

    // The user functions each have a lowered signature and body.
    use crate::sema::DefKind;
    for name in ["id", "main"] {
        let def = a
            .program
            .defs
            .iter()
            .enumerate()
            .find(|(_, d)| d.name == name && d.kind == DefKind::Function)
            .map(|(i, _)| crate::ids::DefId(i as u32))
            .unwrap_or_else(|| panic!("no function def named {name}"));
        assert!(hir.sig(def).is_some(), "missing sig for {name}");
        assert!(hir.body(def).is_some(), "missing body for {name}");
    }
}

#[test]
fn function_body_shape_add() {
    let (a, hir) = lower("function add(x: i64, y: i64): i64 { x + y }");
    let i64t = a.tcx.int(crate::ty::IntTy::I64);
    let body = body_named(&a, &hir, "add");
    assert_eq!(body.params.len(), 2);
    assert_eq!(body.ret, i64t);
    // params and their uses are recorded as locals with i64 type.
    for p in &body.params {
        assert_eq!(body.local_ty(*p), Some(i64t));
    }
    // trailing expression is `x + y` — a primitive Add with two Name operands.
    let trailing = body.block.trailing.as_ref().expect("trailing expr");
    match &trailing.kind {
        ExprKind::Binary { op, left, right, overload } => {
            assert_eq!(*op, BinaryOp::Add);
            assert!(overload.is_none());
            assert!(matches!(left.kind, ExprKind::Name(Res::Local(_))));
            assert!(matches!(right.kind, ExprKind::Name(Res::Local(_))));
        }
        other => panic!("expected Binary, got {other:?}"),
    }
}

#[test]
fn literals_are_parsed_to_values() {
    let (_a, hir) = lower(
        r#"function f() {
            var a = 255;
            var b = 0xFF;
            var c = 1.5;
            var d = true;
            var e = 'A';
            var g = '\n';
        }"#,
    );
    let body = body_named(&_a, &hir, "f");
    let mut ints = vec![];
    let mut floats = vec![];
    let mut chars = vec![];
    walk_block(&body.block, &mut |e| match &e.kind {
        ExprKind::Int(v) => ints.push(*v),
        ExprKind::Float(v) => floats.push(*v),
        ExprKind::Char(v) => chars.push(*v),
        _ => {}
    });
    assert!(ints.contains(&255), "decimal 255 parsed: {ints:?}");
    assert!(ints.contains(&0xFF), "hex 0xFF parsed: {ints:?}");
    assert!(floats.iter().any(|f| (*f - 1.5).abs() < 1e-9));
    assert!(chars.contains(&('A' as u32)));
    assert!(chars.contains(&('\n' as u32)));
}

#[test]
fn direct_and_builtin_calls_classified() {
    let (_a, hir) = lower(
        r#"
        function helper(n: i64): i64 { n }
        function main() {
            var x = helper(7);
            println("hi");
        }
        "#,
    );
    let main = body_named(&_a, &hir, "main");
    let mut direct = false;
    let mut builtin = false;
    walk_block(&main.block, &mut |e| {
        if let ExprKind::Call { kind, .. } = &e.kind {
            match kind {
                CallKind::Direct { .. } => direct = true,
                CallKind::Builtin(Builtin::Println) => builtin = true,
                _ => {}
            }
        }
    });
    assert!(direct, "helper(7) should be a Direct call");
    assert!(builtin, "println should be a Builtin call");
}

#[test]
fn struct_literal_resolves_field_indices() {
    let (_a, hir) = lower(
        r#"
        struct Point { x: i64, y: i64 }
        function main() { var p = Point { y: 2, x: 1 }; }
        "#,
    );
    let body = body_named(&_a, &hir, "main");
    let mut found = false;
    walk_block(&body.block, &mut |e| {
        if let ExprKind::Struct { fields, .. } = &e.kind {
            found = true;
            // Even though written `{ y, x }`, indices follow declaration order.
            let x = fields.iter().find(|f| f.name == "x").unwrap();
            let y = fields.iter().find(|f| f.name == "y").unwrap();
            assert_eq!(x.index, 0);
            assert_eq!(y.index, 1);
        }
    });
    assert!(found, "struct literal present");
}

#[test]
fn control_flow_lowers_structurally() {
    let (_a, hir) = lower(
        r#"function f(n: i64): i64 {
            var total = 0;
            while n > 0 {
                total = total + n;
            }
            if total > 10 { return total; }
            total
        }"#,
    );
    let body = body_named(&_a, &hir, "f");
    let mut has_while = false;
    let mut has_if = false;
    let mut has_return = false;
    walk_block(&body.block, &mut |e| match &e.kind {
        ExprKind::While { .. } => has_while = true,
        ExprKind::If { .. } => has_if = true,
        ExprKind::Return(_) => has_return = true,
        _ => {}
    });
    assert!(has_while && has_if && has_return);
}

#[test]
fn var_binding_locals_are_recorded() {
    let (a, hir) = lower("function f(): i64 { var x = 1; var y = x + 1; y }");
    let i64t = a.tcx.int(crate::ty::IntTy::I64);
    let body = body_named(&a, &hir, "f");
    // Two `var` bindings → at least two locals, all i64.
    let bind_count = body
        .block
        .stmts
        .iter()
        .filter(|s| matches!(s.kind, StmtKind::Let { .. }))
        .count();
    assert_eq!(bind_count, 2);
    assert!(body.locals.len() >= 2);
    for (&_id, &ty) in &body.locals {
        assert_eq!(ty, i64t);
    }
}

#[test]
fn empty_program_lowers_to_empty_hir() {
    let (_a, hir) = lower("function main() {}");
    let body = body_named(&_a, &hir, "main");
    assert!(body.block.stmts.is_empty());
    assert!(body.block.trailing.is_none());
    assert!(body.params.is_empty());
}

#[test]
fn link_libs_derived_from_attributes_not_a_side_table() {
    // `@Link` libraries are collected straight from the program's attributes by
    // `collect_link_libs` (Stage 5: the `CheckResults.link_libs` table was
    // retired). De-duplicated, first-seen order, empty names skipped.
    let src = "\
@Link(lib = \"m\")
extern function cos(x: f64): f64;
@Link(\"m\")
extern function sin(x: f64): f64;
@Link(lib = \"z\")
extern function compress(p: i64): i64;
function main() {}
";
    let (a, hir) = lower(src);
    assert_eq!(hir.link_libs, vec!["m".to_string(), "z".to_string()]);
    // The free function agrees with the HIR field.
    assert_eq!(super::collect_link_libs(&a), hir.link_libs);
}

#[test]
fn no_link_attrs_yields_no_libs() {
    let (a, hir) = lower("extern function puts(s: i64): i64;\nfunction main() {}");
    assert!(hir.link_libs.is_empty());
    assert!(super::collect_link_libs(&a).is_empty());
}

#[test]
fn payload_free_builtins_lower_from_shape_not_a_side_table() {
    // Stage 5: `channel`/`yield_now`/`sleep`/`Shared.new` are recognized from
    // the callee shape during lowering (their `CheckResults` marker sets were
    // retired), producing the matching payload-free `Intrinsic` nodes.
    let src = "\
function main() {
  var ch = channel<i64>();
  var sh = Shared.new(0);
  var y = yield_now();
  var z = sleep(1);
}
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let mut seen: Vec<std::mem::Discriminant<Intrinsic>> = Vec::new();
    for stmt in &body.block.stmts {
        if let StmtKind::Let { init, .. } = &stmt.kind {
            walk_expr(init, &mut |e| {
                if let ExprKind::Intrinsic { intrinsic, .. } = &e.kind {
                    seen.push(std::mem::discriminant(intrinsic));
                }
            });
        }
    }
    for want in [
        Intrinsic::ChannelNew,
        Intrinsic::SharedNew,
        Intrinsic::YieldNow,
        Intrinsic::AsyncSleep,
    ] {
        assert!(
            seen.contains(&std::mem::discriminant(&want)),
            "expected {want:?} intrinsic in lowered main; got {} intrinsics",
            seen.len()
        );
    }
}

#[test]
fn num_intrinsics_lower_from_shared_recognition() {
    // `i32.MAX` (a constant, field-position) and `i32.wrapping_add(a,b)` (a
    // method, call-position) both lower to `Intrinsic::Num` via the shared
    // `num_constant_of`/`num_method_of` helpers — the `num_intrinsics` table is
    // gone. A user function `wrapping_add` would not be shadowed here.
    let src = "\
function main(): i64 {
  var hi = i32.MAX;
  var s = i32.wrapping_add(1, 2);
  0
}
";
    use crate::sema::results::NumIntrinsic;
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let (mut bound, mut arith) = (false, false);
    for stmt in &body.block.stmts {
        if let StmtKind::Let { init, .. } = &stmt.kind {
            walk_expr(init, &mut |e| {
                if let ExprKind::Intrinsic { intrinsic: Intrinsic::Num(n), .. } = &e.kind {
                    match n {
                        NumIntrinsic::IntBound { max: true, .. } => bound = true,
                        NumIntrinsic::IntArith { family: 0, op: 0, .. } => arith = true,
                        _ => {}
                    }
                }
            });
        }
    }
    assert!(bound, "expected i32.MAX → IntBound{{max:true}}");
    assert!(arith, "expected i32.wrapping_add → IntArith{{wrapping,add}}");
}

#[test]
fn foreign_alloc_lowers_with_type_from_result_pointer() {
    // `Foreign.alloc<T>()` is recognized by callee shape; `T` is recovered from
    // the `*T | null` result, `zeroed` from the method name (was `foreign_allocs`).
    let src = "\
extern struct Pair { a: i64, b: i64 }
function main() {
  var p = Foreign.alloc<Pair>();
  var z = Foreign.alloc_zeroed<Pair>();
}
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let mut allocs: Vec<bool> = Vec::new();
    for stmt in &body.block.stmts {
        if let StmtKind::Let { init, .. } = &stmt.kind {
            walk_expr(init, &mut |e| {
                if let ExprKind::Intrinsic { intrinsic: Intrinsic::ForeignAlloc { ty, zeroed }, .. } = &e.kind {
                    assert_eq!(
                        a.tcx.display(*ty, &|id| a.program.def(id).name.clone()),
                        "Pair"
                    );
                    allocs.push(*zeroed);
                }
            });
        }
    }
    assert_eq!(allocs, vec![false, true], "alloc then alloc_zeroed");
}

#[test]
fn thread_spawn_lowers_with_output_from_join_handle_type() {
    // `Thread.spawn { … }` is recognized from the callee shape; its output `R`
    // is read from the `JoinHandle<R>` result type (was the `thread_spawns`
    // table). The closure body returns `i64`, so the output is `i64`.
    let src = "function main() { var h = Thread.spawn(() => 7); }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let mut found = None;
    for stmt in &body.block.stmts {
        if let StmtKind::Let { init, .. } = &stmt.kind {
            walk_expr(init, &mut |e| {
                if let ExprKind::Intrinsic { intrinsic: Intrinsic::ThreadSpawn { output }, .. } = &e.kind {
                    found = Some(*output);
                }
            });
        }
    }
    let out = found.expect("expected a ThreadSpawn intrinsic");
    assert_eq!(a.tcx.display(out, &|id| a.program.def(id).name.clone()), "i64");
}

#[test]
fn collection_ctors_lower_to_payload_free_intrinsic() {
    // `List<T>()` / `Map<K,V>()` are recognized by the callee's type name (the
    // collection type rides on the node), so they lower to `CollectionCtor`
    // without the retired `builtin_ctors` table.
    let src = "\
function main() {
  var xs = List<i64>();
  var m = Map<str, i64>();
}
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let mut ctors = 0;
    for stmt in &body.block.stmts {
        if let StmtKind::Let { init, .. } = &stmt.kind {
            walk_expr(init, &mut |e| {
                if matches!(&e.kind, ExprKind::Intrinsic { intrinsic: Intrinsic::CollectionCtor, .. }) {
                    ctors += 1;
                }
            });
        }
    }
    assert_eq!(ctors, 2, "expected both List and Map ctors to lower to CollectionCtor");
}

#[test]
fn shadowed_builtin_name_is_a_normal_call_not_an_intrinsic() {
    // If the user defines their own `sleep`, the checker resolves the call to
    // it (recording a resolution), so lowering must NOT treat it as the builtin.
    let src = "\
function sleep(ms: i64): i64 { ms }
function main(): i64 { sleep(5) }
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "main");
    let trailing = body.block.trailing.as_ref().expect("trailing call");
    match &trailing.kind {
        ExprKind::Call { kind: CallKind::Direct { .. }, .. } => {}
        other => panic!("expected a direct call to user `sleep`, got {other:?}"),
    }
}

// --- lock the lowering of the remaining table-backed forms (Stage 5) ---------
// These HIR-shape assertions pin the desugaring of the forms whose recognition
// still flows through `CheckResults` tables (clone, static dispatch, the
// `Iterator` `for` driver, `?`, `await`, operator overloads, coercions, match
// patterns). They are the safety net for the eventual checker-constructs-HIR
// refactor that retires those tables: the HIR these forms lower to must not
// change when their source moves from a side table onto the checker's nodes.

/// Find the first HIR expression in `name`'s body matching `pred`.
fn find_expr<'h>(a: &Analysis, hir: &'h Hir, name: &str, mut pred: impl FnMut(&Expr) -> bool) -> bool {
    let body = body_named(a, hir, name);
    let mut hit = false;
    for stmt in &body.block.stmts {
        match &stmt.kind {
            StmtKind::Let { init, .. } => walk_expr(init, &mut |e| if pred(e) { hit = true }),
            StmtKind::Expr(e) | StmtKind::Assign { value: e, .. } => {
                walk_expr(e, &mut |e| if pred(e) { hit = true })
            }
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &body.block.trailing {
        walk_expr(t, &mut |e| if pred(e) { hit = true });
    }
    hit
}

#[test]
fn checker_builds_hir_leaf_nodes_directly() {
    // Stage 5 rewrite (in progress): the checker constructs the typed HIR node
    // for leaf expressions as it checks them (`results.node_hir`), and lowering
    // consumes those instead of re-deriving from side tables. Verify the leaves
    // are present and correctly typed/valued.
    let (a, _hir) = lower("function f(): i64 { var b = true; var c = 'A'; 42 }");
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Int(42))),
        "checker should have built the `42` literal node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Bool(true))),
        "checker should have built the `true` literal node"
    );
    assert!(
        nh.values().any(|e| matches!(&e.kind, ExprKind::Char(c) if *c == 'A' as u32)),
        "checker should have built the `'A'` literal node"
    );
}

#[test]
fn checker_builds_hir_recursive_nodes_directly() {
    // Stage 5: composite expressions whose every child is already checker-built
    // are themselves built during checking (Cast/Ref/Deref/TupleIndex/Index/Tuple).
    let (a, _hir) = lower(
        "function f(): i64 { var t = (1, 2); var xs = [10, 20]; var i = (3 as i64); t.0 + xs[1] + i }",
    );
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Cast { .. })),
        "checker should have built the `3 as i64` cast node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Index { .. })),
        "checker should have built the `xs[1]` index node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::TupleIndex { index: 0, .. })),
        "checker should have built the `t.0` tuple-index node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Tuple(_))),
        "checker should have built the `(1, 2)` tuple node"
    );
}

#[test]
fn checker_builds_hir_aggregate_nodes_directly() {
    // Stage 5: string interpolation, map literals, struct literals and field
    // access are checker-built once their sub-expressions are migrated.
    let (a, _hir) = lower(
        "struct P { x: i64, y: i64 }\n\
         function f(): i64 { var p = P { x: 1, y: 2 }; var m = { \"a\": p.x }; var s = \"v=${p.x}\"; p.y }",
    );
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Struct { .. })),
        "checker should have built the `P {{ .. }}` struct literal node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Map(_))),
        "checker should have built the map literal node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Str(_))),
        "checker should have built the interpolated string node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Field { .. })),
        "checker should have built the `p.x` field-access node"
    );
}

#[test]
fn checker_builds_hir_call_nodes_directly() {
    // Stage 5: call dispatch (direct calls, builtin methods, intrinsics) is
    // classified by the checker into the same `Call`/`Intrinsic` HIR variants
    // that `lower_call` produced from side tables.
    let (a, _hir) = lower(
        "function g(n: i64): i64 { n + 1 }\n\
         function f(): i64 { var xs = [1, 2]; xs.push(3); g(xs.size()) }",
    );
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(
            &e.kind,
            ExprKind::Call { kind: CallKind::Direct { .. }, .. }
        )),
        "checker should have built the direct call `g(..)`"
    );
    assert!(
        nh.values().any(|e| matches!(
            &e.kind,
            ExprKind::Call { kind: CallKind::BuiltinMethod { .. }, .. }
        )),
        "checker should have built the builtin method call `xs.push(..)`/`xs.len()`"
    );
}

#[test]
fn checker_builds_hir_control_flow_directly() {
    // Stage 5: control-flow expressions (and the blocks/patterns they contain)
    // are built by the checker — `if`, `match`, `while`, `for`, and bare blocks.
    let (a, _hir) = lower(
        "function f(xs: List<i64>): i64 {\n\
         var total = 0;\n\
         for x in xs { total = total + x; }\n\
         var n = if total > 0 { total } else { 0 };\n\
         var c = 0;\n\
         while c < n { c = c + 1; }\n\
         match n { 0 => 0, _ => c }\n\
         }",
    );
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::For { .. })),
        "checker should have built the `for` loop node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::If { .. })),
        "checker should have built the `if` node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::While { .. })),
        "checker should have built the `while` node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Match { .. })),
        "checker should have built the `match` node"
    );
}

#[test]
fn checker_builds_hir_closure_and_async_block_directly() {
    // Stage 5: closures and `async { … }` blocks are checker-built, carrying the
    // capture/param/output info the checker computed (was `closures` /
    // `async_blocks` consulted only at lowering time).
    let (a, _hir) = lower(
        "function f(): i64 { var add = (x: i64): i64 => x + 1; add(41) }\n\
         function g(): Future<i64> { async { 7 } }",
    );
    let nh = &a.results.node_hir;
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::Closure { .. })),
        "checker should have built the closure node"
    );
    assert!(
        nh.values().any(|e| matches!(e.kind, ExprKind::AsyncBlock { .. })),
        "checker should have built the `async` block node"
    );
}

#[test]
fn retired_tables_data_lives_on_hir_nodes() {
    // The `operator_methods` / `cast_targets` tables are deleted from
    // `CheckResults`; their data now flows through transient slots onto the HIR
    // node fields. Assert the nodes still carry it (a regression guard for the
    // transient hand-off — a misordered slot would drop these).
    let src = "\
struct V { x: i64 }
extend V { function add(self, o: V): V { V { x: self.x + o.x } } }
function f(a: V, b: V): i64 { var c = a + b; (c.x as i64) }
";
    let (a, hir) = lower(src);
    // The overloaded `a + b` carries its resolved method on the Binary node.
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Binary { overload: Some(_), .. }
        )),
        "overloaded `+` must carry its OpOverload on the Binary node"
    );
    // The `c.x as i64` cast carries its resolved target on the Cast node.
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(&e.kind, ExprKind::Cast { .. })),
        "the `as i64` cast node must be present with its target"
    );
}

#[test]
fn retired_call_and_for_tables_data_lives_on_hir_nodes() {
    // The `clone_kinds` / `static_calls` / `static_recv` / `for_iters` tables are
    // deleted; their data now reaches the HIR via transient slots. Assert the
    // nodes still carry it (regression guard for the transient hand-off).
    let src = "\
struct B { v: i64 }
extend B { function make(n: i64): B { B { v: n } } }
function f(): i64 {
  var xs = [1, 2, 3];
  var ys = xs.clone();
  var b = B.make(7);
  var total = 0;
  for x in ys { total = total + x; }
  total + b.v
}
";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Intrinsic { intrinsic: Intrinsic::Clone(_), .. }
        )),
        "`xs.clone()` must lower to a Clone intrinsic (was `clone_kinds`)"
    );
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Call { kind: CallKind::Method { is_static: true, .. }, .. }
        )),
        "`B.make(7)` must be a static method call (was `static_calls`/`static_recv`)"
    );
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::For { driver: ForDriver::ListFast { .. }, .. }
        )),
        "`for x in ys` must carry its List-fast driver (was `for_iters`/`for_maps`)"
    );
}

#[test]
fn retired_stringify_and_type_args_tables_data_lives_on_hir_nodes() {
    // `stringify_methods` + `call_type_args` are deleted; the per-hole `to_str`
    // methods (via a per-hole transient deque) and generic-call type args (via a
    // transient) now reach the HIR `Str`/`Call` nodes. Assert both.
    let src = "\
struct P { x: i64 }
extend P { function to_str(self): str { \"P!\" } }
function id<T>(v: T): T { v }
function f(): str { var p = P { x: 1 }; var n = id(5); \"n=${n} p=${p}\" }
";
    let (a, hir) = lower(src);
    // The interpolated string: the `n` hole (i64) needs no to_str; the `p` hole
    // (user type) carries its `to_str` method — order preserved per hole.
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(&e.kind, ExprKind::Str(parts)
            if parts.iter().filter(|p| matches!(p, StrPart::Interp { stringify: Some(_), .. })).count() == 1
            && parts.iter().filter(|p| matches!(p, StrPart::Interp { stringify: None, .. })).count() == 1)),
        "the `${{p}}` hole must carry a to_str method and the `${{n}}` hole must not"
    );
    // The generic call `id(5)` carries its solved type args (`[i64]`).
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(&e.kind,
            ExprKind::Call { kind: CallKind::Direct { type_args, .. }, .. } if !type_args.is_empty())),
        "`id(5)` must carry its solved type args (was `call_type_args`)"
    );
}

#[test]
fn resolutions_live_on_hir_name_nodes_not_a_table() {
    // The `resolutions` table is deleted; `results.resolution(span)` reads the
    // resolution off the `Name` HIR node the checker recorded there (value uses,
    // call dispatch, pattern binds all store one). A direct call resolves to its
    // function, a value name to its local.
    let src = "function g(n: i64): i64 { n + 1 }\nfunction f(): i64 { var x = 5; g(x) }";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Call { kind: CallKind::Direct { .. }, .. }
        )),
        "`g(x)` must resolve to a direct call via the HIR (was `resolutions`)"
    );
    // The value name `x` resolves to a local on its HIR `Name` node.
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Name(crate::sema::results::ValueRes::Local(_))
        )),
        "`x` must resolve to a local `Name` node"
    );
}

#[test]
fn pattern_test_type_is_built_into_the_hir_not_a_table() {
    // The `pattern_types` table is deleted; a `TypeBind` pattern's matched
    // variant type is computed by `build_pattern` (via `lower_ty`) and lives on
    // the HIR `Pattern` node's `test_ty`.
    let src = "function f(x: i64 | str): i64 { match x { i64 n => n, str s => 0 } }";
    let (a, hir) = lower(src);
    let mut found = false;
    for_each_expr(&hir, &mut |e| {
        if let ExprKind::Match { arms, .. } = &e.kind {
            for arm in arms {
                if let crate::hir::PatternKind::TypeBind { test_ty, .. } = &arm.pattern.kind {
                    assert!(!a.tcx.is_error(*test_ty), "TypeBind test_ty must be resolved");
                    found = true;
                }
            }
        }
    });
    assert!(found, "expected a TypeBind pattern with a resolved test_ty");
}

#[test]
fn narrowing_unbox_is_baked_into_the_hir_not_a_table() {
    // The `adjustments` table is deleted; a flow-narrowed read bakes its `Unbox`
    // coercion directly onto the HIR `Name` node, recovered structurally from the
    // local's declared (union) type vs the narrowed use type.
    let src = "function f(x: i64 | str): i64 { if x is i64 { x + 1 } else { 0 } }";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Adjust { adjust: crate::sema::results::Adjust::Unbox(_), .. }
        )),
        "the narrowed read of `x` must carry a baked `Unbox` adjust"
    );
}

#[test]
fn checker_emits_whole_function_bodies_directly() {
    // Stage 5: the checker builds each function's entire HIR body `Block` into
    // `results.fn_bodies`; lowering assembles the `Body` from it (no re-lowering
    // of the block/statements/patterns for a well-formed program).
    let src = "\
function f(n: i64): i64 { var t = 0; for i in [1, 2, 3] { t = t + i; } t + n }
function g(): i64 { f(10) }
";
    let (a, hir) = lower(src);
    let f = a.program.defs.iter().position(|d| d.name == "f").map(|i| crate::ids::DefId(i as u32)).unwrap();
    assert!(a.results.fn_bodies.contains_key(&f), "checker should have built f's body block");
    // The HIR body lowering used the checker-built block verbatim.
    let checker_block = &a.results.fn_bodies[&f];
    let hir_body = hir.body(f).expect("f has an HIR body");
    assert_eq!(checker_block.stmts.len(), hir_body.block.stmts.len());
    assert_eq!(checker_block.span, hir_body.block.span);
}

#[test]
fn checker_emits_fn_sigs_directly() {
    // Stage 5: the checker builds each function's `hir::FnSig` as it types it
    // (retiring `fn_params`/`fn_return`/`async_fns`); lowering copies them.
    let src = "\
function add(a: i64, b: i64): i64 { a + b }
function spin(): Future<null> async { }
";
    let (a, hir) = lower(src);
    // The checker populated `fn_sigs`; lowering carried it onto the HIR verbatim.
    assert_eq!(hir.fn_sigs.len(), a.results.fn_sigs.len());
    let add = a.program.defs.iter().position(|d| d.name == "add").map(|i| crate::ids::DefId(i as u32)).unwrap();
    let sig = &hir.fn_sigs[&add];
    assert_eq!(sig.params.len(), 2, "add has two params");
    assert_eq!(a.tcx.display(sig.ret, &|id| a.program.def(id).name.clone()), "i64");
    assert!(sig.async_output.is_none(), "add is not async");
    let spin = a.program.defs.iter().position(|d| d.name == "spin").map(|i| crate::ids::DefId(i as u32)).unwrap();
    assert!(hir.fn_sigs[&spin].async_output.is_some(), "spin is async — has an output type");
}

#[test]
fn builtin_clone_lowers_to_clone_intrinsic() {
    let src = "function f() { var xs = [1, 2, 3]; var ys = xs.clone(); }";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Intrinsic { intrinsic: Intrinsic::Clone(_), .. }
        )),
        "expected a Clone intrinsic for `xs.clone()`"
    );
}

#[test]
fn static_method_call_lowers_to_static_method_kind() {
    let src = "\
struct Box { v: i64 }
extend Box { function make(n: i64): Box { Box { v: n } } }
function f(): Box { Box.make(7) }
";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Call { kind: CallKind::Method { is_static: true, .. }, .. }
        )),
        "expected a static Method call for `Box.make(7)`"
    );
}

#[test]
fn operator_overload_lowers_with_overload_method() {
    let src = "\
struct V { x: i64 }
extend V { function add(self, o: V): V { V { x: self.x + o.x } } }
function f(a: V, b: V): V { a + b }
";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(
            &e.kind,
            ExprKind::Binary { overload: Some(_), .. }
        )),
        "expected `a + b` to carry an operator-overload method"
    );
}

#[test]
fn try_operator_lowers_to_try_node() {
    let src = "\
struct User { name: str }
function find(id: i64): User | str { if id == 1 { User { name: \"A\" } } else { \"nf\" } }
function f(id: i64): str { var u: User = find(id)?; u.name }
";
    let (a, hir) = lower(src);
    assert!(
        find_expr(&a, &hir, "f", |e| matches!(&e.kind, ExprKind::Try { .. })),
        "expected a Try node for `find(id)?`"
    );
}

#[test]
fn iterator_for_loop_lowers_with_iter_driver() {
    // A user `Iterator` (not the `List` fast path) lowers to `ForDriver::Iter`.
    // The iterator is bound to a variable first to avoid the `for x in EXPR {`
    // struct-literal/block ambiguity.
    let src = "\
struct Count { n: i64 }
extend Count: Iterator<i64> {
  function next(self): Item<i64> | Done {
    if self.n > 0 { self.n = self.n - 1; Item { value: self.n } } else { Done {} }
  }
}
function f() { var c = Count { n: 3 }; for x in c { var y = x; } }
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut iter_driver = false;
    walk_block(&body.block, &mut |e| {
        if matches!(&e.kind, ExprKind::For { driver: ForDriver::Iter(_), .. }) {
            iter_driver = true;
        }
    });
    assert!(iter_driver, "expected a `for` with the Iterator driver");
}

#[test]
fn widen_to_union_lowers_to_adjust_node() {
    let src = "function f(): i64 | str { 1 }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let trailing = body.block.trailing.as_ref().expect("trailing");
    assert!(
        matches!(&trailing.kind, ExprKind::Adjust { .. }),
        "expected the `1` to be wrapped in an Adjust (widen to `i64 | str`)"
    );
}

// --- broader HIR-node coverage (Stage 5 test mandate) ------------------------
// Pin the lowering of more node kinds + edge cases so the checker→HIR refactor
// (which will rebuild these from the checker rather than side tables) is bound
// by behavior, not implementation.

#[test]
fn closure_lowers_with_captures_and_body() {
    let src = "function f(): i64 { var n = 5; var g = () => n + 1; g() }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut saw_closure = false;
    walk_block(&body.block, &mut |e| {
        if let ExprKind::Closure { captures, is_async, .. } = &e.kind {
            assert!(!is_async, "plain closure");
            assert!(captures.iter().any(|(_, _)| true), "captures recorded");
            saw_closure = true;
        }
    });
    assert!(saw_closure, "expected a Closure node");
}

#[test]
fn async_block_lowers_to_async_block_node() {
    let src = "function f(): Future<i64> { async { 7 } }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut saw = false;
    walk_block(&body.block, &mut |e| {
        if matches!(&e.kind, ExprKind::AsyncBlock { .. }) {
            saw = true;
        }
    });
    assert!(saw, "expected an AsyncBlock node");
}

#[test]
fn await_lowers_to_await_node_with_output() {
    let src = "\
function g(): Future<i64> { async { 1 } }
function f(): Future<i64> async { var x = await g(); x }
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut out_ok = false;
    walk_block(&body.block, &mut |e| {
        if let ExprKind::Await { output, .. } = &e.kind {
            out_ok = a.tcx.display(*output, &|id| a.program.def(id).name.clone()) == "i64";
        }
    });
    assert!(out_ok, "expected an Await node yielding i64");
}

#[test]
fn for_map_lowers_with_map_driver() {
    let src = "\
function f() {
  var m = Map<str, i64>();
  m.set(\"a\", 1);
  for e in m { var k = e.key; }
}
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut map_driver = false;
    walk_block(&body.block, &mut |e| {
        if matches!(&e.kind, ExprKind::For { driver: ForDriver::Map { .. }, .. }) {
            map_driver = true;
        }
    });
    assert!(map_driver, "expected a `for` with the Map driver");
}

#[test]
fn match_lowers_all_arm_pattern_kinds() {
    let src = "\
function f(x: i64 | str | null): i64 {
  match x {
    null => 0,
    i64 n => n,
    str s => 1,
  }
}
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut arms = 0;
    walk_block(&body.block, &mut |e| {
        if let ExprKind::Match { arms: ms, .. } = &e.kind {
            arms = ms.len();
        }
    });
    assert_eq!(arms, 3, "three match arms lowered");
}

#[test]
fn user_to_str_interpolation_carries_stringify() {
    // A user type with `@Derive(ToStr)` interpolated in a string records the
    // resolved `to_str` on the `StrPart::Interp` node.
    let src = "\
@Derive(ToStr)
struct P { x: i64 }
function f(p: P): str { \"p=$p\" }
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let trailing = body.block.trailing.as_ref().expect("trailing str");
    let ExprKind::Str(parts) = &trailing.kind else { panic!("expected a Str node") };
    let has_stringify = parts.iter().any(|p| matches!(
        p,
        StrPart::Interp { stringify: Some(_), .. }
    ));
    assert!(has_stringify, "interpolated user value records its `to_str`");
}

#[test]
fn nested_field_and_index_preserve_spans() {
    let src = "\
struct Inner { v: i64 }
struct Outer { inner: Inner }
function f(o: Outer, xs: List<i64>): i64 { o.inner.v + xs[0] }
";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let mut fields = 0;
    let mut indexes = 0;
    walk_block(&body.block, &mut |e| {
        match &e.kind {
            ExprKind::Field { .. } => fields += 1,
            ExprKind::Index { .. } => indexes += 1,
            _ => {}
        }
        // Every node carries a real document-file span.
        assert_eq!(e.span.file, crate::span::FileId(0));
    });
    assert!(fields >= 2, "o.inner and .v are field accesses");
    assert_eq!(indexes, 1, "xs[0] is an index");
}

// ===========================================================================
// Expanded HIR-node + edge-case coverage (Stage 5: the checker is the sole HIR
// producer; these pin node kinds, desugars, drivers, and tricky paths).
// ===========================================================================

/// No `Error` node escapes for valid, feature-rich source — the checker built
/// every expression's HIR node.
#[test]
fn no_error_nodes_in_a_feature_rich_program() {
    let src = r#"
struct V { x: i64 }
extend V { function add(self, o: V): V { V { x: self.x + o.x } } }
extend V: Eq { function eq(self, o: V): bool { self.x == o.x } }
interface Greet { function greet(self): str; }
extend V: Greet { function greet(self): str { "v" } }
function id<T>(t: T): T { t }
function pick(b: bool): i64 | str { if b { 1 } else { "x" } }
function use_dyn(g: Greet): str { g.greet() }
function main() {
    var a = V { x: 1 };
    var b = V { x: 2 };
    var c = a.add(b);
    var eq = a == b;
    var n = id(7);
    var xs = [1, 2, 3];
    var total = 0;
    for e in xs { total = total + e; }
    var m = { "k": 1 };
    var s = "sum=${total} ${m.size()}";
    var p = pick(true);
    var k = match p { i64 v => v, str s => 0 };
    var g = use_dyn(a as Greet);
    var clos = (z: i64): i64 => z + n;
    var r = clos(k);
}
"#;
    let (_a, hir) = lower(src);
    let mut errors = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(e.kind, ExprKind::Error) {
            errors += 1;
        }
    });
    assert_eq!(errors, 0, "valid program lowered with {errors} Error node(s)");
}

/// Provenance survives arbitrarily deep nesting: every node keeps a real span,
/// and every binary operator is preserved (parens are transparent).
#[test]
fn deep_nesting_preserves_spans() {
    let src = "function f(): i64 { ((((1 + 2) * 3) - 4) / (5 + 6)) }";
    let (a, hir) = lower(src);
    let dummy = Span::dummy();
    let mut binaries = 0;
    walk_block(&body_named(&a, &hir, "f").block, &mut |e| {
        assert_ne!(e.span, dummy);
        if matches!(e.kind, ExprKind::Binary { .. }) {
            binaries += 1;
        }
    });
    assert_eq!(binaries, 5, "five binary ops nest here");
}

/// Nested flow-narrowing: a narrowed use inside an `is` branch bakes an `Unbox`.
#[test]
fn nested_narrowing_bakes_unbox() {
    let src = "\
function f(x: i64 | str | bool): i64 {
    if x is i64 { x + 1 } else { if x is bool { 0 } else { 2 } }
}";
    let (_a, hir) = lower(src);
    let mut unboxes = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(&e.kind, ExprKind::Adjust { adjust: crate::sema::results::Adjust::Unbox(_), .. }) {
            unboxes += 1;
        }
    });
    assert!(unboxes >= 1, "narrowed `x + 1` use must unbox");
}

/// Each link of a chained builtin-method sequence lowers to a `BuiltinMethod`.
#[test]
fn chained_builtin_methods_each_lower() {
    let src = "function f(): i64 { var xs = [3, 1, 2]; xs.push(4); xs.size() + xs.size() }";
    let (a, hir) = lower(src);
    let mut builtin_methods = 0;
    walk_block(&body_named(&a, &hir, "f").block, &mut |e| {
        if matches!(&e.kind, ExprKind::Call { kind: CallKind::BuiltinMethod { .. }, .. }) {
            builtin_methods += 1;
        }
    });
    assert_eq!(builtin_methods, 3, "push + two size() calls");
}

/// Nested closures both lower to `Closure` nodes.
#[test]
fn nested_closures_lower() {
    let src = "function f(): i64 { var add = (a: i64): i64 => { var inner = (b: i64): i64 => a + b; inner(10) }; add(5) }";
    let (_a, hir) = lower(src);
    let mut closures = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(e.kind, ExprKind::Closure { .. }) {
            closures += 1;
        }
    });
    assert_eq!(closures, 2, "outer and inner closures");
}

/// Tuple destructuring in `var` lowers a `Tuple` pattern (nested too).
#[test]
fn tuple_destructuring_patterns_lower() {
    let src = "function f(): i64 { var (a, (b, c)) = (40, (1, 1)); a + b + c }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let outer = match &body.block.stmts[0].kind {
        StmtKind::Let { pattern, .. } => pattern,
        _ => panic!("first stmt is a let"),
    };
    let PatternKind::Tuple { elems, .. } = &outer.kind else { panic!("outer tuple pattern") };
    assert_eq!(elems.len(), 2);
    assert!(matches!(elems[1].kind, PatternKind::Tuple { .. }), "nested tuple pattern");
}

/// All three statement kinds (`Let`, `Assign`, `Expr`) + a trailing expr lower.
#[test]
fn all_statement_kinds_lower() {
    let src = "function f(): i64 { var x = 1; x = x + 1; println(\"hi\"); x }";
    let (a, hir) = lower(src);
    let body = body_named(&a, &hir, "f");
    let (mut lets, mut assigns, mut exprs) = (0, 0, 0);
    for s in &body.block.stmts {
        match &s.kind {
            StmtKind::Let { .. } => lets += 1,
            StmtKind::Assign { .. } => assigns += 1,
            StmtKind::Expr(_) => exprs += 1,
            StmtKind::Item(_) => {}
        }
    }
    assert_eq!((lets, assigns, exprs), (1, 1, 1));
    assert!(body.block.trailing.is_some(), "trailing `x`");
}

/// `match` over a union lowers every arm; the `TypeBind` arm carries `test_ty`.
#[test]
fn match_arms_carry_patterns_and_test_types() {
    let src = "function f(x: i64 | str | null): i64 { match x { i64 n => n, str s => 1, null => 2 } }";
    let (a, hir) = lower(src);
    let mut arm_count = 0;
    let mut type_binds = 0;
    let mut unit_paths = 0;
    for_each_expr(&hir, &mut |e| {
        if let ExprKind::Match { arms, .. } = &e.kind {
            arm_count = arms.len();
            for arm in arms {
                match &arm.pattern.kind {
                    PatternKind::TypeBind { test_ty, .. } => {
                        assert!(!a.tcx.is_error(*test_ty));
                        type_binds += 1;
                    }
                    PatternKind::Literal(_) => unit_paths += 1, // `null` is a literal pattern
                    _ => {}
                }
            }
        }
    });
    assert_eq!(arm_count, 3);
    assert_eq!(type_binds, 2, "`i64 n` and `str s`");
    assert_eq!(unit_paths, 1, "the `null` arm");
}

/// Cast/`is` forms lower to `Cast` nodes with resolved targets.
#[test]
fn cast_and_is_forms_lower() {
    let src = "function f(): bool { var a = (300 as i64); var b: i64 | str = a; b is i64 }";
    let (a, hir) = lower(src);
    let mut casts = 0;
    for_each_expr(&hir, &mut |e| {
        if let ExprKind::Cast { target, .. } = &e.kind {
            assert!(!a.tcx.is_error(*target), "cast target resolved");
            casts += 1;
        }
    });
    assert!(casts >= 2, "`300 as i64` and `b is i64`");
}

/// String interpolation with multiple mixed-type holes lowers each as an
/// `Interp` part; only the user-typed hole carries a `to_str` method.
#[test]
fn string_interpolation_multi_hole() {
    let src = r#"
struct P { v: i64 }
extend P { function to_str(self): str { "P" } }
function f(): str { var p = P { v: 1 }; var n = 9; "a=${n} b=${p} c=${n + 1}" }
"#;
    let (a, hir) = lower(src);
    let mut found = false;
    walk_block(&body_named(&a, &hir, "f").block, &mut |e| {
        if let ExprKind::Str(parts) = &e.kind {
            let interps = parts.iter().filter(|p| matches!(p, StrPart::Interp { .. })).count();
            let with_tostr = parts
                .iter()
                .filter(|p| matches!(p, StrPart::Interp { stringify: Some(_), .. }))
                .count();
            assert_eq!(interps, 3, "three holes");
            assert_eq!(with_tostr, 1, "only `${{p}}` needs to_str");
            found = true;
        }
    });
    assert!(found, "expected an interpolated string node");
}

/// Struct construction with `..spread` lowers the spread expression.
#[test]
fn struct_spread_lowers() {
    let src = r#"
struct P { x: i64, y: i64 }
function f(): i64 { var a = P { x: 1, y: 2 }; var b = P { x: 9, ..a }; b.x + b.y }
"#;
    let (_a, hir) = lower(src);
    let mut spreads = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(&e.kind, ExprKind::Struct { spread: Some(_), .. }) {
            spreads += 1;
        }
    });
    assert_eq!(spreads, 1);
}

/// `for entry in map` lowers with the `Map` driver.
#[test]
fn for_over_map_uses_map_driver() {
    let src = "function f(): i64 { var m = { \"a\": 1, \"b\": 2 }; var s = 0; for e in m { s = s + e.value; } s }";
    let (_a, hir) = lower(src);
    let mut map_drivers = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(&e.kind, ExprKind::For { driver: ForDriver::Map { .. }, .. }) {
            map_drivers += 1;
        }
    });
    assert_eq!(map_drivers, 1);
}

/// A user `Iterator` impl drives `for` via the `Iter` driver (struct literal in
/// the loop header parenthesised, per the brace-ambiguity rule).
#[test]
fn for_over_user_iterator_uses_iter_driver() {
    let src = r#"
struct Count { n: i64, max: i64 }
extend Count: Iterator<i64> {
    function next(self): Item<i64> | Done {
        if self.n >= self.max { Done {} }
        else { var v = self.n; self.n = self.n + 1; Item { value: v } }
    }
}
function f(): i64 { var s = 0; for v in (Count { n: 0, max: 3 }) { s = s + v; } s }
"#;
    let (_a, hir) = lower(src);
    let mut iter_drivers = 0;
    for_each_expr(&hir, &mut |e| {
        if matches!(&e.kind, ExprKind::For { driver: ForDriver::Iter(_), .. }) {
            iter_drivers += 1;
        }
    });
    assert_eq!(iter_drivers, 1);
}

/// `await` and `spawn` lower with their resolved `Output` type.
#[test]
fn await_and_spawn_carry_output_types() {
    let src = r#"
function work(): Future<i64> async { 7 }
function f(): Future<i64> async { var h = spawn work(); await h }
"#;
    let (a, hir) = lower(src);
    let (mut awaits, mut spawns) = (0, 0);
    for_each_expr(&hir, &mut |e| match &e.kind {
        ExprKind::Await { output, .. } => {
            assert!(!a.tcx.is_error(*output));
            awaits += 1;
        }
        ExprKind::Spawn { output, .. } => {
            assert!(!a.tcx.is_error(*output));
            spawns += 1;
        }
        _ => {}
    });
    assert_eq!((awaits, spawns), (1, 1));
}

/// The `?` operator lowers to a `Try` node.
#[test]
fn try_operator_union_lowers_to_try_node() {
    // `g()` is `i64 | str`; in `f` (returning `str`) the `str` variant is the
    // failure (early-returned) and `i64` is the success — a valid `?` partition.
    let src = r#"
function g(): i64 | str { 1 }
function f(): str { var x = g()?; "ok" }
"#;
    let (a, hir) = lower(src);
    let mut tries = 0;
    walk_block(&body_named(&a, &hir, "f").block, &mut |e| {
        if matches!(e.kind, ExprKind::Try { .. }) {
            tries += 1;
        }
    });
    assert_eq!(tries, 1);
}

/// A generic call records its solved type arguments on the HIR `Call` node.
#[test]
fn generic_call_carries_type_args() {
    let src = "function id<T>(t: T): T { t }\nfunction f(): i64 { id(42) }";
    let (_a, hir) = lower(src);
    let mut found = false;
    for_each_expr(&hir, &mut |e| {
        if let ExprKind::Call { kind: CallKind::Direct { type_args, .. }, .. } = &e.kind {
            if !type_args.is_empty() {
                found = true;
            }
        }
    });
    assert!(found, "id<i64> solved type args present");
}

/// `loop` with a value-carrying `break` lowers the loop and its break value.
#[test]
fn loop_with_break_value_lowers() {
    let src = "function f(): i64 { var i = 0; loop { i = i + 1; if i > 3 { break i; } } }";
    let (_a, hir) = lower(src);
    let (mut loops, mut breaks_with_value) = (0, 0);
    for_each_expr(&hir, &mut |e| match &e.kind {
        ExprKind::Loop(_) => loops += 1,
        ExprKind::Break(Some(_)) => breaks_with_value += 1,
        _ => {}
    });
    assert_eq!(loops, 1);
    assert_eq!(breaks_with_value, 1);
}
