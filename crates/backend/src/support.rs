//! Shared codegen support (`docs/22` §4): pure helpers over `Analysis` and the
//! Cranelift `Module` used by both the whole-program driver and the per-function
//! code generator — type lowering, struct/tuple layout, monomorphization
//! instance management, and async-body (state-machine) analysis. Factored out of
//! the per-function generator so each layer stays navigable.

use crate::{CgResult, CodegenError, Instance, PTR};
use compiler::ast::*;
use compiler::ids::{DefId, LocalId};
use compiler::sema::{Analysis, DefKind, StructFields, ValueRes};
use compiler::span::Span;
use compiler::ty::{FloatTy, IntTy, Ty, TyKind};
use cranelift_codegen::ir::{types, AbiParam, Type as ClType};
use cranelift_module::{FuncId, Linkage, Module};
use std::collections::{HashMap, HashSet};

// -- async body analysis (state-machine lowering, `docs/21`) ----------------

/// Whether `block` contains an `await` in its own async scope (NOT descending
/// into nested closures / `async { … }` blocks, which have their own `poll`).
pub(crate) fn block_has_await(block: &Block) -> bool {
    block.stmts.iter().any(stmt_has_await)
        || block.trailing.as_deref().is_some_and(expr_has_await)
}

pub(crate) fn stmt_has_await(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Var(v) => expr_has_await(&v.init),
        StmtKind::Assign { target, value } => expr_has_await(target) || expr_has_await(value),
        StmtKind::Expr(e) => expr_has_await(e),
        StmtKind::Item(_) => false,
    }
}

pub(crate) fn expr_has_await(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Await { .. } => true,
        // Nested async scopes have their own poll function — do not descend.
        ExprKind::Closure { .. } | ExprKind::AnonFn(_) | ExprKind::AsyncBlock(_) => false,
        ExprKind::Paren(x) | ExprKind::Unary { operand: x, .. }
        | ExprKind::Cast { expr: x, .. } | ExprKind::Field { receiver: x, .. }
        | ExprKind::TupleIndex { receiver: x, .. } | ExprKind::Try { expr: x, .. }
        | ExprKind::Ref { expr: x, .. } | ExprKind::Deref { expr: x, .. }
        | ExprKind::Spawn { expr: x, .. } => expr_has_await(x),
        ExprKind::Binary { left, right, .. } => expr_has_await(left) || expr_has_await(right),
        ExprKind::Tuple(xs) | ExprKind::List(xs) => xs.iter().any(expr_has_await),
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            expr_has_await(callee)
                || args.iter().any(expr_has_await)
                || trailing_closure.as_deref().is_some_and(expr_has_await)
        }
        ExprKind::Index { receiver, index } => expr_has_await(receiver) || expr_has_await(index),
        ExprKind::StructLit { fields, spread, .. } => {
            fields.iter().any(|f| f.value.as_ref().is_some_and(expr_has_await))
                || spread.as_deref().is_some_and(expr_has_await)
        }
        ExprKind::MapLit(items) => items.iter().any(|it| match it {
            MapItem::Entry { key, value, .. } => expr_has_await(key) || expr_has_await(value),
            MapItem::Spread(e) => expr_has_await(e),
        }),
        ExprKind::If { cond, then_block, else_branch } => {
            expr_has_await(cond) || block_has_await(then_block)
                || match else_branch {
                    Some(ElseBranch::If(e)) => expr_has_await(e),
                    Some(ElseBranch::Block(b)) => block_has_await(b),
                    None => false,
                }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_has_await(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_await) || expr_has_await(&a.body)
                })
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => block_has_await(b),
        ExprKind::While { cond, body } => expr_has_await(cond) || block_has_await(body),
        ExprKind::For { in_async, iter, body, .. } => {
            *in_async || expr_has_await(iter) || block_has_await(body)
        }
        ExprKind::Return(v) | ExprKind::Break(v) => v.as_deref().is_some_and(expr_has_await),
        _ => false,
    }
}

/// Record the local a binding-site `span` resolves to (deduped, in order).
pub(crate) fn push_local(a: &Analysis, span: Span, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    if let Some(ValueRes::Local(id)) = a.results.resolution(span) {
        if seen.insert(id) {
            out.push(id);
        }
    }
}

/// Enumerate every local *binding* introduced in `block` (so an async state
/// struct can reserve a slot for each), NOT descending into nested closures /
/// `async { … }` blocks (their locals live in their own frames).
pub(crate) fn collect_block_locals(a: &Analysis, block: &Block, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    for s in &block.stmts {
        collect_stmt_locals(a, s, out, seen);
    }
    if let Some(t) = &block.trailing {
        collect_expr_locals(a, t, out, seen);
    }
}

pub(crate) fn collect_stmt_locals(a: &Analysis, s: &Stmt, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &s.kind {
        StmtKind::Var(v) => {
            collect_pat_locals(a, &v.pattern, out, seen);
            collect_expr_locals(a, &v.init, out, seen);
        }
        StmtKind::Assign { target, value } => {
            collect_expr_locals(a, target, out, seen);
            collect_expr_locals(a, value, out, seen);
        }
        StmtKind::Expr(e) => collect_expr_locals(a, e, out, seen),
        StmtKind::Item(_) => {}
    }
}

pub(crate) fn collect_pat_locals(a: &Analysis, p: &Pattern, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &p.kind {
        PatternKind::Binding(name) => push_local(a, name.span, out, seen),
        PatternKind::TypeBinding { binding: Some(name), .. } => push_local(a, name.span, out, seen),
        PatternKind::TupleStruct { fields, rest, .. } => {
            for f in fields {
                collect_pat_locals(a, f, out, seen);
            }
            if let Some(r) = rest {
                if let Some(n) = &r.name {
                    push_local(a, n.span, out, seen);
                }
            }
        }
        PatternKind::RecordStruct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(sub) => collect_pat_locals(a, sub, out, seen),
                    None => push_local(a, f.name.span, out, seen), // shorthand binds the field
                }
            }
        }
        PatternKind::Tuple { elems, rest } | PatternKind::List { elems, rest } => {
            for e in elems {
                collect_pat_locals(a, e, out, seen);
            }
            if let Some((_, r)) = rest {
                if let Some(n) = &r.name {
                    push_local(a, n.span, out, seen);
                }
            }
        }
        PatternKind::Or(ps) => {
            for sub in ps {
                collect_pat_locals(a, sub, out, seen);
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_expr_locals(a: &Analysis, e: &Expr, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &e.kind {
        // Nested async/closure scopes own their locals.
        ExprKind::Closure { .. } | ExprKind::AnonFn(_) | ExprKind::AsyncBlock(_) => {}
        ExprKind::Paren(x) | ExprKind::Unary { operand: x, .. }
        | ExprKind::Cast { expr: x, .. } | ExprKind::Field { receiver: x, .. }
        | ExprKind::TupleIndex { receiver: x, .. } | ExprKind::Try { expr: x, .. }
        | ExprKind::Ref { expr: x, .. } | ExprKind::Deref { expr: x, .. }
        | ExprKind::Await { expr: x, .. } | ExprKind::Spawn { expr: x, .. } => {
            collect_expr_locals(a, x, out, seen)
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_locals(a, left, out, seen);
            collect_expr_locals(a, right, out, seen);
        }
        ExprKind::Tuple(xs) | ExprKind::List(xs) => {
            for x in xs {
                collect_expr_locals(a, x, out, seen);
            }
        }
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            collect_expr_locals(a, callee, out, seen);
            for x in args {
                collect_expr_locals(a, x, out, seen);
            }
            if let Some(tc) = trailing_closure {
                collect_expr_locals(a, tc, out, seen);
            }
        }
        ExprKind::Index { receiver, index } => {
            collect_expr_locals(a, receiver, out, seen);
            collect_expr_locals(a, index, out, seen);
        }
        ExprKind::StructLit { fields, spread, .. } => {
            for f in fields {
                if let Some(v) = &f.value {
                    collect_expr_locals(a, v, out, seen);
                }
            }
            if let Some(s) = spread {
                collect_expr_locals(a, s, out, seen);
            }
        }
        ExprKind::MapLit(items) => {
            for it in items {
                match it {
                    MapItem::Entry { key, value, .. } => {
                        collect_expr_locals(a, key, out, seen);
                        collect_expr_locals(a, value, out, seen);
                    }
                    MapItem::Spread(x) => collect_expr_locals(a, x, out, seen),
                }
            }
        }
        ExprKind::If { cond, then_block, else_branch } => {
            collect_expr_locals(a, cond, out, seen);
            collect_block_locals(a, then_block, out, seen);
            match else_branch {
                Some(ElseBranch::If(x)) => collect_expr_locals(a, x, out, seen),
                Some(ElseBranch::Block(b)) => collect_block_locals(a, b, out, seen),
                None => {}
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_expr_locals(a, scrutinee, out, seen);
            for arm in arms {
                collect_pat_locals(a, &arm.pattern, out, seen);
                if let Some(g) = &arm.guard {
                    collect_expr_locals(a, g, out, seen);
                }
                collect_expr_locals(a, &arm.body, out, seen);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => collect_block_locals(a, b, out, seen),
        ExprKind::While { cond, body } => {
            collect_expr_locals(a, cond, out, seen);
            collect_block_locals(a, body, out, seen);
        }
        ExprKind::For { pattern, iter, body, .. } => {
            collect_pat_locals(a, pattern, out, seen);
            collect_expr_locals(a, iter, out, seen);
            collect_block_locals(a, body, out, seen);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                collect_expr_locals(a, x, out, seen);
            }
        }
        _ => {}
    }
}

/// Collect the spans of `await`s that appear in a *statement-level* position
/// (the whole RHS of a `var`/assignment, a bare expression statement, a block's
/// trailing expression, or `return`) — the positions where no sibling
/// sub-expression temporary is live across the suspension point, so saving and
/// restoring named locals alone is correct. Recurses through control-flow
/// bodies. Awaits elsewhere are not collected (and are rejected at codegen)
/// until ANF hoisting lands.
pub(crate) fn scan_stmt_awaits(block: &Block, out: &mut Vec<Span>) {
    for s in &block.stmts {
        match &s.kind {
            StmtKind::Var(v) => scan_value_await(&v.init, out),
            StmtKind::Assign { value, .. } => scan_value_await(value, out),
            StmtKind::Expr(e) => scan_value_await(e, out),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &block.trailing {
        scan_value_await(t, out);
    }
}

pub(crate) fn scan_value_await(e: &Expr, out: &mut Vec<Span>) {
    match &e.kind {
        ExprKind::Await { kw_span, .. } => out.push(*kw_span),
        ExprKind::Paren(x) | ExprKind::Return(Some(x)) | ExprKind::Break(Some(x)) => {
            scan_value_await(x, out)
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => scan_stmt_awaits(b, out),
        ExprKind::While { body, .. } => scan_stmt_awaits(body, out),
        ExprKind::For { in_async, iter, body, .. } => {
            // `for await` introduces one suspend site (the `next_async()` await),
            // keyed by the iterable span.
            if *in_async {
                out.push(iter.span);
            }
            scan_stmt_awaits(body, out);
        }
        ExprKind::If { then_block, else_branch, .. } => {
            scan_stmt_awaits(then_block, out);
            match else_branch {
                Some(ElseBranch::If(x)) => scan_value_await(x, out),
                Some(ElseBranch::Block(b)) => scan_stmt_awaits(b, out),
                None => {}
            }
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                scan_value_await(&arm.body, out);
            }
        }
        _ => {}
    }
}

/// The state-struct layout for an async body that suspends: `[state @0][inner
/// future @8][local_i @16 + i*8]` over every body local (the `entry` locals —
/// parameters or captures — come first, since the constructor pre-stores them).
pub(crate) struct AsyncLayout {
    /// Local → byte offset within the state struct.
    pub(crate) slot_off: HashMap<LocalId, i32>,
    /// Locals that carry a runtime value (with offset + Cranelift type) — the
    /// ones the state machine saves and restores.
    pub(crate) live: Vec<(LocalId, i32, ClType)>,
    /// Managed-pointer field offsets for the GC descriptor (includes `inner`).
    pub(crate) ptr_offsets: Vec<u32>,
    /// Total state-struct size in bytes.
    pub(crate) state_size: u32,
}

/// Offset of the suspended-inner-future slot in every async state struct.
pub(crate) const ASYNC_INNER_OFF: i32 = 8;

pub(crate) fn async_state_layout(
    analysis: &Analysis,
    subst: &HashMap<DefId, Ty>,
    entry: &[LocalId],
    body: &Block,
    captured_locals: &HashSet<LocalId>,
) -> AsyncLayout {
    let mut all_locals = entry.to_vec();
    let mut seen: HashSet<LocalId> = all_locals.iter().copied().collect();
    collect_block_locals(analysis, body, &mut all_locals, &mut seen);
    let mut slot_off = HashMap::new();
    let mut ptr_offsets = vec![ASYNC_INNER_OFF as u32]; // the inner-future slot is managed
    let mut live = Vec::new();
    for (i, l) in all_locals.iter().enumerate() {
        let off = (16 + i * 8) as i32;
        slot_off.insert(*l, off);
        // Captured locals are cell-backed (`docs/09` §7): the Cranelift
        // variable holds a managed cell pointer, regardless of the local's
        // declared (content) type. The async state saves/restores that
        // pointer, so the slot is `PTR` and the descriptor traces it.
        if captured_locals.contains(l) {
            live.push((*l, off, PTR));
            ptr_offsets.push(off as u32);
            continue;
        }
        let ty = analysis.results.local_ty(*l).unwrap_or(analysis.tcx.error);
        let resolved = resolve_shallow(analysis, ty, subst);
        if let Some(ct) = clty_of(analysis, resolved) {
            live.push((*l, off, ct));
            if is_managed_ptr(analysis, resolved) {
                ptr_offsets.push(off as u32);
            }
        }
    }
    let state_size = (16 + all_locals.len() * 8) as u32;
    AsyncLayout { slot_off, live, ptr_offsets, state_size }
}

/// The generic-parameter → argument substitution for an instance.
pub(crate) fn build_subst(analysis: &Analysis, def: DefId, args: &[Ty]) -> HashMap<DefId, Ty> {
    let prog = &analysis.program;
    // For an `extend`/interface method, the enclosing block's generics (e.g. the
    // `T` of `extend<T> Pair<T>`) come first; the method's own generics follow.
    let mut params: Vec<DefId> = Vec::new();
    if let Some(parent) = prog.def(def).parent {
        if matches!(prog.def(parent).kind, DefKind::Extend) {
            params.extend(prog.def(parent).generics.iter().copied());
        }
    }
    params.extend(prog.def(def).generics.iter().copied());
    params.into_iter().zip(args.iter().copied()).collect()
}

/// A unique Cranelift symbol for an instance: name, def id, and arg type ids.
pub(crate) fn mangle(analysis: &Analysis, def: DefId, args: &[Ty]) -> String {
    let mut s = format!("{}${}", analysis.program.def(def).name, def.index());
    for a in args {
        s.push('_');
        s.push_str(&type_id(analysis, *a).to_string());
    }
    s
}

/// Shallow-substitute a top-level `Param` (sufficient for clty/type_id/layout).
pub(crate) fn resolve_shallow(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Ty {
    if subst.is_empty() {
        return ty;
    }
    match analysis.tcx.kind(ty) {
        TyKind::Param(d) => subst.get(d).copied().unwrap_or(ty),
        _ => ty,
    }
}

pub(crate) fn clty_subst(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Option<ClType> {
    clty_of(analysis, resolve_shallow(analysis, ty, subst))
}

/// Build a function/method instance's Cranelift signature under `subst`, or
/// `None` if a parameter type is not lowerable.
pub(crate) fn signature_of(
    module: &mut impl Module,
    analysis: &Analysis,
    def: DefId,
    subst: &HashMap<DefId, Ty>,
) -> CgResult<Option<cranelift_codegen::ir::Signature>> {
    let results = &analysis.results;
    let ret = results.fn_return.get(&def).copied().unwrap_or(analysis.tcx.null);
    let params = results.fn_params.get(&def).cloned().unwrap_or_default();
    let mut sig = module.make_signature();
    for p in &params {
        let ty = results.local_ty(*p).unwrap_or(analysis.tcx.error);
        match clty_subst(analysis, ty, subst) {
            Some(ct) => sig.params.push(AbiParam::new(ct)),
            None => return Ok(None),
        }
    }
    if let Some(ct) = clty_subst(analysis, ret, subst) {
        sig.returns.push(AbiParam::new(ct));
    }
    Ok(Some(sig))
}

/// Declare an instance (idempotently), queuing it for definition. Returns
/// `None` if its signature is not lowerable.
pub(crate) fn declare_instance(
    module: &mut impl Module,
    funcs: &mut HashMap<Instance, FuncId>,
    worklist: &mut Vec<Instance>,
    analysis: &Analysis,
    def: DefId,
    args: Vec<Ty>,
) -> CgResult<Option<FuncId>> {
    let inst = (def, args);
    if let Some(&f) = funcs.get(&inst) {
        return Ok(Some(f));
    }
    let subst = build_subst(analysis, def, &inst.1);
    let Some(sig) = signature_of(module, analysis, def, &subst)? else {
        return Ok(None);
    };
    let name = mangle(analysis, def, &inst.1);
    let fid = module
        .declare_function(&name, Linkage::Export, &sig)
        .map_err(|e| CodegenError::new(analysis.program.def(def).span, format!("declare: {e}")))?;
    funcs.insert(inst.clone(), fid);
    worklist.push(inst);
    Ok(Some(fid))
}

/// Shared, immutable codegen context handed to the per-function generator.

pub(crate) fn clty_of(analysis: &Analysis, ty: Ty) -> Option<ClType> {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => Some(int_clty(*it)),
        TyKind::Float(FloatTy::F32) => Some(types::F32),
        TyKind::Float(FloatTy::F64) => Some(types::F64),
        TyKind::Bool => Some(types::I8),
        TyKind::Char => Some(types::I32),
        // `str` is a managed reference — a pointer (to a runtime `LangStr`).
        TyKind::Str => Some(PTR),
        // Structs are managed references (a pointer to the field block); an
        // interface object is a pointer to a `{vtable, data}` fat-pointer box.
        TyKind::Named { def, .. }
            if matches!(
                analysis.program.def(*def).kind,
                DefKind::Struct | DefKind::ExternStruct | DefKind::Interface
            ) =>
        {
            Some(PTR)
        }
        // Anonymous tuples are heap-boxed records — a pointer.
        TyKind::Tuple(_) => Some(PTR),
        // A closure value is a pointer to its heap environment.
        TyKind::Func { .. } => Some(PTR),
        // A union/dynamic value is a pointer to a `{type_id, data}` box.
        TyKind::Union(_) | TyKind::Dynamic => Some(PTR),
        // A raw FFI pointer `*T` is a machine pointer (`docs/19`).
        TyKind::Ptr(_) => Some(PTR),
        TyKind::Null | TyKind::Never => None,
        _ => None,
    }
}

/// A stable runtime type id for a (non-union) type, stored in a union/dynamic
/// box so `is`/`as` can identify the inhabited variant. Conceptually the
/// "type pointer" of `docs/16` §3, collapsed to an integer for now.
pub(crate) fn type_id(analysis: &Analysis, ty: Ty) -> i64 {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => match it {
            IntTy::I8 => 1,
            IntTy::I16 => 2,
            IntTy::I32 => 3,
            IntTy::I64 => 4,
            IntTy::U8 => 5,
            IntTy::U16 => 6,
            IntTy::U32 => 7,
            IntTy::U64 => 8,
            IntTy::Isize => 9,
            IntTy::Usize => 10,
        },
        TyKind::Float(FloatTy::F32) => 11,
        TyKind::Float(FloatTy::F64) => 12,
        TyKind::Bool => 13,
        TyKind::Char => 14,
        TyKind::Str => 15,
        TyKind::Null => 16,
        // Nominal types get ids past the primitive range, keyed by def.
        TyKind::Named { def, .. } => 1000 + def.index() as i64,
        // Tuples/functions in unions are not yet supported; -1 never matches.
        _ => -1,
    }
}

/// The `(MIN, MAX)` bit patterns (as `i64` for `iconst`) of an integer type.
pub(crate) fn int_min_max(it: IntTy) -> (i64, i64) {
    let bits = it.bits().unwrap_or(64);
    if it.is_signed() {
        if bits >= 64 {
            (i64::MIN, i64::MAX)
        } else {
            let m = 1i64 << (bits - 1);
            (-m, m - 1)
        }
    } else if bits >= 64 {
        (0, -1) // u64::MAX is all-ones (read back as i64 = -1)
    } else {
        (0, (1i64 << bits) - 1)
    }
}

pub(crate) fn int_clty(it: IntTy) -> ClType {
    match it {
        IntTy::I8 | IntTy::U8 => types::I8,
        IntTy::I16 | IntTy::U16 => types::I16,
        IntTy::I32 | IntTy::U32 => types::I32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => types::I64,
    }
}

pub(crate) fn float_clty(ft: FloatTy) -> ClType {
    match ft {
        FloatTy::F32 => types::F32,
        FloatTy::F64 => types::F64,
    }
}

/// The in-memory layout of a struct's field block: per-field byte offset and
/// Cranelift type (`None` for zero-sized `null` fields), plus the total size.
pub(crate) struct Layout {
    pub(crate) names: Vec<String>,
    pub(crate) offsets: Vec<u32>,
    pub(crate) cltys: Vec<Option<ClType>>,
    pub(crate) size: u32,
    /// Byte offsets of fields that hold managed pointers (the GC trace map).
    pub(crate) ptr_offsets: Vec<u32>,
}

impl Layout {
    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }
}

pub(crate) fn align_up(x: u32, align: u32) -> u32 {
    x.div_ceil(align) * align
}

/// Is `ty` an immutable, intrinsically-cloneable value? Primitives + `str` +
/// `null` qualify (sharing them is observationally a deep copy, `docs/15` §8).
/// Mirrors `Checker::is_immutable_value` for the codegen-side deep-clone
/// recursion.
pub(crate) fn is_immutable_value_codegen(analysis: &Analysis, ty: Ty) -> bool {
    matches!(
        analysis.tcx.kind(ty),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
    )
}

/// Is a value of `ty` a managed-heap pointer (so the collector must trace it)?
/// Primitives are not; `str`, tuples, unions/`dynamic`, and managed structs
/// (including `List`) are. Foreign (`extern`) structs are not managed.
pub(crate) fn is_managed_ptr(analysis: &Analysis, ty: Ty) -> bool {
    match analysis.tcx.kind(ty) {
        TyKind::Str | TyKind::Tuple(_) | TyKind::Union(_) | TyKind::Dynamic => true,
        // A closure value is a pointer to a managed environment.
        TyKind::Func { is_extern: false, .. } => true,
        TyKind::Named { def, .. } => {
            matches!(analysis.program.def(*def).kind, DefKind::Struct | DefKind::Interface)
        }
        _ => false,
    }
}

/// Compute a field-block layout from named, lowered field types. Field offsets
/// respect each field's natural alignment; the total size is rounded up to the
/// aggregate's alignment (`docs/02` §9). Records which fields are managed
/// pointers for the GC trace map.
pub(crate) fn layout_of_fields(analysis: &Analysis, fields: &[(String, Ty)]) -> Layout {
    let mut offset = 0u32;
    let mut offsets = Vec::new();
    let mut cltys = Vec::new();
    let mut names = Vec::new();
    let mut ptr_offsets = Vec::new();
    let mut max_align = 1u32;
    for (name, ty) in fields {
        let ct = clty_of(analysis, *ty);
        let (size, align) = match ct {
            Some(c) => (c.bytes(), c.bytes().max(1)),
            None => (0, 1),
        };
        offset = align_up(offset, align);
        offsets.push(offset);
        cltys.push(ct);
        names.push(name.clone());
        if is_managed_ptr(analysis, *ty) {
            ptr_offsets.push(offset);
        }
        offset += size;
        max_align = max_align.max(align);
    }
    Layout { names, offsets, cltys, size: align_up(offset, max_align).max(1), ptr_offsets }
}

/// The field-block layout of a (non-generic) struct, by its recorded fields.
pub(crate) fn compute_layout(analysis: &Analysis, def: DefId, args: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> = match analysis.results.struct_fields.get(&def) {
        Some(StructFields::Record(fs)) => fs.clone(),
        Some(StructFields::Tuple(ts)) => {
            ts.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect()
        }
        _ => Vec::new(),
    };
    // For a generic struct, the field types reference the struct's own
    // parameters; substitute the instantiation's arguments.
    let ssubst: HashMap<DefId, Ty> =
        analysis.program.def(def).generics.iter().copied().zip(args.iter().copied()).collect();
    let resolved: Vec<(String, Ty)> = fields
        .into_iter()
        .map(|(n, t)| (n, resolve_shallow(analysis, t, &ssubst)))
        .collect();
    layout_of_fields(analysis, &resolved)
}

/// The layout of an anonymous tuple, positions named "0", "1", ….
pub(crate) fn tuple_layout(analysis: &Analysis, elems: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> =
        elems.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect();
    layout_of_fields(analysis, &fields)
}

/// Process the supported backslash escapes of a string-literal text run.
pub(crate) fn unescape_into(text: &str, out: &mut Vec<u8>) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('\'') => out.push(b'\''),
            Some('"') => out.push(b'"'),
            Some('$') => out.push(b'$'),
            Some('0') => out.push(0),
            Some('u') => {
                // \u{H..} — consume the brace-delimited hex.
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    while let Some(&h) = chars.peek() {
                        if h == '}' { chars.next(); break; }
                        hex.push(h);
                        chars.next();
                    }
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => {}
        }
    }
}

/// Decode a `char` literal (with surrounding quotes) to its scalar value.
pub(crate) fn parse_char(raw: &str) -> Option<u32> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        return if chars.next().is_none() { Some(first as u32) } else { None };
    }
    let esc = chars.next()?;
    let val = match esc {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '\\' => '\\' as u32,
        '\'' => '\'' as u32,
        '"' => '"' as u32,
        '0' => 0,
        'u' => {
            // \u{...}
            let rest: String = chars.collect();
            let hex = rest.strip_prefix('{')?.strip_suffix('}')?;
            return u32::from_str_radix(hex, 16).ok();
        }
        _ => return None,
    };
    if chars.next().is_none() { Some(val) } else { None }
}

