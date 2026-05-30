//! The type checker.
//!
//! Bidirectional checking: every expression is checked against an *expected*
//! type when one is known (which drives integer/float literal defaulting and
//! union widening) and otherwise *synthesizes* its own type. Assignability
//! follows `docs/03` (union widening is implicit, narrowing is not) and
//! `docs/02`/`docs/12` (no implicit numeric coercion, `dynamic` accepts any
//! value but yields nothing without a narrow).
//!
//! This module grows feature-by-feature; the currently supported surface is
//! the imperative core (literals, locals, operators, blocks, `if`, `return`,
//! direct calls). Unsupported forms emit an explicit diagnostic rather than
//! silently accepting, so the gap is always visible.

use crate::ast::*;
use crate::ids::{DefId, LocalId, ModId};
use crate::sema::diag::{SemaError, SemaErrorKind};
use crate::sema::lower::{Lowerer, TypeEnv};
use crate::sema::results::{Adjust, Builtin, TryBranch, ValueRes};
use crate::sema::symbols::{DefKind, Program};
use crate::span::Span;
use crate::token::IntBase;
use crate::ty::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use std::collections::HashMap;

/// Type checks the bodies of every function and extend-method in a program.
pub struct Checker<'a> {
    prog: &'a Program,
    tcx: &'a mut TyCtxt,
    errors: &'a mut Vec<SemaError>,
    /// Lexical scopes of local bindings (name → type + id), innermost last.
    scopes: Vec<HashMap<String, (Ty, LocalId)>>,
    /// The enclosing function's declared return type.
    ret_ty: Ty,
    /// Stack of enclosing loops, for `break`/`continue` checking. `is_loop`
    /// distinguishes `loop` (break may carry a value) from `while`/`for`.
    loops: Vec<LoopFrame>,
    /// Next local id to hand out within the current function body.
    next_local: u32,
    /// The `self` binding's local id in the current method body, if any.
    self_local: Option<LocalId>,
    /// The module whose scope name resolution uses — set to the module owning
    /// the function currently being checked.
    cur_module: ModId,
    /// The current function's in-scope generic parameters (`name → Param(def)`)
    /// and `Self` type, so body type annotations and `T.static_method()` calls
    /// resolve generics. Set per function in [`Checker::check_function`].
    cur_generics: HashMap<String, Ty>,
    cur_self_ty: Option<Ty>,
    /// Active flow-narrowing overrides: a local's type within the current
    /// branch (`if x is T { … }` narrows `x` to `T`). Restored on branch exit.
    narrowings: HashMap<LocalId, Ty>,
    /// Stack of enclosing closures being checked, for capture analysis. A local
    /// referenced inside a closure whose id predates the closure is captured.
    closure_stack: Vec<ClosureFrame>,
    /// Whether the body currently being checked is an `async` context (an
    /// `async` function/closure body or a bare `async { … }` block) — i.e.
    /// whether `await` is permitted here (`docs/21` §4).
    in_async: bool,
    /// Transient hand-off slots: a check method stashes the datum it just
    /// computed for the expression node being checked, and `build_hir_node`
    /// consumes it the instant `check_expr_inner` returns for the same node.
    /// Because construction is synchronous and depth-first (every child node is
    /// built — and its own slot consumed — before its parent's check method
    /// sets these), one slot per fact suffices and no persistent span-keyed side
    /// table is kept; the datum lives only on the resulting HIR node field.
    /// (Replaces `operator_methods` / `cast_targets` / `awaits` / `async_spawns`
    /// / `try_branches` / `residual_conversions`.)
    pending_overload: std::cell::Cell<Option<crate::hir::OpOverload>>,
    pending_cast_target: std::cell::Cell<Option<Ty>>,
    pending_await: std::cell::Cell<Option<Ty>>,
    pending_spawn: std::cell::Cell<Option<Ty>>,
    pending_try_branch: std::cell::Cell<Option<TryBranch>>,
    pending_residuals: std::cell::Cell<Option<Vec<(Ty, crate::ids::DefId, Ty)>>>,
    pending_clone_kind: std::cell::Cell<Option<crate::sema::results::CloneKind>>,
    /// A static-method call's solved receiver type (was `static_calls` +
    /// `static_recv`): `Some(recv)` marks the call static.
    pending_static_recv: std::cell::Cell<Option<Ty>>,
    pending_foreign_flex: std::cell::Cell<Option<(Ty, Ty)>>,
    pending_for_driver: std::cell::Cell<Option<crate::hir::ForDriver>>,
    /// A generic call's solved type arguments (was the call-keyed use of
    /// `call_type_args`), consumed by `build_call_kind`.
    pending_type_args: std::cell::Cell<Option<Vec<Ty>>>,
    /// Per-hole `(to_str method, targs)` for a string literal's interpolation
    /// holes, in source order (was `stringify_methods` + the hole-keyed use of
    /// `call_type_args`). `build_hir_node`'s `Str` arm pops them in order.
    pending_stringify:
        std::cell::Cell<Option<std::collections::VecDeque<(Option<crate::ids::DefId>, Vec<Ty>)>>>,
    /// A closure / `async` block's resolved capture+param info (was the
    /// `closures` / `async_blocks` tables), consumed by `build_hir_node`.
    pending_closure: std::cell::Cell<Option<crate::sema::results::ClosureInfo>>,
    pending_async: std::cell::Cell<Option<crate::sema::results::AsyncInfo>>,
    /// The typed HIR the checker emits directly: the def-keyed maps
    /// (`structs`/`fn_sigs`/`extern_sigs`/`iface_impls`/`local_decls`/
    /// `local_types`) are filled as the checker resolves each definition;
    /// [`Checker::finish`] then assembles the bodies + link libs. There is no
    /// `CheckResults` side-table struct any more.
    pub hir: crate::hir::Hir,
    /// Working accumulator: the typed HIR node built for each expression as it is
    /// checked, keyed by span. Parents read their already-built children from
    /// here (`hir_child`); `finish` assembles function bodies from it. Dropped
    /// after the HIR is assembled — not part of the emitted program.
    node_hir: HashMap<Span, crate::hir::Expr>,
    /// Working accumulator: each function/method's built body `Block`, by def.
    fn_bodies: HashMap<DefId, crate::hir::Block>,
}

/// A closure being checked: the first local id it owns (params/body locals have
/// ids `>= first_local`), and the enclosing locals it has captured so far.
struct ClosureFrame {
    first_local: u32,
    captures: Vec<(LocalId, Ty)>,
}

impl<'a> Checker<'a> {
    pub fn new(
        prog: &'a Program,
        tcx: &'a mut TyCtxt,
        errors: &'a mut Vec<SemaError>,
    ) -> Self {
        let null = tcx.null;
        Checker {
            prog,
            tcx,
            errors,
            scopes: Vec::new(),
            ret_ty: null,
            loops: Vec::new(),
            next_local: 0,
            self_local: None,
            cur_module: ModId::ROOT,
            cur_generics: HashMap::new(),
            cur_self_ty: None,
            narrowings: HashMap::new(),
            closure_stack: Vec::new(),
            in_async: false,
            pending_overload: std::cell::Cell::new(None),
            pending_cast_target: std::cell::Cell::new(None),
            pending_await: std::cell::Cell::new(None),
            pending_spawn: std::cell::Cell::new(None),
            pending_try_branch: std::cell::Cell::new(None),
            pending_residuals: std::cell::Cell::new(None),
            pending_clone_kind: std::cell::Cell::new(None),
            pending_static_recv: std::cell::Cell::new(None),
            pending_foreign_flex: std::cell::Cell::new(None),
            pending_for_driver: std::cell::Cell::new(None),
            pending_type_args: std::cell::Cell::new(None),
            pending_stringify: std::cell::Cell::new(None),
            pending_closure: std::cell::Cell::new(None),
            pending_async: std::cell::Cell::new(None),
            hir: crate::hir::Hir::new(),
            node_hir: HashMap::new(),
            fn_bodies: HashMap::new(),
        }
    }

    /// What the value-position name / callee / binding at `span` resolves to,
    /// read off the `Name` HIR node the checker recorded there.
    pub(crate) fn resolution(&self, span: Span) -> Option<ValueRes> {
        match &self.node_hir.get(&span)?.kind {
            crate::hir::ExprKind::Name(res) => Some(*res),
            crate::hir::ExprKind::Adjust { expr, .. } => match &expr.kind {
                crate::hir::ExprKind::Name(res) => Some(*res),
                _ => None,
            },
            _ => None,
        }
    }

    /// The checked type of the expression at `span`, read off its built HIR node
    /// (a baked `Adjust` carries the post-coercion type, so unwrap it).
    pub(crate) fn expr_ty(&self, span: Span) -> Option<Ty> {
        self.node_hir.get(&span).map(|n| match &n.kind {
            crate::hir::ExprKind::Adjust { expr, .. } => expr.ty,
            _ => n.ty,
        })
    }

    /// Check every checkable definition in the program.
    pub fn check_program(&mut self) {
        self.collect_struct_layouts();
        self.collect_impls();
        self.validate_extern_structs();
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            match self.prog.def(def).kind {
                // Free functions and `extend` methods share the function checker;
                // methods additionally bind `self` and the extend's generics.
                DefKind::Function | DefKind::ExtendMethod => self.check_function(def),
                // A `test "name" { … }` body: a zero-arg unit body (`docs/23`).
                DefKind::Test => self.check_test(def),
                // Record each extern function's C-ABI signature for codegen.
                DefKind::ExternFunction => self.record_extern_sig(def),
                _ => {}
            }
        }
    }

    /// Consume the checker and produce the complete [`crate::hir::Hir`]. The
    /// def-keyed maps were filled during checking; here we add the
    /// `@Link`-derived libraries and assemble each function's `Body` from the
    /// checker-built block plus its locals (walked out of the block).
    pub fn finish(mut self) -> crate::hir::Hir {
        use crate::hir::Body;
        self.hir.link_libs = crate::hir::collect_link_libs(self.prog);
        let null = self.tcx.null;
        let err = self.tcx.error;

        // Collect (def, params, ret, async_output, block, span) first, draining
        // the per-body blocks, so the locals walk below borrows `hir.local_types`
        // without conflicting with the `fn_bodies` drain.
        let mut pending: Vec<(DefId, Vec<LocalId>, Ty, Option<Ty>, crate::hir::Block, Span)> =
            Vec::new();
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            let d = self.prog.def(def);
            if !matches!(d.kind, DefKind::Function | DefKind::ExtendMethod | DefKind::Test) {
                continue;
            }
            // The body span comes from the function body or the test block.
            let span = match &d.item {
                Some(ItemKind::Function(f)) => match &f.body {
                    Some(b) => b.span,
                    None => continue,
                },
                Some(ItemKind::Test(t)) => t.body.span,
                _ => continue,
            };
            let (params, ret, async_output) = match self.hir.fn_sigs.get(&def) {
                Some(s) => (
                    s.params.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
                    s.ret,
                    s.async_output,
                ),
                None => (Vec::new(), null, None),
            };
            // `build_block` is total, so there is always a block; default to an
            // empty one only for the unreachable bodyless case.
            let block = self.fn_bodies.remove(&def).unwrap_or(crate::hir::Block {
                stmts: Vec::new(),
                trailing: None,
                ty: null,
                span,
            });
            pending.push((def, params, ret, async_output, block, span));
        }

        for (def, params, ret, async_output, block, span) in pending {
            let mut locals = HashMap::new();
            for &l in &params {
                record_local(l, &self.hir.local_types, err, &mut locals);
            }
            record_block_locals(&block, &self.hir.local_types, err, &mut locals);
            self.hir.bodies.insert(
                def,
                Body { def, params, locals, ret, async_output, block, span },
            );
        }
        self.hir
    }

    /// Lower and record an `extern function`'s parameter and return types, so
    /// the code generator can build its C-ABI signature without a body.
    pub(crate) fn record_extern_sig(&mut self, def: DefId) {
        self.cur_module = self.prog.def(def).module;
        let env = self.def_env(def, None);
        let Some(ItemKind::Extern(ExternItem::Function(f))) = self.prog.def(def).item.clone()
        else {
            return;
        };
        let mut ptys = Vec::new();
        for p in &f.params {
            if let ParamKind::Normal { ty, .. } = &p.kind {
                ptys.push(self.lower_ty(ty, &env));
            }
        }
        let rty = match &f.return_type {
            Some(t) => self.lower_ty(t, &env),
            None => self.tcx.null,
        };
        self.hir.extern_sigs.insert(def, crate::hir::ExternSig { params: ptys, ret: rty });
        // `@Link(lib = "…")` libraries (`docs/19` §13) are no longer recorded
        // here: they are derived from the program's attributes by
        // `hir::collect_link_libs` (consumed as `Hir::link_libs`).
    }

    /// Whether `ty` is a C-ABI-compatible (`ReprC`) field type for an extern
    /// struct (`docs/19` §3): the numeric primitives, `bool`/`char`, raw
    /// pointers `*T`, C function pointers, fixed arrays `[T; N]`, and *nested*
    /// extern structs (embedded inline). `str`, managed structs, tuples,
    /// unions, and closures are not C-ABI-compatible.
    pub(crate) fn is_repr_c(&self, ty: Ty) -> bool {
        match self.tcx.kind(ty) {
            TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::Ptr(_)
            // A C function pointer — `extern (..) => R`.
            | TyKind::Func { is_extern: true, .. } => true,
            // A fixed-size array `[T; N]` whose element is itself ReprC
            // (`docs/19` §4) — only valid as an extern field.
            TyKind::Array { elem, .. } => self.is_repr_c(*elem),
            // A nested `extern struct` — embedded inline (`docs/19` §3).
            TyKind::Named { def, .. } => self.prog.def(*def).kind == DefKind::ExternStruct,
            _ => false,
        }
    }

    /// Enforce that every `extern struct` field is C-ABI-compatible
    /// (`docs/19` §3). An incompatible field would have no sound C layout.
    pub(crate) fn validate_extern_structs(&mut self) {
        use crate::sema::results::StructFields as SF;
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            let kind = self.prog.def(def).kind;
            if !matches!(kind, DefKind::Struct | DefKind::ExternStruct) {
                continue;
            }
            let is_extern = kind == DefKind::ExternStruct;
            let span = self.prog.def(def).span;
            let name = self.prog.def(def).name.clone();
            // `@Transparent` (ABI newtype, `docs/19` §3): the struct must have
            // exactly one field, whose representation/ABI it inherits.
            if self.prog.def(def).attrs.iter().any(|a| a.name.name == "Transparent") {
                match self.hir.structs.get(&def) {
                    Some(SF::Tuple(ts)) if ts.len() == 1 => {}
                    Some(SF::Tuple(ts)) => self.emit(span, SemaErrorKind::Message(format!(
                        "`@Transparent` requires exactly one field, but `{}` has {} \
                         (`docs/19` §3)", name, ts.len()
                    ))),
                    _ => self.emit(span, SemaErrorKind::Message(format!(
                        "`@Transparent` requires a single-field tuple struct, e.g. \
                         `@Transparent struct {}(i32)` (`docs/19` §3)", name
                    ))),
                }
            }
            // The C-layout decorators only apply to `extern struct` (`docs/19` §3).
            if !is_extern {
                for a in &self.prog.def(def).attrs {
                    if matches!(a.name.name.as_str(), "Packed" | "Align" | "Union") {
                        self.emit(a.span, SemaErrorKind::Message(format!(
                            "`@{}` is only valid on an `extern struct` (`docs/19` §3)",
                            a.name.name
                        )));
                    }
                }
                continue;
            }
            let fields: Vec<(String, Ty)> = match self.hir.structs.get(&def) {
                Some(SF::Record(fs)) => fs.clone(),
                Some(SF::Tuple(ts)) => {
                    ts.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect()
                }
                _ => Vec::new(),
            };
            for (fname, fty) in fields {
                if !self.is_repr_c(fty) {
                    self.emit(span, SemaErrorKind::Message(format!(
                        "field `{}` of `extern struct {}` has type `{}`, which is not \
                         C-ABI-compatible; extern struct fields must be numeric, `bool`, \
                         `char`, or a raw pointer `*T` (`docs/19` §3)",
                        fname, name, self.display(fty)
                    )));
                }
            }
        }
    }

    /// Build the interface-implementation table: for every `extend Target: I…`
    /// block, map `(Target's type def, I's def) → extend def`. Codegen uses this
    /// to monomorphize interface-method calls on type parameters.
    pub(crate) fn collect_impls(&mut self) {
        let module = self.current_module();
        let extends = self.prog.visible_extends(module);
        for ext in extends {
            let Some(ItemKind::Extend(e)) = self.prog.def(ext).item.clone() else { continue };
            let mut env = TypeEnv::new(self.prog.def(ext).module);
            for g in self.prog.def(ext).generics.clone() {
                let nm = self.prog.def(g).name.clone();
                let pty = self.tcx.mk_param(g);
                env.generics.insert(nm, pty);
            }
            let target = self.lower_ty(&e.target, &env);
            let TyKind::Named { def: tdef, .. } = self.tcx.kind(target).clone() else { continue };
            for itf in &e.interfaces {
                let TypeKind::Named { name, .. } = &itf.kind else { continue };
                if let Some(idef) = self.prog.resolve_type_in(module, &name.name) {
                    if self.prog.def(idef).kind == DefKind::Interface {
                        self.hir.iface_impls.insert((tdef, idef), ext);
                    }
                }
            }
        }
    }

    /// Record each struct's lowered field-type template for the code generator.
    pub(crate) fn collect_struct_layouts(&mut self) {
        use crate::sema::results::StructFields as SF;
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            if !matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
                continue;
            }
            let Some(ItemKind::Struct(s)) = self.prog.def(def).item.clone() else { continue };
            let env = self.def_env(def, None);
            let fields = match &s.kind {
                StructKind::Unit => SF::Unit,
                StructKind::Tuple(ts) => {
                    SF::Tuple(ts.iter().map(|f| self.lower_ty(&f.ty, &env)).collect())
                }
                StructKind::Record(fs) => SF::Record(
                    fs.iter()
                        .map(|f| (f.name.name.clone(), self.lower_ty(&f.ty, &env)))
                        .collect(),
                ),
            };
            self.hir.structs.insert(def, fields);
        }
    }

}


/// A loop on the checker's enclosing-loop stack.
struct LoopFrame {
    /// `true` for `loop` (a `break` may carry a value), `false` for `while`/`for`.
    is_loop: bool,
    /// Types of the `break` expressions seen so far (for `loop`'s result type).
    break_types: Vec<Ty>,
}

fn binop_str(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
    }
}


mod functions;
mod stmt;
mod expr;
mod structs;
mod control;
mod builtins;
mod methods;
mod helpers;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Body-locals collection (used by `Checker::finish` to fill `Body.locals` from
// the checker-built HIR block). Walks the typed HIR recording every bound or
// referenced local with its type.
// ---------------------------------------------------------------------------

fn record_local(
    id: LocalId,
    local_types: &HashMap<LocalId, Ty>,
    err: Ty,
    out: &mut HashMap<LocalId, Ty>,
) {
    if let Some(ty) = local_types.get(&id) {
        out.insert(id, *ty);
    } else {
        out.entry(id).or_insert(err);
    }
}

fn record_block_locals(
    b: &crate::hir::Block,
    local_types: &HashMap<LocalId, Ty>,
    err: Ty,
    out: &mut HashMap<LocalId, Ty>,
) {
    use crate::hir::StmtKind as S;
    for s in &b.stmts {
        match &s.kind {
            S::Let { pattern, init } => {
                record_pattern_locals(pattern, local_types, err, out);
                record_node_locals(init, local_types, err, out);
            }
            S::Assign { target, value } => {
                record_node_locals(target, local_types, err, out);
                record_node_locals(value, local_types, err, out);
            }
            S::Expr(e) => record_node_locals(e, local_types, err, out),
            S::Item(_) => {}
        }
    }
    if let Some(t) = &b.trailing {
        record_node_locals(t, local_types, err, out);
    }
}

fn record_pattern_locals(
    p: &crate::hir::Pattern,
    local_types: &HashMap<LocalId, Ty>,
    err: Ty,
    out: &mut HashMap<LocalId, Ty>,
) {
    use crate::hir::PatternKind as P;
    match &p.kind {
        P::Bind(id) => record_local(*id, local_types, err, out),
        P::TypeBind { bind, .. } => {
            if let Some(id) = bind {
                record_local(*id, local_types, err, out);
            }
        }
        P::Literal(e) => record_node_locals(e, local_types, err, out),
        P::TupleStruct { fields, rest, .. } => {
            fields.iter().for_each(|f| record_pattern_locals(f, local_types, err, out));
            if let Some(r) = rest {
                if let Some(id) = r.bind {
                    record_local(id, local_types, err, out);
                }
            }
        }
        P::RecordStruct { fields, .. } => {
            fields.iter().for_each(|f| record_pattern_locals(&f.pattern, local_types, err, out))
        }
        P::Tuple { elems, rest } | P::List { elems, rest } => {
            elems.iter().for_each(|e| record_pattern_locals(e, local_types, err, out));
            if let Some((_, r)) = rest {
                if let Some(id) = r.bind {
                    record_local(id, local_types, err, out);
                }
            }
        }
        P::Or(ps) => ps.iter().for_each(|p| record_pattern_locals(p, local_types, err, out)),
        P::Wildcard | P::UnitPath { .. } => {}
    }
}

fn record_node_locals(
    e: &crate::hir::Expr,
    local_types: &HashMap<LocalId, Ty>,
    err: Ty,
    out: &mut HashMap<LocalId, Ty>,
) {
    use crate::hir::{CallKind, ExprKind as K, MapEntry, StrPart};
    let rec = |x, out: &mut HashMap<LocalId, Ty>| record_node_locals(x, local_types, err, out);
    if let K::Name(ValueRes::Local(id)) = &e.kind {
        record_local(*id, local_types, err, out);
    }
    match &e.kind {
        K::Tuple(xs) | K::List(xs) => xs.iter().for_each(|x| rec(x, out)),
        K::Unary { operand, .. } => rec(operand, out),
        K::Binary { left, right, .. } => {
            rec(left, out);
            rec(right, out);
        }
        K::Cast { expr, .. }
        | K::Ref(expr)
        | K::Deref(expr)
        | K::Adjust { expr, .. }
        | K::Try { expr, .. }
        | K::Await { expr, .. }
        | K::Spawn { expr, .. }
        | K::Field { receiver: expr, .. }
        | K::TupleIndex { receiver: expr, .. } => rec(expr, out),
        K::Index { receiver, index } => {
            rec(receiver, out);
            rec(index, out);
        }
        K::Return(v) | K::Break(v) => {
            if let Some(e) = v {
                rec(e, out);
            }
        }
        K::Call { args, kind, .. } => {
            if let CallKind::Closure { callee } = kind {
                rec(callee, out);
            }
            args.iter().for_each(|a| rec(a, out));
        }
        K::Intrinsic { args, .. } => args.iter().for_each(|a| rec(a, out)),
        K::Struct { fields, spread, .. } => {
            fields.iter().for_each(|f| rec(&f.value, out));
            if let Some(s) = spread {
                rec(s, out);
            }
        }
        K::Str(parts) => parts.iter().for_each(|p| {
            if let StrPart::Interp { expr, .. } = p {
                rec(expr, out);
            }
        }),
        K::Map(items) => items.iter().for_each(|it| match it {
            MapEntry::Kv { key, value } => {
                rec(key, out);
                rec(value, out);
            }
            MapEntry::Spread(e) => rec(e, out),
        }),
        K::If { cond, then_block, else_branch } => {
            rec(cond, out);
            record_block_locals(then_block, local_types, err, out);
            if let Some(e) = else_branch {
                rec(e, out);
            }
        }
        K::Match { scrutinee, arms } => {
            rec(scrutinee, out);
            for a in arms {
                record_pattern_locals(&a.pattern, local_types, err, out);
                if let Some(g) = &a.guard {
                    rec(g, out);
                }
                rec(&a.body, out);
            }
        }
        K::Block(b) | K::Loop(b) => record_block_locals(b, local_types, err, out),
        K::While { cond, body } => {
            rec(cond, out);
            record_block_locals(body, local_types, err, out);
        }
        K::For { pattern, iter, body, .. } => {
            record_pattern_locals(pattern, local_types, err, out);
            rec(iter, out);
            record_block_locals(body, local_types, err, out);
        }
        K::Closure { params, captures, body, .. } => {
            for (id, _) in params.iter().chain(captures) {
                record_local(*id, local_types, err, out);
            }
            rec(body, out);
        }
        K::AsyncBlock { params, captures, body, .. } => {
            for (id, _) in params.iter().chain(captures) {
                record_local(*id, local_types, err, out);
            }
            record_block_locals(body, local_types, err, out);
        }
        K::Int(_) | K::Float(_) | K::Bool(_) | K::Null | K::Char(_) | K::Name(_)
        | K::Discard | K::Continue | K::Error => {}
    }
}
