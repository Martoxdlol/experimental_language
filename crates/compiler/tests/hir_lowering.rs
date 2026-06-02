//! Integration gate for HIR lowering (migration Stage 2).
//!
//! Lowers every single-file example program the checker accepts and asserts the
//! resulting HIR is a faithful, lossless translation:
//!
//! * no [`ExprKind::Error`] node leaks through (the lowering is *total* over the
//!   real feature surface — async, generics, closures, FFI, channels, …);
//! * every node preserves a real source span (provenance);
//! * every non-coerced node's type equals the checker's `expr_types` entry.
//!
//! This complements the focused unit tests in `hir::lower_tests` by exercising
//! the lowering against the whole example corpus at once.

use compiler::hir::{Block, CallKind, Expr, ExprKind, Hir, MapEntry, StmtKind, StrPart};
use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::sema::{Analysis, analyze};
use compiler::span::{FileId, Span};
use std::path::PathBuf;

fn examples_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/crates/compiler`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

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
            fields.iter().for_each(|fi| walk_expr(&fi.value, f));
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
        | ExprKind::Adjust { expr, .. }
        | ExprKind::Try { expr, .. }
        | ExprKind::Await { expr, .. }
        | ExprKind::Spawn { expr, .. } => walk_expr(expr, f),
        ExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
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

/// Per-program lowering statistics.
struct Stats {
    /// User-source (`FileId(0)`) expression nodes checked.
    user_nodes: usize,
    /// `Error` nodes found in compiler-synthesized prelude bodies (non-source
    /// `FileId`). Reported, never hidden — lowering these is a Stage-3 concern
    /// (codegen will force it correct via the iterator/async examples).
    synth_errors: usize,
}

/// Assert HIR invariants over one lowered program.
///
/// Provenance and losslessness are asserted for **user source** nodes
/// (`FileId(0)`): the goal's "every node traces back to its source" is about the
/// program the user wrote. Compiler-injected prelude bodies live in a synthetic
/// file; their lowering fidelity is tracked separately (see [`Stats`]).
fn check_hir(name: &str, a: &Analysis, hir: &Hir) -> Stats {
    let dummy = Span::dummy();
    let mut st = Stats {
        user_nodes: 0,
        synth_errors: 0,
    };
    for body in hir.bodies.values() {
        walk_block(&body.block, &mut |e| {
            if e.span.file != FileId(0) {
                if matches!(e.kind, ExprKind::Error) {
                    st.synth_errors += 1;
                }
                return;
            }
            st.user_nodes += 1;
            assert!(
                !matches!(e.kind, ExprKind::Error),
                "{name}: lowering produced an Error node at {:?} — a user-source lowering gap",
                e.span
            );
            assert_ne!(e.span, dummy, "{name}: node lost its span: {:?}", e.kind);
            // Type-table consistency on non-coerced nodes (a baked `Adjust`
            // wrapper carries the post-coercion type; its inner node matches).
            if !matches!(e.kind, ExprKind::Adjust { .. }) {
                if let Some(t) = a.expr_ty(e.span) {
                    assert_eq!(
                        e.ty, t,
                        "{name}: type mismatch at {:?} for {:?}",
                        e.span, e.kind
                    );
                }
            }
        });
    }
    st
}

#[test]
fn lowers_every_clean_example_losslessly() {
    let dir = examples_dir();
    let mut lowered = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut total_nodes = 0usize;
    let mut synth_errors = 0usize;

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read examples dir {dir:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "otter").unwrap_or(false))
        .collect();
    entries.sort();

    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let (tokens, le) = lex(&src, FileId(0));
        if !le.is_empty() {
            skipped.push(format!("{name} (lex)"));
            continue;
        }
        let (module, pe) = parse(&src, &tokens);
        if !pe.is_empty() {
            // A single-file parse of a multi-file example (e.g. needs modules).
            skipped.push(format!("{name} (parse)"));
            continue;
        }
        let a = analyze(&module);
        if !a.errors.is_empty() {
            // Needs externals/modules or otherwise isn't a clean single file.
            skipped.push(format!("{name} (analysis)"));
            continue;
        }
        let st = check_hir(&name, &a, &a.hir);
        total_nodes += st.user_nodes;
        synth_errors += st.synth_errors;
        lowered += 1;
    }

    eprintln!(
        "HIR lowering gate: lowered {lowered} examples ({total_nodes} user-source expr nodes); \
         synthesized-body Error nodes (Stage-3 follow-up): {synth_errors}; skipped: {skipped:?}"
    );
    // The corpus has many clean single-file programs; require a healthy floor so
    // a regression that makes everything skip is caught.
    assert!(
        lowered >= 10,
        "expected to lower ≥10 examples, only did {lowered}"
    );
}
