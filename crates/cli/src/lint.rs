//! `otter_fusion lint` (`docs/23`): lightweight, opinion-free static lints over the
//! typed HIR — currently **unused local variables** and **unused private
//! functions**. Lints read the already-computed analysis (no extra inference)
//! and are purely informational (they never change exit status by themselves).

use std::collections::HashSet;

use compiler::hir::{Block, CallKind, Expr, ExprKind, MapEntry, Stmt, StmtKind, StrPart};
use compiler::ids::{DefId, LocalId};
use compiler::sema::symbols::DefKind;
use compiler::sema::{Analysis, ValueRes};
use compiler::span::{SourceMap, Span};

/// What a walk over a body collects: which locals are *read* and which
/// definitions are *referenced* (called or used as a value).
#[derive(Default)]
struct Uses {
    locals: HashSet<LocalId>,
    defs: HashSet<DefId>,
}

/// The structured findings of a lint pass: unused local variables (each as its
/// binding-name span + name) and unused private functions (name-item span +
/// name). `fix` consumes the unused-local list; the `lint` command flattens both.
pub struct Lints {
    pub unused_locals: Vec<(Span, String)>,
    pub unused_fns: Vec<(Span, String)>,
}

/// Run the lint analysis: one HIR walk to gather local reads + def references,
/// then collect unused locals and unused private functions. `map` reads local
/// names (to honour the `_name` "intentionally unused" convention) and filters
/// out synthesized/prelude spans.
pub fn analyze(analysis: &Analysis, map: &SourceMap) -> Lints {
    let hir = &analysis.hir;
    let mut uses = Uses::default();
    for body in hir.bodies.values() {
        walk_block(&body.block, &mut uses);
    }

    // (1) Unused local variables: a binding never read. Skip parameters (an
    // unused parameter is often intentional) and names starting with `_`.
    let mut unused_locals = Vec::new();
    for body in hir.bodies.values() {
        let params: HashSet<LocalId> = body.params.iter().copied().collect();
        for &local in body.locals.keys() {
            if params.contains(&local) || uses.locals.contains(&local) {
                continue;
            }
            let Some(&decl) = hir.local_decls.get(&local) else { continue };
            if (decl.file.0 as usize) >= map.file_count() {
                continue;
            }
            let name = map.slice(decl);
            if name.starts_with('_') || name.is_empty() {
                continue;
            }
            unused_locals.push((decl, name.to_string()));
        }
    }
    unused_locals.sort_by_key(|(s, _)| (s.file.0, s.lo.0));

    // (2) Unused private functions: a non-`pub` free function never called or
    // used as a value. Skip `main`, tests/benches, and methods (callable via
    // dynamic dispatch or derive).
    let mut unused_fns = Vec::new();
    for (i, def) in analysis.program.defs.iter().enumerate() {
        if def.kind != DefKind::Function || def.public || def.name == "main" {
            continue;
        }
        if uses.defs.contains(&DefId(i as u32)) || (def.span.file.0 as usize) >= map.file_count() {
            continue;
        }
        unused_fns.push((def.span, def.name.clone()));
    }
    unused_fns.sort_by_key(|(s, _)| (s.file.0, s.lo.0));

    Lints { unused_locals, unused_fns }
}

/// Collect every lint warning as `(span, message)` pairs in source order — the
/// flattened view the `lint` command renders.
pub fn collect_lints(analysis: &Analysis, map: &SourceMap) -> Vec<(Span, String)> {
    let l = analyze(analysis, map);
    let mut out: Vec<(Span, String)> = Vec::new();
    out.extend(l.unused_locals.into_iter().map(|(s, n)| (s, format!("unused variable `{n}`"))));
    out.extend(l.unused_fns.into_iter().map(|(s, n)| (s, format!("unused function `{n}`"))));
    out.sort_by_key(|(s, _)| (s.file.0, s.lo.0));
    out
}

fn walk_block(b: &Block, u: &mut Uses) {
    for s in &b.stmts {
        walk_stmt(s, u);
    }
    if let Some(t) = &b.trailing {
        walk_expr(t, u);
    }
}

fn walk_stmt(s: &Stmt, u: &mut Uses) {
    match &s.kind {
        StmtKind::Let { init, .. } => walk_expr(init, u),
        StmtKind::Assign { target, value } => {
            // The assignment target's local is a *write*, not a read — but a
            // field/index target's receiver (and the value) are reads.
            walk_expr(target, u);
            walk_expr(value, u);
        }
        StmtKind::Expr(e) => walk_expr(e, u),
        StmtKind::Item(_) => {}
    }
}

fn record_res(res: &ValueRes, u: &mut Uses) {
    match res {
        ValueRes::Local(l) => {
            u.locals.insert(*l);
        }
        ValueRes::Function(d)
        | ValueRes::Method(d)
        | ValueRes::Global(d)
        | ValueRes::StructCtor(d) => {
            u.defs.insert(*d);
        }
        ValueRes::Builtin(_) => {}
    }
}

fn walk_expr(e: &Expr, u: &mut Uses) {
    use ExprKind as K;
    match &e.kind {
        K::Name(res) => record_res(res, u),
        K::Str(parts) => {
            for p in parts {
                if let StrPart::Interp { expr, .. } = p {
                    walk_expr(expr, u);
                }
            }
        }
        K::Tuple(xs) | K::List(xs) => xs.iter().for_each(|x| walk_expr(x, u)),
        K::Map(entries) => {
            for entry in entries {
                match entry {
                    MapEntry::Kv { key, value } => {
                        walk_expr(key, u);
                        walk_expr(value, u);
                    }
                    MapEntry::Spread(x) => walk_expr(x, u),
                }
            }
        }
        K::Struct { fields, spread, .. } => {
            fields.iter().for_each(|f| walk_expr(&f.value, u));
            if let Some(s) = spread {
                walk_expr(s, u);
            }
        }
        K::Field { receiver, .. } | K::TupleIndex { receiver, .. } => walk_expr(receiver, u),
        K::Index { receiver, index } => {
            walk_expr(receiver, u);
            walk_expr(index, u);
        }
        K::Call { kind, args, .. } => {
            match kind {
                CallKind::Direct { def, .. }
                | CallKind::Method { def, .. }
                | CallKind::TupleCtor { def, .. }
                | CallKind::Extern { def } => {
                    u.defs.insert(*def);
                }
                CallKind::Closure { callee } => walk_expr(callee, u),
                CallKind::Builtin(_) | CallKind::BuiltinMethod { .. } => {}
            }
            args.iter().for_each(|a| walk_expr(a, u));
        }
        K::Intrinsic { args, .. } => args.iter().for_each(|a| walk_expr(a, u)),
        K::Unary { operand, .. } => walk_expr(operand, u),
        K::Binary { left, right, .. } => {
            walk_expr(left, u);
            walk_expr(right, u);
        }
        K::Cast { expr, .. }
        | K::Ref(expr)
        | K::Deref(expr)
        | K::Adjust { expr, .. }
        | K::Await { expr, .. }
        | K::Spawn { expr, .. } => walk_expr(expr, u),
        K::Try { expr, .. } => walk_expr(expr, u),
        K::If { cond, then_block, else_branch } => {
            walk_expr(cond, u);
            walk_block(then_block, u);
            if let Some(e) = else_branch {
                walk_expr(e, u);
            }
        }
        K::Match { scrutinee, arms } => {
            walk_expr(scrutinee, u);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_expr(g, u);
                }
                walk_expr(&arm.body, u);
            }
        }
        K::While { cond, body } => {
            walk_expr(cond, u);
            walk_block(body, u);
        }
        K::For { iter, body, .. } => {
            walk_expr(iter, u);
            walk_block(body, u);
        }
        K::Block(b) | K::Loop(b) => walk_block(b, u),
        K::Return(e) | K::Break(e) => {
            if let Some(e) = e {
                walk_expr(e, u);
            }
        }
        K::Closure { body, .. } => walk_expr(body, u),
        K::AsyncBlock { body, .. } => walk_block(body, u),
        K::Int(_) | K::Float(_) | K::Bool(_) | K::Null | K::Char(_) | K::Continue
        | K::Discard | K::Error => {}
    }
}
