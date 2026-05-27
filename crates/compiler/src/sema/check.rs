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
use crate::sema::results::{Adjust, Builtin, CheckResults, ValueRes};
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
    /// Side tables recorded for downstream phases.
    pub results: CheckResults,
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
            results: CheckResults::new(),
        }
    }

    /// Check every checkable definition in the program.
    pub fn check_program(&mut self) {
        self.collect_struct_layouts();
        self.collect_impls();
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            match self.prog.def(def).kind {
                // Free functions and `extend` methods share the function checker;
                // methods additionally bind `self` and the extend's generics.
                DefKind::Function | DefKind::ExtendMethod => self.check_function(def),
                // Record each extern function's C-ABI signature for codegen.
                DefKind::ExternFunction => self.record_extern_sig(def),
                _ => {}
            }
        }
    }

    /// Lower and record an `extern function`'s parameter and return types, so
    /// the code generator can build its C-ABI signature without a body.
    fn record_extern_sig(&mut self, def: DefId) {
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
        self.results.extern_sigs.insert(def, (ptys, rty));
    }

    /// Build the interface-implementation table: for every `extend Target: I…`
    /// block, map `(Target's type def, I's def) → extend def`. Codegen uses this
    /// to monomorphize interface-method calls on type parameters.
    fn collect_impls(&mut self) {
        let module = self.current_module();
        let extends = self.prog.module(module).extends.clone();
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
                        self.results.iface_impls.insert((tdef, idef), ext);
                    }
                }
            }
        }
    }

    /// Record each struct's lowered field-type template for the code generator.
    fn collect_struct_layouts(&mut self) {
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
            self.results.struct_fields.insert(def, fields);
        }
    }

    // -- functions -----------------------------------------------------------

    pub fn check_function(&mut self, def: DefId) {
        let Some(ItemKind::Function(f)) = self.prog.def(def).item.clone() else {
            return;
        };
        // Resolve names against the module that owns this function.
        self.cur_module = self.prog.def(def).module;
        let (env, self_ty) = self.fn_env(def);
        // Make the function's generics and `Self` visible to body annotations
        // and `T.static_method()` calls (via `local_env`).
        self.cur_generics = env.generics.clone();
        self.cur_self_ty = env.self_ty;
        let ret_ty = match &f.return_type {
            Some(t) => self.lower_ty(t, &env),
            None => self.tcx.null,
        };
        // An `async` function declares its return type as `Future<Output>`; the
        // body itself yields `Output` (`docs/21` §3). Inside the body `await` is
        // allowed and `return e` returns `Output`. We record the output for
        // codegen (which lowers the body to a `Future` state machine) and check
        // the body against it.
        let prev_async = self.in_async;
        let body_ret = if f.is_async {
            match self.future_output(ret_ty) {
                Some(out) => {
                    self.in_async = true;
                    self.results.async_fns.insert(def, out);
                    out
                }
                None => {
                    self.emit(
                        f.return_type.as_ref().map_or(self.prog.def(def).span, |t| t.span),
                        SemaErrorKind::Message(
                            "an `async` function must declare its return type as \
                             `Future<Output>`"
                                .into(),
                        ),
                    );
                    self.in_async = true;
                    ret_ty
                }
            }
        } else {
            ret_ty
        };
        self.ret_ty = body_ret;
        self.results.fn_return.insert(def, ret_ty);
        self.scopes.clear();
        // `next_local` is NOT reset per function: local ids must be globally
        // unique because `results.local_types` is a program-wide map.
        self.self_local = None;
        self.push_scope();
        let mut param_locals = Vec::new();
        for p in &f.params {
            match &p.kind {
                ParamKind::SelfParam => {
                    // `self` binds to the receiver type; offset 0 in the params.
                    let sty = self_ty.unwrap_or(self.tcx.error);
                    let id = self.bind("self", p.span, sty);
                    self.self_local = Some(id);
                    param_locals.push(id);
                }
                ParamKind::Normal { name, ty } => {
                    let pty = self.lower_ty(ty, &env);
                    param_locals.push(self.bind(&name.name, name.span, pty));
                }
            }
        }
        self.results.fn_params.insert(def, param_locals);
        if let Some(body) = &f.body {
            let bty = self.check_block(body, Some(body_ret));
            // The body block's value is the function's result (the future's
            // `Output` for an async body).
            self.expect(bty, body_ret, body.span);
        }
        self.in_async = prev_async;
        self.pop_scope();
    }

    /// Build the lowering env for a function-like def, plus its `self` type if
    /// it is a method (its parent is an `extend` block). The env carries the
    /// extend's generics and `Self`, then the method's own generics.
    fn fn_env(&mut self, def: DefId) -> (TypeEnv, Option<Ty>) {
        if let Some(parent) = self.prog.def(def).parent {
            if self.prog.def(parent).kind == DefKind::Extend {
                if let Some(ItemKind::Extend(e)) = self.prog.def(parent).item.clone() {
                    let mut env = TypeEnv::new(self.prog.def(parent).module);
                    // Extend-level generics.
                    for g in self.prog.def(parent).generics.clone() {
                        let name = self.prog.def(g).name.clone();
                        let pty = self.tcx.mk_param(g);
                        env.generics.insert(name, pty);
                    }
                    let self_ty = self.lower_ty(&e.target, &env);
                    env.self_ty = Some(self_ty);
                    // Method-level generics.
                    for g in self.prog.def(def).generics.clone() {
                        let name = self.prog.def(g).name.clone();
                        let pty = self.tcx.mk_param(g);
                        env.generics.insert(name, pty);
                    }
                    return (env, Some(self_ty));
                }
            }
        }
        (self.def_env(def, None), None)
    }

    /// Build the type-lowering environment for a definition: its own generic
    /// parameters mapped to `Param(def)`, in its module.
    fn def_env(&mut self, def: DefId, self_ty: Option<Ty>) -> TypeEnv {
        let module = self.prog.def(def).module;
        let mut env = TypeEnv::new(module);
        env.self_ty = self_ty;
        let gens = self.prog.def(def).generics.clone();
        for g in gens {
            let name = self.prog.def(g).name.clone();
            let pty = self.tcx.mk_param(g);
            env.generics.insert(name, pty);
        }
        env
    }

    /// Substitute generic parameters in `ty` using a `Param-def → type` map.
    fn subst_ty(&mut self, ty: Ty, map: &HashMap<DefId, Ty>) -> Ty {
        if map.is_empty() {
            return ty;
        }
        match self.tcx.kind(ty).clone() {
            TyKind::Param(d) => map.get(&d).copied().unwrap_or(ty),
            TyKind::Named { def, args } => {
                let nargs: Vec<Ty> = args.iter().map(|a| self.subst_ty(*a, map)).collect();
                self.tcx.mk_named(def, nargs)
            }
            TyKind::Tuple(es) => {
                let ne: Vec<Ty> = es.iter().map(|e| self.subst_ty(*e, map)).collect();
                self.tcx.mk_tuple(ne)
            }
            TyKind::Func { params, ret, is_extern } => {
                let np: Vec<Ty> = params.iter().map(|p| self.subst_ty(*p, map)).collect();
                let nr = self.subst_ty(ret, map);
                self.tcx.mk_func(np, nr, is_extern)
            }
            TyKind::Union(ms) => {
                let nm: Vec<Ty> = ms.iter().map(|m| self.subst_ty(*m, map)).collect();
                self.tcx.mk_union(nm)
            }
            TyKind::Ptr(i) => {
                let n = self.subst_ty(i, map);
                self.tcx.mk_ptr(n)
            }
            TyKind::Array { elem, len } => {
                let e = self.subst_ty(elem, map);
                self.tcx.intern(TyKind::Array { elem: e, len })
            }
            _ => ty,
        }
    }

    /// Unify a parameter type (which may contain `Param`s) against a concrete
    /// argument type, recording inferred bindings into `map`.
    fn unify(&self, pat: Ty, val: Ty, map: &mut HashMap<DefId, Ty>) {
        match (self.tcx.kind(pat).clone(), self.tcx.kind(val).clone()) {
            (TyKind::Param(d), _) => {
                map.entry(d).or_insert(val);
            }
            (TyKind::Named { def: d1, args: a1 }, TyKind::Named { def: d2, args: a2 })
                if d1 == d2 && a1.len() == a2.len() =>
            {
                for (p, v) in a1.iter().zip(a2) {
                    self.unify(*p, v, map);
                }
            }
            (TyKind::Tuple(p), TyKind::Tuple(v)) if p.len() == v.len() => {
                for (a, b) in p.iter().zip(v) {
                    self.unify(*a, b, map);
                }
            }
            (TyKind::Func { params: pp, ret: pr, .. }, TyKind::Func { params: vp, ret: vr, .. })
                if pp.len() == vp.len() =>
            {
                for (a, b) in pp.iter().zip(vp) {
                    self.unify(*a, b, map);
                }
                self.unify(pr, vr, map);
            }
            (TyKind::Ptr(p), TyKind::Ptr(v)) => self.unify(p, v, map),
            _ => {}
        }
    }

    // -- statements ----------------------------------------------------------

    fn check_block(&mut self, block: &Block, expected: Option<Ty>) -> Ty {
        self.push_scope();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        let ty = match &block.trailing {
            Some(e) => {
                let t = self.check_expr(e, expected);
                // Coerce the tail to the block's expected type if it widens,
                // recording the adjustment at the tail expression's span.
                if let Some(exp) = expected {
                    self.expect(t, exp, e.span);
                    if self.assignable(t, exp) && t != exp {
                        // Report the block's type as the (widened) expected one
                        // so callers don't double-coerce.
                        if matches!(self.tcx.kind(exp), TyKind::Union(_) | TyKind::Dynamic) {
                            self.pop_scope();
                            return exp;
                        }
                    }
                }
                t
            }
            None => self.tcx.null,
        };
        self.pop_scope();
        ty
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Var(local) => {
                let env = self.local_env();
                let annotated = local.ty.as_ref().map(|t| self.lower_ty(t, &env));
                let init_ty = self.check_expr(&local.init, annotated);
                let binding_ty = annotated.unwrap_or(init_ty);
                if let Some(ann) = annotated {
                    self.expect(init_ty, ann, local.init.span);
                }
                self.bind_pattern(&local.pattern, binding_ty);
            }
            StmtKind::Assign { target, value } => {
                let target_ty = self.check_lvalue(target);
                let v = self.check_expr(value, Some(target_ty));
                self.expect(v, target_ty, value.span);
            }
            StmtKind::Expr(e) => {
                let t = self.check_expr(e, None);
                // "Forgot to await" lint (`docs/21` §5): a `Future` produced as a
                // statement and silently discarded is almost always a bug — it
                // never runs. `await`ing it, `spawn`ing it (yields a
                // `JoinHandle`, not a `Future`), or binding it (`var _ = …`)
                // all avoid this. An `await` expression statement is fine even
                // when its own result is a future.
                if self.is_future_ty(t) && !matches!(e.kind, ExprKind::Await { .. }) {
                    self.emit(e.span, SemaErrorKind::Message(
                        "this `Future` is created but never used — `await` it, `spawn` \
                         it, or bind it with `var _ = …`"
                            .into(),
                    ));
                }
            }
            StmtKind::Item(_) => {
                // Nested item declarations are collected/checked separately.
            }
        }
    }

    /// Check an assignment target and return the type it holds.
    fn check_lvalue(&mut self, target: &Expr) -> Ty {
        match &target.kind {
            ExprKind::Ident(name) => match self.lookup(&name.name) {
                Some((ty, id)) => {
                    self.results.resolutions.insert(target.span, ValueRes::Local(id));
                    ty
                }
                None => {
                    self.emit(target.span, SemaErrorKind::UnknownValue {
                        name: name.name.clone(),
                    });
                    self.tcx.error
                }
            },
            ExprKind::Underscore => self.tcx.error, // discard; accepts anything
            // Field and tuple-index targets are checked as expressions; the
            // result type is what the assigned value must satisfy.
            ExprKind::Field { receiver, name } => self.check_field(receiver, name, target.span),
            ExprKind::TupleIndex { receiver, index, index_span } => {
                self.check_tuple_index(receiver, *index, *index_span)
            }
            ExprKind::Index { receiver, index } => self.check_index(receiver, index),
            _ => self.check_expr(target, None),
        }
    }

    // -- expressions ---------------------------------------------------------

    fn check_expr(&mut self, expr: &Expr, expected: Option<Ty>) -> Ty {
        let ty = self.check_expr_inner(expr, expected);
        self.results.expr_types.insert(expr.span, ty);
        ty
    }

    fn check_expr_inner(&mut self, expr: &Expr, expected: Option<Ty>) -> Ty {
        match &expr.kind {
            ExprKind::Int(lit) => self.check_int_lit(lit, expected, expr.span),
            ExprKind::Float(lit) => self.check_float_lit(lit, expected),
            ExprKind::Bool(_) => self.tcx.bool,
            ExprKind::Null => self.tcx.null,
            ExprKind::Char(_) => self.tcx.char,
            ExprKind::Str(s) => {
                // Type-check interpolation holes. Each must be stringifiable;
                // full `ToStr` dispatch arrives with interfaces — for now the
                // primitives that `as str` covers are accepted.
                for part in &s.parts {
                    let (pty, pspan) = match part {
                        StringPart::Expr(e) => (self.check_expr(e, None), e.span),
                        StringPart::Ident(id) => {
                            let t = self.check_ident(&id.name, id.span);
                            // `check_ident` bypasses the `check_expr` wrapper, so
                            // record the type for codegen's stringify lookup.
                            self.results.expr_types.insert(id.span, t);
                            (t, id.span)
                        }
                        StringPart::Text { .. } => continue,
                    };
                    if !self.tcx.is_error(pty) && !self.is_stringifiable(pty) {
                        // A user type is interpolatable if it has a
                        // `to_str(self): str` method (hand-written or derived
                        // via `@Derive(ToStr)`) — the `ToStr` protocol of
                        // `docs/01` §8.
                        if let Some(mdef) = self.tostr_method(pty) {
                            self.results.stringify_methods.insert(pspan, mdef);
                        } else {
                            let t = self.display(pty);
                            self.emit(pspan, SemaErrorKind::Message(format!(
                                "cannot interpolate `{t}`: it has no `to_str(): str` \
                                 method (add one or `@Derive(ToStr)`)"
                            )));
                        }
                    }
                }
                self.tcx.str
            }
            ExprKind::Ident(name) => self.check_ident(&name.name, expr.span),
            ExprKind::SelfExpr => match (self.self_local, self.lookup("self")) {
                (Some(id), Some((ty, _))) => {
                    self.results.resolutions.insert(expr.span, ValueRes::Local(id));
                    // `self` used inside a closure / `async { … }` block is a
                    // capture, like any other enclosing local.
                    self.record_capture(id, ty);
                    ty
                }
                _ => {
                    self.emit(expr.span, SemaErrorKind::Message(
                        "`self` is only valid inside a method".into(),
                    ));
                    self.tcx.error
                }
            },
            ExprKind::Paren(inner) => self.check_expr(inner, expected),
            ExprKind::Tuple(elems) => {
                let elem_expected = expected.and_then(|e| match self.tcx.kind(e) {
                    TyKind::Tuple(ts) if ts.len() == elems.len() => Some(ts.clone()),
                    _ => None,
                });
                let tys: Vec<Ty> = elems
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let exp = elem_expected.as_ref().map(|ts| ts[i]);
                        self.check_expr(e, exp)
                    })
                    .collect();
                self.tcx.mk_tuple(tys)
            }
            ExprKind::Unary { op, operand, op_span } => {
                self.check_unary(*op, operand, *op_span)
            }
            ExprKind::Binary { op, left, right, op_span } => {
                self.check_binary(*op, left, right, *op_span)
            }
            ExprKind::Block(b) => self.check_block(b, expected),
            ExprKind::If { cond, then_block, else_branch } => {
                self.check_if(cond, then_block, else_branch.as_ref(), expected)
            }
            ExprKind::Return(value) => {
                let rty = self.ret_ty;
                match value {
                    Some(e) => {
                        let v = self.check_expr(e, Some(rty));
                        self.expect(v, rty, e.span);
                    }
                    None => self.expect(self.tcx.null, rty, expr.span),
                }
                self.tcx.never
            }
            ExprKind::Call { callee, args, generics, trailing_closure } => {
                self.check_call(callee, args, generics, trailing_closure.as_deref(), expr.span)
            }
            ExprKind::StructLit { path, fields, spread } => {
                self.check_struct_lit(path, fields, spread.as_deref(), expected, expr.span)
            }
            ExprKind::Field { receiver, name } => self.check_field(receiver, name, expr.span),
            ExprKind::TupleIndex { receiver, index, index_span } => {
                self.check_tuple_index(receiver, *index, *index_span)
            }
            ExprKind::List(elems) => self.check_list_lit(elems, expected, expr.span),
            ExprKind::MapLit(items) => self.check_map_lit(items, expected, expr.span),
            ExprKind::Index { receiver, index } => self.check_index(receiver, index),
            ExprKind::Cast { op, expr: inner, ty, .. } => {
                self.check_cast(*op, inner, ty, expr.span)
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(scrutinee, arms, expr.span, expected)
            }
            ExprKind::Try { expr: inner, q_span } => self.check_try(inner, *q_span),
            ExprKind::Await { expr: inner, kw_span } => self.check_await(inner, *kw_span),
            ExprKind::AsyncBlock(block) => self.check_async_block(block, expected, expr.span),
            ExprKind::While { cond, body } => {
                let cty = self.check_expr(cond, Some(self.tcx.bool));
                if !self.tcx.is_error(cty) && cty != self.tcx.bool {
                    let found = self.display(cty);
                    self.emit(cond.span, SemaErrorKind::NonBoolCondition { found });
                }
                self.loops.push(LoopFrame { is_loop: false, break_types: Vec::new() });
                self.check_block(body, None);
                self.loops.pop();
                self.tcx.null
            }
            ExprKind::For { pattern, in_async, iter, body } if *in_async => {
                // `for await x in stream` (`docs/21` §10): drive an
                // `AsyncIterator<T>` by awaiting `next_async()` each step.
                if !self.in_async {
                    self.emit(expr.span, SemaErrorKind::Message(
                        "`for await` is only allowed inside an async body".into(),
                    ));
                }
                let ity = self.check_expr(iter, None);
                let elem = match self.async_iterator_elem(ity) {
                    Some(info) => {
                        let elem = info.elem;
                        self.results.for_async_iters.insert(iter.span, info);
                        elem
                    }
                    None => {
                        if !self.tcx.is_error(ity) {
                            self.emit(iter.span, SemaErrorKind::Message(format!(
                                "`{}` is not an async stream: it has no \
                                 `next_async(self): Future<Item<T> | Done>` method",
                                self.display(ity)
                            )));
                        }
                        self.tcx.error
                    }
                };
                self.push_scope();
                self.bind_pattern(pattern, elem);
                self.loops.push(LoopFrame { is_loop: false, break_types: Vec::new() });
                self.check_block(body, None);
                self.loops.pop();
                self.pop_scope();
                self.tcx.null
            }
            ExprKind::For { pattern, iter, body, .. } => {
                let ity = self.check_expr(iter, None);
                let elem = match self.list_elem(ity) {
                    Some(e) => e,
                    None if self.tcx.is_error(ity) => self.tcx.error,
                    None if self.map_kv(ity).is_some() => {
                        // `for entry in map` yields `Entry<K, V>` (docs/18 §6).
                        let (kt, vt) = self.map_kv(ity).unwrap();
                        let entry_ty = self.tcx.mk_named(self.prog.entry_def, vec![kt, vt]);
                        self.results.for_maps.insert(iter.span, (kt, vt, entry_ty));
                        entry_ty
                    }
                    None => match self.iterator_elem(ity) {
                        Some((elem, next, next_targs, item_ty, done_ty)) => {
                            self.results.for_iters.insert(
                                iter.span,
                                crate::sema::results::ForIter {
                                    elem, next, next_targs, iter_ty: ity, done_ty, item_ty,
                                },
                            );
                            elem
                        }
                        None => {
                            self.emit(iter.span, SemaErrorKind::Message(format!(
                                "`{}` is not iterable: it is not a `List` and has no \
                                 `next(self): Item<T> | Done` method",
                                self.display(ity)
                            )));
                            self.tcx.error
                        }
                    },
                };
                self.push_scope();
                self.bind_pattern(pattern, elem);
                self.loops.push(LoopFrame { is_loop: false, break_types: Vec::new() });
                self.check_block(body, None);
                self.loops.pop();
                self.pop_scope();
                self.tcx.null
            }
            ExprKind::Loop(body) => {
                self.loops.push(LoopFrame { is_loop: true, break_types: Vec::new() });
                self.check_block(body, None);
                let frame = self.loops.pop().unwrap();
                // The loop's value is the union of its `break` values; with no
                // value-carrying break it never completes normally (`never`).
                if frame.break_types.is_empty() {
                    self.tcx.never
                } else {
                    self.tcx.mk_union(frame.break_types)
                }
            }
            ExprKind::Break(value) => {
                let vty = match value {
                    Some(e) => self.check_expr(e, None),
                    None => self.tcx.null,
                };
                match self.loops.last_mut() {
                    None => self.emit(expr.span, SemaErrorKind::Message(
                        "`break` outside of a loop".into(),
                    )),
                    Some(frame) => {
                        if value.is_some() && !frame.is_loop {
                            self.emit(expr.span, SemaErrorKind::Message(
                                "only `loop` can `break` with a value".into(),
                            ));
                        } else {
                            frame.break_types.push(vty);
                        }
                    }
                }
                self.tcx.never
            }
            ExprKind::Continue => {
                if self.loops.is_empty() {
                    self.emit(expr.span, SemaErrorKind::Message(
                        "`continue` outside of a loop".into(),
                    ));
                }
                self.tcx.never
            }
            ExprKind::Closure { params, return_type, is_async, body } => {
                self.check_closure(
                    params, return_type.as_ref(), body, *is_async, expected, expr.span,
                )
            }
            _ => {
                self.emit(expr.span, SemaErrorKind::Message(
                    "this expression form is not yet supported by the type checker".into(),
                ));
                self.tcx.error
            }
        }
    }

    /// Record `id` as a capture for every enclosing closure that does not own
    /// it (its id predates the closure's own locals).
    fn record_capture(&mut self, id: LocalId, ty: Ty) {
        for frame in self.closure_stack.iter_mut() {
            if id.0 < frame.first_local && !frame.captures.iter().any(|(c, _)| *c == id) {
                frame.captures.push((id, ty));
            }
        }
    }

    /// Type-check a closure `(params) => body`. Parameter types come from
    /// annotations or, failing that, from the expected function type; the body
    /// is checked in a fresh scope and its free variables become captures.
    fn check_closure(
        &mut self,
        params: &[ClosureParam],
        return_type: Option<&Type>,
        body: &Expr,
        is_async: bool,
        expected: Option<Ty>,
        span: Span,
    ) -> Ty {
        let exp_params: Vec<Ty> = match expected.map(|e| self.tcx.kind(e).clone()) {
            Some(TyKind::Func { params, .. }) => params,
            _ => Vec::new(),
        };
        // A non-error expected return type guides the body; an `error`
        // placeholder (used by `List.map` before `U` is known) means "infer".
        let exp_ret = match expected.map(|e| self.tcx.kind(e).clone()) {
            Some(TyKind::Func { ret, .. }) if !self.tcx.is_error(ret) => Some(ret),
            _ => None,
        };
        let env = self.local_env();
        let first_local = self.next_local;
        self.closure_stack.push(ClosureFrame { first_local, captures: Vec::new() });
        self.push_scope();

        // Implicit `it`: a parameterless closure with a one-parameter expected
        // type binds the single argument as `it` (`docs/09` — `xs.map { it*2 }`).
        let mut synth_it: Vec<ClosureParam> = Vec::new();
        let params: &[ClosureParam] = if params.is_empty() && exp_params.len() == 1 {
            synth_it.push(ClosureParam {
                name: Ident { name: "it".into(), span },
                ty: None,
                span,
            });
            &synth_it
        } else {
            params
        };

        let mut param_locals: Vec<(LocalId, Ty)> = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let pty = match &p.ty {
                Some(t) => self.lower_ty(t, &env),
                None => exp_params.get(i).copied().unwrap_or_else(|| {
                    self.emit(p.span, SemaErrorKind::Message(format!(
                        "cannot infer the type of closure parameter `{}`; annotate it",
                        p.name.name
                    )));
                    self.tcx.error
                }),
            };
            let id = self.bind(&p.name.name, p.name.span, pty);
            param_locals.push((id, pty));
        }

        let want_ret = return_type.map(|t| self.lower_ty(t, &env)).or(exp_ret);
        // For an `async` closure the declared/expected return type is
        // `Future<Output>`; the body yields `Output` (`docs/21` §7).
        let body_expected = if is_async {
            want_ret.and_then(|r| self.future_output(r))
        } else {
            want_ret
        };
        let prev_async = self.in_async;
        self.in_async = is_async;
        let body_ty = self.check_expr(body, body_expected);
        self.in_async = prev_async;
        if let Some(r) = body_expected {
            self.expect(body_ty, r, body.span);
        }
        // The body's value type (the `Output` for an async closure).
        let body_out = body_expected.unwrap_or(body_ty);

        self.pop_scope();
        let frame = self.closure_stack.pop().expect("closure frame");
        let param_tys: Vec<Ty> = param_locals.iter().map(|(_, t)| *t).collect();
        if is_async {
            // The closure's *value* type is `(params) => Future<Output>`; the
            // recorded `AsyncInfo` drives state-machine lowering.
            let fut_ty = self.tcx.mk_named(self.prog.future_def, vec![body_out]);
            self.results.async_blocks.insert(span, crate::sema::results::AsyncInfo {
                output: body_out,
                params: param_locals,
                captures: frame.captures,
            });
            return self.tcx.mk_func(param_tys, fut_ty, false);
        }
        self.results.closures.insert(span, crate::sema::results::ClosureInfo {
            params: param_locals,
            captures: frame.captures,
            ret: body_out,
        });
        self.tcx.mk_func(param_tys, body_out, false)
    }

    /// Type-check `await e` (`docs/21` §4): `e` must be a `Future<Output>` (the
    /// interface object, an `async` function's return, or a concrete type
    /// implementing `Future`), and the result is `Output`. Only valid inside an
    /// async body.
    fn check_await(&mut self, inner: &Expr, kw_span: Span) -> Ty {
        let fty = self.check_expr(inner, None);
        if !self.in_async {
            self.emit(kw_span, SemaErrorKind::Message(
                "`await` is only allowed inside an `async` function, `async` closure, \
                 or `async { … }` block"
                    .into(),
            ));
        }
        if self.tcx.is_error(fty) {
            return self.tcx.error;
        }
        match self.future_output(fty) {
            Some(out) => {
                self.results.awaits.insert(kw_span, out);
                out
            }
            None => {
                let t = self.display(fty);
                self.emit(inner.span, SemaErrorKind::Message(format!(
                    "`await` requires a `Future`, but `{t}` is not one"
                )));
                self.tcx.error
            }
        }
    }

    /// Type-check a bare `async { … }` block (`docs/21` §6): a zero-argument
    /// inline future literal. Captures enclosing locals (like a closure) and
    /// yields `Future<Output>` where `Output` is the block's trailing type.
    fn check_async_block(&mut self, block: &Block, expected: Option<Ty>, span: Span) -> Ty {
        let out_expected = expected.and_then(|e| self.future_output(e));
        let first_local = self.next_local;
        self.closure_stack.push(ClosureFrame { first_local, captures: Vec::new() });
        let prev_async = self.in_async;
        self.in_async = true;
        let out = self.check_block(block, out_expected);
        self.in_async = prev_async;
        let frame = self.closure_stack.pop().expect("async block frame");
        self.results.async_blocks.insert(span, crate::sema::results::AsyncInfo {
            output: out,
            params: Vec::new(),
            captures: frame.captures,
        });
        self.tcx.mk_named(self.prog.future_def, vec![out])
    }

    /// If `ty` is a future, its `Output` type (`docs/21` §1). Handles the
    /// `Future<Out>` interface object / declared-async-return form directly, and
    /// a concrete type implementing `Future` by reading its `poll` return type
    /// (`Ready<Out> | Pending`).
    fn future_output(&mut self, ty: Ty) -> Option<Ty> {
        if self.tcx.is_error(ty) {
            return None;
        }
        if let TyKind::Named { def, args } = self.tcx.kind(ty).clone() {
            if def == self.prog.future_def && args.len() == 1 {
                return Some(args[0]);
            }
        }
        let (poll, ext_subst) = self.resolve_method(ty, "poll")?;
        let (env, _) = self.fn_env(poll);
        let Some(ItemKind::Function(f)) = self.prog.def(poll).item.clone() else {
            return None;
        };
        let ret = match &f.return_type {
            Some(t) => {
                let t = self.lower_ty(t, &env);
                self.subst_ty(t, &ext_subst)
            }
            None => return None,
        };
        // The poll return is `Ready<Out> | Pending`; pull `Out` from `Ready`.
        let members = match self.tcx.kind(ret).clone() {
            TyKind::Union(ms) => ms,
            _ => vec![ret],
        };
        for m in members {
            if let TyKind::Named { def, args } = self.tcx.kind(m).clone() {
                if def == self.prog.ready_def && args.len() == 1 {
                    return Some(args[0]);
                }
            }
        }
        None
    }

    /// Is `ty` a `Future<…>` interface type? Used by the "forgot to await" lint.
    fn is_future_ty(&self, ty: Ty) -> bool {
        if self.tcx.is_error(ty) {
            return false;
        }
        matches!(self.tcx.kind(ty), TyKind::Named { def, .. } if *def == self.prog.future_def)
    }

    fn check_ident(&mut self, name: &str, span: Span) -> Ty {
        if let Some((ty, id)) = self.lookup(name) {
            self.results.resolutions.insert(span, ValueRes::Local(id));
            self.record_capture(id, ty);
            // Flow narrowing: if this local is narrowed in the current branch,
            // report the narrowed type. When narrowed from a boxed type (a union
            // box or an interface object) to a single concrete variant, codegen
            // must unbox at this use — both layouts carry the payload at offset 8.
            if let Some(&narrowed) = self.narrowings.get(&id) {
                let was_boxed = matches!(self.tcx.kind(ty), TyKind::Union(_) | TyKind::Dynamic)
                    || self.is_interface(ty);
                let now_single = !matches!(self.tcx.kind(narrowed), TyKind::Union(_) | TyKind::Dynamic)
                    && !self.is_interface(narrowed);
                if was_boxed && now_single {
                    self.results.adjustments.insert(span, Adjust::Unbox(narrowed));
                }
                return narrowed;
            }
            return ty;
        }
        // A module-level value: function, var, or extern function/var.
        let module = self.current_module();
        if let Some(def) = self.prog.resolve_value_in(module, name) {
            self.results.resolutions.insert(span, self.value_res(def));
            return self.value_def_ty(def);
        }
        // A compiler builtin (temporary prelude; see `Builtin`).
        if let Some(b) = Builtin::from_name(name) {
            self.results.resolutions.insert(span, ValueRes::Builtin(b));
            return self.builtin_ty(b);
        }
        self.emit(span, SemaErrorKind::UnknownValue { name: name.to_string() });
        self.tcx.error
    }

    fn builtin_ty(&mut self, b: Builtin) -> Ty {
        match b {
            Builtin::Print | Builtin::Println => {
                let str_ty = self.tcx.str;
                let null = self.tcx.null;
                self.tcx.mk_func(vec![str_ty], null, false)
            }
            // Diverging builtins return `never` (`docs/14`, `docs/24`); a call
            // to one is well-typed wherever any value is expected.
            Builtin::Panic => {
                let str_ty = self.tcx.str;
                let never = self.tcx.never;
                self.tcx.mk_func(vec![str_ty], never, false)
            }
            // The value is widened to `dynamic` (the language never inspects it).
            Builtin::PanicWith => {
                let dynamic = self.tcx.dynamic;
                let never = self.tcx.never;
                self.tcx.mk_func(vec![dynamic], never, false)
            }
            Builtin::Exit => {
                let i32_ty = self.tcx.int(IntTy::I32);
                let never = self.tcx.never;
                self.tcx.mk_func(vec![i32_ty], never, false)
            }
            Builtin::Abort => {
                let never = self.tcx.never;
                self.tcx.mk_func(vec![], never, false)
            }
        }
    }

    /// Check `expr as T` / `expr is T`. `is` always yields `bool`; `as` yields
    /// `T` when the conversion is defined (`docs/12` §2, `docs/02` §1).
    fn check_cast(&mut self, op: CastOp, inner: &Expr, target: &Type, cast_span: Span) -> Ty {
        let env = self.local_env();
        let to = self.lower_ty(target, &env);
        self.results.cast_targets.insert(cast_span, to);
        let from = self.check_expr(inner, None);
        match op {
            CastOp::Is => self.tcx.bool,
            CastOp::As => {
                if self.cast_ok(from, to) {
                    to
                } else {
                    let f = self.display(from);
                    let t = self.display(to);
                    self.emit(inner.span, SemaErrorKind::InvalidCast { from: f, to: t });
                    self.tcx.error
                }
            }
        }
    }

    /// Is `from as to` a defined conversion?
    fn cast_ok(&self, from: Ty, to: Ty) -> bool {
        if from == to || self.tcx.is_error(from) || self.tcx.is_error(to) {
            return true;
        }
        // dynamic widening/narrowing is always permitted.
        if matches!(self.tcx.kind(to), TyKind::Dynamic)
            || matches!(self.tcx.kind(from), TyKind::Dynamic)
        {
            return true;
        }
        let from_num = self.is_numeric(from);
        let from_char = matches!(self.tcx.kind(from), TyKind::Char);
        let to_char = matches!(self.tcx.kind(to), TyKind::Char);
        // numeric ↔ numeric, int ↔ char (docs/02 §1, §4).
        if from_num && self.is_numeric(to) {
            return true;
        }
        if (self.is_integer(from) && to_char) || (from_char && self.is_integer(to)) {
            return true;
        }
        // `value as str` — the ToStr sugar for primitives (docs/15 §10).
        if to == self.tcx.str && (from_num || from_char || from == self.tcx.bool) {
            return true;
        }
        // Interface object up/down-casts: `concrete as Iface` (upcast) and
        // `iface as Concrete` (downcast, checked at runtime).
        if self.implements_dyn(from, to) || self.implements_dyn(to, from) {
            return true;
        }
        // Union narrowing: every variant of `to` is a variant of `from`.
        self.tcx.is_union_subtype(to, from)
    }

    /// Classify a module-level value definition for the resolution table.
    fn value_res(&self, def: DefId) -> ValueRes {
        match self.prog.def(def).kind {
            DefKind::Function | DefKind::ExternFunction => ValueRes::Function(def),
            DefKind::ModuleVar | DefKind::ExternVar => ValueRes::Global(def),
            DefKind::Struct => ValueRes::StructCtor(def),
            _ => ValueRes::Global(def),
        }
    }

    /// The type of a module-level value definition referenced by name.
    fn value_def_ty(&mut self, def: DefId) -> Ty {
        match self.prog.def(def).kind {
            DefKind::Function | DefKind::ExternFunction => self.function_value_ty(def),
            DefKind::ModuleVar => {
                let env = self.def_env(def, None);
                match self.prog.def(def).item.clone() {
                    Some(ItemKind::Var(v)) => match &v.ty {
                        Some(t) => self.lower_ty(t, &env),
                        // Inference of module-var types from initializer is a
                        // later refinement; require an annotation for now.
                        None => self.tcx.error,
                    },
                    _ => self.tcx.error,
                }
            }
            DefKind::ExternVar => {
                let env = self.def_env(def, None);
                match self.prog.def(def).item.clone() {
                    Some(ItemKind::Extern(ExternItem::Var { ty, .. })) => {
                        self.lower_ty(&ty, &env)
                    }
                    _ => self.tcx.error,
                }
            }
            DefKind::Struct => {
                // Unit struct used as a value: its own nominal type.
                self.tcx.mk_named(def, Vec::new())
            }
            _ => self.tcx.error,
        }
    }

    /// The function-type `Ty` of a (possibly extern) function definition.
    fn function_value_ty(&mut self, def: DefId) -> Ty {
        let env = self.def_env(def, None);
        let (params, ret, is_extern) = match self.prog.def(def).item.clone() {
            Some(ItemKind::Function(f)) => (f.params, f.return_type, false),
            Some(ItemKind::Extern(ExternItem::Function(f))) => {
                (f.params, f.return_type, true)
            }
            _ => return self.tcx.error,
        };
        let mut ptys = Vec::new();
        for p in &params {
            if let ParamKind::Normal { ty, .. } = &p.kind {
                ptys.push(self.lower_ty(ty, &env));
            }
        }
        let rty = match &ret {
            Some(t) => self.lower_ty(t, &env),
            None => self.tcx.null,
        };
        self.tcx.mk_func(ptys, rty, is_extern)
    }

    /// Recognise the empty-collection constructors `Map<K, V>()`,
    /// `Map.new<K, V>()`, `List<T>()`, `List.new<T>()`. Returns the constructed
    /// collection type and records it in `results.builtin_ctors` for codegen.
    fn try_builtin_ctor(
        &mut self,
        callee: &Expr,
        generics: &[Type],
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        // Identify the type-name the callee refers to, in either `Name<..>()`
        // or `Name.new<..>()` form.
        let type_name = match &callee.kind {
            ExprKind::Ident(name) => &name.name,
            ExprKind::Field { receiver, name } if name.name == "new" => {
                let ExprKind::Ident(recv) = &receiver.kind else { return None };
                &recv.name
            }
            _ => return None,
        };
        let module = self.current_module();
        let def = self.prog.resolve_type_in(module, type_name)?;
        let is_map = def == self.prog.map_def;
        let is_list = def == self.prog.list_def;
        if !is_map && !is_list {
            return None;
        }
        let arity = if is_map { 2 } else { 1 };
        let env = self.local_env();
        let kind = if is_map { "Map" } else { "List" };
        let tys: Vec<Ty> = if generics.len() == arity {
            generics.iter().map(|t| self.lower_ty(t, &env)).collect()
        } else {
            self.emit(span, SemaErrorKind::Message(format!(
                "`{kind}` constructor needs {arity} explicit type argument(s)"
            )));
            return Some(self.tcx.error);
        };
        if !args.is_empty() {
            self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
        }
        if is_map && !self.is_valid_map_key(tys[0]) && !self.tcx.is_error(tys[0]) {
            self.emit(span, SemaErrorKind::Message(format!(
                "`{}` cannot be used as a map key (expected `str` or an integer type)",
                self.display(tys[0])
            )));
        }
        let ty = self.tcx.mk_named(def, tys);
        self.results.builtin_ctors.insert(span, ty);
        Some(ty)
    }

    fn check_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        _generics: &[Type],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        // Builtin collection constructors: `Map<K, V>()` / `Map.new<K, V>()`
        // and `List<T>()` / `List.new<T>()`.
        if let Some(ty) = self.try_builtin_ctor(callee, _generics, args, span) {
            return ty;
        }
        // `channel<T>()` (`docs/20` §2): construct a message-passing channel.
        if let ExprKind::Ident(name) = &callee.kind {
            if name.name == "channel"
                && self.lookup("channel").is_none()
                && self.prog.resolve_value_in(self.current_module(), "channel").is_none()
            {
                return self.check_channel_new(_generics, args, span);
            }
        }
        // `block_on(fut)` (`docs/21` §6): drive a future to completion on the
        // current thread, returning its `Output`. A free-function builtin (no
        // real binding), recognised before the general call path.
        if let ExprKind::Ident(name) = &callee.kind {
            if name.name == "block_on"
                && self.lookup("block_on").is_none()
                && self.prog.resolve_value_in(self.current_module(), "block_on").is_none()
            {
                return self.check_block_on(args, span);
            }
            // `yield_now()` (`docs/21`): a `Future<null>` that suspends once.
            if name.name == "yield_now"
                && self.lookup("yield_now").is_none()
                && self.prog.resolve_value_in(self.current_module(), "yield_now").is_none()
            {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.results.yield_nows.insert(span);
                return self.tcx.mk_named(self.prog.future_def, vec![self.tcx.null]);
            }
            // `spawn(fut)` (`docs/21` §6): run a future on a worker, yielding a
            // `JoinHandle<T>`.
            if name.name == "spawn"
                && self.lookup("spawn").is_none()
                && self.prog.resolve_value_in(self.current_module(), "spawn").is_none()
            {
                return self.check_async_spawn(args, span);
            }
            // `sleep(ms)` (`docs/21` §9): a `Future<null>` completing after a delay.
            if name.name == "sleep"
                && self.lookup("sleep").is_none()
                && self.prog.resolve_value_in(self.current_module(), "sleep").is_none()
            {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                } else {
                    let i64t = self.tcx.int(IntTy::I64);
                    let a = self.check_expr(&args[0], Some(i64t));
                    self.expect(a, i64t, args[0].span);
                }
                self.results.async_sleeps.insert(span);
                return self.tcx.mk_named(self.prog.future_def, vec![self.tcx.null]);
            }
        }
        // `Shared.new(value)` (`docs/20` §4): construct a mutex-protected cell.
        // Recognised before the static-method path (which would not find `new`).
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(recv) = &receiver.kind {
                if recv.name == "Shared"
                    && name.name == "new"
                    && self.lookup(&recv.name).is_none()
                {
                    return self.check_shared_new(args, span);
                }
            }
        }
        // Numeric-namespace methods: `i32.wrapping_add(a,b)`, `f64.is_nan(x)`, …
        // (`docs/18` §10, `docs/14` §5).
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(recv) = &receiver.kind {
                if self.lookup(&recv.name).is_none() {
                    if let Some(t) = self.check_num_method(&recv.name, name, args, span) {
                        return t;
                    }
                }
            }
        }
        // `M.foo(args)` where `M` is an `import … as M` namespace alias (and not
        // shadowed by a local) — resolve `foo` in the aliased module.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(m) = &receiver.kind {
                if self.lookup(&m.name).is_none() {
                    if let Some(target) =
                        self.prog.namespace_target(self.current_module(), &m.name)
                    {
                        return self.check_namespaced_call(
                            target, &m.name, callee, name, args, _generics, trailing, span,
                        );
                    }
                }
            }
        }
        // `Thread.spawn { … }` (`docs/20` §1): a builtin that runs a closure on
        // a new OS thread. `Thread` is not a real binding, so this is recognised
        // before the method-call path.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(m) = &receiver.kind {
                if m.name == "Thread"
                    && name.name == "spawn"
                    && self.lookup(&m.name).is_none()
                    && self.prog.resolve_value_in(self.current_module(), &m.name).is_none()
                {
                    return self.check_thread_spawn(args, trailing, span);
                }
            }
        }
        // `Type.method(args)` / `T.method(args)` — a static method call
        // (`docs/09` §6, `docs/10`): the receiver names a type or an in-scope
        // generic parameter, not a value. Checked before the instance-method
        // path so it is not mistaken for a method on a value.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(recv_id) = &receiver.kind {
                if self.lookup(&recv_id.name).is_none()
                    && self.prog.namespace_target(self.current_module(), &recv_id.name).is_none()
                {
                    if let Some(ty) =
                        self.try_static_call(&recv_id.name, callee, name, args, _generics, trailing, span)
                    {
                        return ty;
                    }
                }
            }
        }
        // `recv.method(args)` — a method call (callee is a field access). A
        // trailing closure (`xs.map { … }`) is the final argument.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let Some(tc) = trailing {
                let mut all = args.to_vec();
                all.push(tc.clone());
                return self.check_method_call(callee, receiver, name, &all, span);
            }
            return self.check_method_call(callee, receiver, name, args, span);
        }
        // `Pair(a, b)` on a tuple struct is direct construction, not a call
        // (docs/09 §10 — tuple structs are not rewritten to `.new`).
        if let ExprKind::Ident(name) = &callee.kind {
            if self.lookup(&name.name).is_none() {
                let module = self.current_module();
                if let Some(def) = self.prog.resolve_type_in(module, &name.name) {
                    if matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
                        return self.check_tuple_ctor(def, callee, args, span);
                    }
                }
                // A generic free function: infer/substitute its type arguments.
                if let Some(def) = self.prog.resolve_value_in(module, &name.name) {
                    if matches!(self.prog.def(def).kind, DefKind::Function | DefKind::ExternFunction)
                        && !self.prog.def(def).generics.is_empty()
                    {
                        return self.check_generic_call(def, callee, args, _generics, span);
                    }
                }
            }
        }
        let callee_ty = self.check_expr(callee, None);
        if self.tcx.is_error(callee_ty) {
            return self.tcx.error;
        }
        self.check_args_against(callee_ty, args, trailing, span)
    }

    /// Type-check `args` (and an optional trailing closure) against a callable
    /// `callee_ty` (a `Func`), returning its result type.
    fn check_args_against(
        &mut self,
        callee_ty: Ty,
        args: &[Expr],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let TyKind::Func { params, ret, .. } = self.tcx.kind(callee_ty).clone() else {
            let found = self.display(callee_ty);
            self.emit(span, SemaErrorKind::NotCallable { found });
            return self.tcx.error;
        };
        let total_args = args.len() + usize::from(trailing.is_some());
        if total_args != params.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: params.len(),
                found: total_args,
            });
        }
        for (i, arg) in args.iter().enumerate() {
            let exp = params.get(i).copied();
            let aty = self.check_expr(arg, exp);
            if let Some(p) = exp {
                self.expect(aty, p, arg.span);
            }
        }
        if let Some(tc) = trailing {
            let exp = params.get(args.len()).copied();
            let tty = self.check_expr(tc, exp);
            if let Some(p) = exp {
                self.expect(tty, p, tc.span);
            }
        }
        ret
    }

    /// A namespaced call `M.foo(args)` where `M` is an `import … as M` alias:
    /// resolve `foo` as a public function in the target module and check it.
    fn check_namespaced_call(
        &mut self,
        target: ModId,
        alias: &str,
        callee: &Expr,
        name: &Ident,
        args: &[Expr],
        generics: &[Type],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let Some(def) = self.prog.resolve_pub_value_in(target, &name.name) else {
            self.emit(name.span, SemaErrorKind::Message(format!(
                "no public value `{}` in module `{alias}`", name.name
            )));
            return self.tcx.error;
        };
        // A generic free function: infer/substitute its type arguments.
        if matches!(self.prog.def(def).kind, DefKind::Function | DefKind::ExternFunction)
            && !self.prog.def(def).generics.is_empty()
        {
            return self.check_generic_call(def, callee, args, generics, span);
        }
        self.results.resolutions.insert(callee.span, self.value_res(def));
        let callee_ty = self.value_def_ty(def);
        self.check_args_against(callee_ty, args, trailing, span)
    }

    // -- structs -------------------------------------------------------------

    /// Build a type-lowering env binding a struct/alias def's generic params to
    /// the supplied arguments.
    fn subst_env(&mut self, def: DefId, args: &[Ty]) -> TypeEnv {
        let module = self.prog.def(def).module;
        let mut env = TypeEnv::new(module);
        let gens = self.prog.def(def).generics.clone();
        for (i, g) in gens.iter().enumerate() {
            let name = self.prog.def(*g).name.clone();
            let aty = args.get(i).copied().unwrap_or(self.tcx.error);
            env.generics.insert(name, aty);
        }
        env
    }

    /// The record fields of a struct def with generic args substituted.
    fn record_fields(&mut self, def: DefId, args: &[Ty]) -> Option<Vec<(String, Ty)>> {
        let item = self.prog.def(def).item.clone();
        let StructKind::Record(fields) = (match item {
            Some(ItemKind::Struct(s)) => s.kind,
            _ => return None,
        }) else {
            return None;
        };
        let env = self.subst_env(def, args);
        Some(
            fields
                .iter()
                .map(|f| (f.name.name.clone(), self.lower_ty(&f.ty, &env)))
                .collect(),
        )
    }

    /// The positional field types of a tuple struct with args substituted.
    fn tuple_fields(&mut self, def: DefId, args: &[Ty]) -> Option<Vec<Ty>> {
        let item = self.prog.def(def).item.clone();
        let StructKind::Tuple(fields) = (match item {
            Some(ItemKind::Struct(s)) => s.kind,
            _ => return None,
        }) else {
            return None;
        };
        let env = self.subst_env(def, args);
        Some(fields.iter().map(|f| self.lower_ty(&f.ty, &env)).collect())
    }

    /// Infer a record struct's generic arguments from its field values. The
    /// expected type (if it names the same struct) seeds the solution; each
    /// field's value type is unified against the field's declared type written
    /// in terms of the struct's generic parameters.
    fn infer_struct_args(
        &mut self,
        def: DefId,
        gens: &[DefId],
        fields: &[FieldInit],
        expected: Option<Ty>,
        path: &TypePath,
        span: Span,
    ) -> Vec<Ty> {
        let mut map: HashMap<DefId, Ty> = HashMap::new();
        if let Some(exp) = expected {
            if let TyKind::Named { def: edef, args: eargs } = self.tcx.kind(exp).clone() {
                if edef == def && eargs.len() == gens.len() {
                    for (g, a) in gens.iter().zip(&eargs) {
                        map.insert(*g, *a);
                    }
                }
            }
        }
        // Field types expressed with each generic param as `Param(g)`.
        let param_args: Vec<Ty> = gens.iter().map(|g| self.tcx.mk_param(*g)).collect();
        let declared = self.record_fields(def, &param_args).unwrap_or_default();
        for fi in fields {
            if let Some((_, pfty)) = declared.iter().find(|(n, _)| *n == fi.name.name) {
                let pfty = *pfty;
                let vt = match &fi.value {
                    Some(v) => self.check_expr(v, None),
                    None => self.check_ident(&fi.name.name, fi.name.span),
                };
                self.unify(pfty, vt, &mut map);
            }
        }
        let mut args = Vec::with_capacity(gens.len());
        for g in gens {
            match map.get(g).copied() {
                Some(t) => args.push(t),
                None => {
                    let gname = self.prog.def(*g).name.clone();
                    self.emit(span, SemaErrorKind::Message(format!(
                        "cannot infer generic argument `{}` for `{}`; annotate it",
                        gname, path.name.name
                    )));
                    args.push(self.tcx.error);
                }
            }
        }
        args
    }

    fn check_struct_lit(
        &mut self,
        path: &TypePath,
        fields: &[FieldInit],
        spread: Option<&Expr>,
        expected: Option<Ty>,
        span: Span,
    ) -> Ty {
        let module = self.current_module();
        let Some(def) = self.prog.resolve_type_in(module, &path.name.name) else {
            self.emit(path.span, SemaErrorKind::UnknownType { name: path.name.name.clone() });
            return self.tcx.error;
        };
        if !matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
            self.emit(path.span, SemaErrorKind::Message(format!(
                "`{}` is not a struct and cannot be constructed with `{{ }}`",
                path.name.name
            )));
            return self.tcx.error;
        }
        let env = self.local_env();
        let explicit: Vec<Ty> = path.generics.iter().map(|g| self.lower_ty(g, &env)).collect();
        let gens = self.prog.def(def).generics.clone();
        // When the generic arguments are not written out, infer them from the
        // field values (seeded by the expected type), mirroring generic calls.
        let inferring = !gens.is_empty() && explicit.len() != gens.len();
        let args: Vec<Ty> = if inferring {
            self.infer_struct_args(def, &gens, fields, expected, path, span)
        } else {
            explicit
        };
        let Some(declared) = self.record_fields(def, &args) else {
            self.emit(span, SemaErrorKind::Message(format!(
                "`{}` is not a record struct", path.name.name
            )));
            return self.tcx.error;
        };

        let mut seen = std::collections::HashSet::new();
        for fi in fields {
            match declared.iter().find(|(n, _)| *n == fi.name.name) {
                Some((_, fty)) => {
                    let fty = *fty;
                    seen.insert(fi.name.name.clone());
                    match &fi.value {
                        Some(v) => {
                            // Inference already checked the value (with no
                            // expectation); reuse that type so we don't
                            // double-check. Otherwise check against the field.
                            let vt = match self.results.expr_types.get(&v.span).copied() {
                                Some(t) if inferring => t,
                                _ => self.check_expr(v, Some(fty)),
                            };
                            self.expect(vt, fty, v.span);
                        }
                        None => {
                            // Field-init shorthand: a local of the same name.
                            let lt = self.check_ident(&fi.name.name, fi.name.span);
                            self.expect(lt, fty, fi.name.span);
                        }
                    }
                }
                None => self.emit(fi.span, SemaErrorKind::Message(format!(
                    "struct `{}` has no field `{}`", path.name.name, fi.name.name
                ))),
            }
        }

        let result = self.tcx.mk_named(def, args);
        match spread {
            Some(base) => {
                let bt = self.check_expr(base, Some(result));
                self.expect(bt, result, base.span);
            }
            None => {
                for (n, _) in &declared {
                    if !seen.contains(n) {
                        self.emit(span, SemaErrorKind::Message(format!(
                            "missing field `{}` in `{}`", n, path.name.name
                        )));
                    }
                }
            }
        }
        result
    }

    fn check_tuple_ctor(&mut self, def: DefId, callee: &Expr, args: &[Expr], span: Span) -> Ty {
        self.results.resolutions.insert(callee.span, ValueRes::StructCtor(def));
        let targs: Vec<Ty> = Vec::new(); // generic tuple-struct inference deferred
        let Some(field_tys) = self.tuple_fields(def, &targs) else {
            self.emit(span, SemaErrorKind::Message(
                "only tuple structs are constructed by call".into(),
            ));
            // Still check the args so errors inside them surface.
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        };
        if args.len() != field_tys.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: field_tys.len(),
                found: args.len(),
            });
        }
        for (a, fty) in args.iter().zip(&field_tys) {
            let at = self.check_expr(a, Some(*fty));
            self.expect(at, *fty, a.span);
        }
        self.tcx.mk_named(def, targs)
    }

    fn check_field(&mut self, receiver: &Expr, name: &Ident, field_span: Span) -> Ty {
        // Numeric-namespace constants: `i32.MAX`, `f64.NAN`, … (`docs/18` §10).
        if let ExprKind::Ident(id) = &receiver.kind {
            if self.lookup(&id.name).is_none() {
                if let Some(t) = self.check_num_constant(&id.name, name, field_span) {
                    return t;
                }
            }
        }
        let rty = self.check_expr(receiver, None);
        if self.tcx.is_error(rty) {
            return self.tcx.error;
        }
        if let TyKind::Named { def, args } = self.tcx.kind(rty).clone() {
            if let Some(fields) = self.record_fields(def, &args) {
                if let Some((_, fty)) = fields.iter().find(|(n, _)| n == &name.name) {
                    return *fty;
                }
            }
        }
        self.emit(name.span, SemaErrorKind::Message(format!(
            "no field `{}` on type `{}`", name.name, self.display(rty)
        )));
        self.tcx.error
    }

    fn check_tuple_index(&mut self, receiver: &Expr, index: u32, span: Span) -> Ty {
        let rty = self.check_expr(receiver, None);
        if self.tcx.is_error(rty) {
            return self.tcx.error;
        }
        let i = index as usize;
        match self.tcx.kind(rty).clone() {
            TyKind::Tuple(elems) => elems.get(i).copied().unwrap_or_else(|| {
                self.emit(span, SemaErrorKind::Message(format!(
                    "tuple index {index} out of range for `{}`", self.display(rty)
                )));
                self.tcx.error
            }),
            TyKind::Named { def, args } => {
                if let Some(fields) = self.tuple_fields(def, &args) {
                    if let Some(fty) = fields.get(i) {
                        return *fty;
                    }
                }
                self.emit(span, SemaErrorKind::Message(format!(
                    "tuple index {index} out of range for `{}`", self.display(rty)
                )));
                self.tcx.error
            }
            _ => {
                self.emit(span, SemaErrorKind::Message(format!(
                    "type `{}` cannot be indexed by position", self.display(rty)
                )));
                self.tcx.error
            }
        }
    }

    // -- `?` propagation -----------------------------------------------------

    /// `expr?` (`docs/13` §2): partition `expr`'s type against the enclosing
    /// return type `R`. Variants of `expr` that are also variants of `R` are
    /// *failures* (early-returned); the rest are *successes* and become the
    /// expression's value.
    fn check_try(&mut self, inner: &Expr, q_span: Span) -> Ty {
        let et = self.check_expr(inner, None);
        if self.tcx.is_error(et) {
            return self.tcx.error;
        }
        let r = self.ret_ty;
        let et_vars = self.tcx.variants(et);
        let r_vars = self.tcx.variants(r);
        // Direct failures: variants of the operand that are also variants of R.
        let failures: Vec<Ty> =
            et_vars.iter().copied().filter(|v| r_vars.contains(v)).collect();
        // The rest are candidate successes — unless a variant can propagate via
        // a `FromResidual` conversion (`docs/13` §4), in which case it is also a
        // failure (converted at the implicit return).
        let mut successes = Vec::new();
        let mut conversions: Vec<(Ty, DefId, Ty)> = Vec::new();
        for v in et_vars.iter().copied().filter(|v| !failures.contains(v)) {
            if let Some((method, target)) = self.find_residual_conversion(v, &r_vars) {
                conversions.push((v, method, target));
            } else {
                successes.push(v);
            }
        }
        if !conversions.is_empty() {
            self.results.residual_conversions.insert(q_span, conversions);
        }

        if failures.is_empty() && self.results.residual_conversions.get(&q_span).is_none() {
            self.emit(q_span, SemaErrorKind::Message(
                "nothing to propagate here; remove the `?`".into(),
            ));
            return et;
        }
        if successes.is_empty() {
            self.emit(q_span, SemaErrorKind::Message(
                "this expression always returns; use a `match` instead".into(),
            ));
            return self.tcx.error;
        }
        self.tcx.mk_union(successes)
    }

    /// Find a `FromResidual` conversion for a `?` residual variant `e` whose
    /// converted target type is one of the enclosing return type's variants
    /// `r_vars`. Returns `(from_residual method def, target type)` (`docs/13`
    /// §4): an `extend Target: FromResidual<E>` where `Target ∈ r_vars`.
    fn find_residual_conversion(&mut self, e: Ty, r_vars: &[Ty]) -> Option<(DefId, Ty)> {
        let fr_def = self.prog.from_residual_def;
        for id in 0..self.prog.defs.len() {
            let ext = DefId(id as u32);
            if self.prog.def(ext).kind != DefKind::Extend {
                continue;
            }
            let Some(ItemKind::Extend(e_item)) = self.prog.def(ext).item.clone() else { continue };
            // Only non-generic FromResidual impls are matched for now.
            if !self.prog.def(ext).generics.is_empty() {
                continue;
            }
            let env = TypeEnv::new(self.prog.def(ext).module);
            let target = self.lower_ty(&e_item.target, &env);
            if !r_vars.contains(&target) {
                continue;
            }
            for itf in &e_item.interfaces {
                let TypeKind::Named { name, generics } = &itf.kind else { continue };
                if generics.len() != 1 {
                    continue;
                }
                let module = self.prog.def(ext).module;
                if self.prog.resolve_type_in(module, &name.name) != Some(fr_def) {
                    continue;
                }
                let residual = self.lower_ty(&generics[0], &env);
                if residual != e {
                    continue;
                }
                // Find this extend's `from_residual` method.
                if let Some(m) = self.extend_method(ext, "from_residual") {
                    return Some((m, target));
                }
            }
        }
        None
    }

    /// The `ExtendMethod` def named `name` inside extend block `ext`, if any.
    fn extend_method(&self, ext: DefId, name: &str) -> Option<DefId> {
        for id in 0..self.prog.defs.len() {
            let d = DefId(id as u32);
            let def = self.prog.def(d);
            if def.kind == DefKind::ExtendMethod
                && def.parent == Some(ext)
                && def.name == name
            {
                return Some(d);
            }
        }
        None
    }

    // -- match ---------------------------------------------------------------

    fn check_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        expected: Option<Ty>,
    ) -> Ty {
        let sty = self.check_expr(scrutinee, None);
        let mut body_tys = Vec::new();
        for arm in arms {
            self.push_scope();
            self.check_pattern(&arm.pattern, sty);
            if let Some(g) = &arm.guard {
                let gt = self.check_expr(g, Some(self.tcx.bool));
                if !self.tcx.is_error(gt) && gt != self.tcx.bool {
                    let found = self.display(gt);
                    self.emit(g.span, SemaErrorKind::NonBoolCondition { found });
                }
            }
            let bt = self.check_expr(&arm.body, expected);
            // Coerce arm bodies to the match's expected type if one is given.
            if let Some(exp) = expected {
                self.expect(bt, exp, arm.body.span);
                body_tys.push(if self.assignable(bt, exp) { exp } else { bt });
            } else {
                body_tys.push(bt);
            }
            self.pop_scope();
        }
        self.check_exhaustive(sty, arms, span);
        if body_tys.is_empty() {
            self.tcx.null
        } else {
            self.tcx.mk_union(body_tys)
        }
    }

    /// Check a pattern against the scrutinee type, binding any names. Records
    /// the tested variant type for type-matching patterns (for codegen).
    fn check_pattern(&mut self, pattern: &Pattern, sty: Ty) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                self.bind(&name.name, name.span, sty);
            }
            PatternKind::Literal(e) => {
                let lt = self.check_expr(e, Some(sty));
                // The literal must be a possible value of the scrutinee.
                if !self.assignable(lt, sty) && !self.tcx.is_error(lt) {
                    // For union scrutinees a literal of a variant type is fine.
                    if !self.tcx.variants(sty).contains(&lt) {
                        let e2 = self.display(sty);
                        let f = self.display(lt);
                        self.emit(e.span, SemaErrorKind::TypeMismatch { expected: e2, found: f });
                    }
                }
            }
            PatternKind::TypeBinding { ty, binding } => {
                let env = self.local_env();
                let t = self.lower_ty(ty, &env);
                self.results.pattern_types.insert(pattern.span, t);
                if let Some(b) = binding {
                    self.bind(&b.name, b.span, t);
                }
            }
            PatternKind::UnitPath(path) => {
                let module = self.current_module();
                if let Some(def) = self.prog.resolve_type_in(module, &path.name.name) {
                    let t = self.tcx.mk_named(def, Vec::new());
                    self.results.pattern_types.insert(pattern.span, t);
                } else {
                    self.emit(path.span, SemaErrorKind::UnknownType {
                        name: path.name.name.clone(),
                    });
                }
            }
            PatternKind::Tuple { elems, rest: None } => {
                if let TyKind::Tuple(ets) = self.tcx.kind(sty).clone() {
                    if ets.len() == elems.len() {
                        for (p, et) in elems.iter().zip(ets) {
                            self.check_pattern(p, et);
                        }
                        return;
                    }
                }
                self.emit(pattern.span, SemaErrorKind::Message(
                    "tuple pattern does not match the scrutinee shape".into(),
                ));
            }
            _ => self.emit(pattern.span, SemaErrorKind::Message(
                "this pattern is not yet supported".into(),
            )),
        }
    }

    /// Basic exhaustiveness (`docs/07` §3): a wildcard/binding arm covers
    /// everything; otherwise a union scrutinee must have every variant matched
    /// by a type/unit pattern.
    fn check_exhaustive(&mut self, sty: Ty, arms: &[MatchArm], span: Span) {
        let has_catch_all = arms
            .iter()
            .any(|a| a.guard.is_none() && self.is_irrefutable(&a.pattern, sty));
        if has_catch_all {
            return;
        }
        if let TyKind::Union(variants) = self.tcx.kind(sty).clone() {
            let mut covered: Vec<Ty> = Vec::new();
            for a in arms.iter().filter(|a| a.guard.is_none()) {
                if let Some(t) = self.results.pattern_types.get(&a.pattern.span) {
                    covered.push(*t);
                }
                // A `null` literal pattern covers the `null` variant.
                if let PatternKind::Literal(e) = &a.pattern.kind {
                    if matches!(e.kind, ExprKind::Null) {
                        covered.push(self.tcx.null);
                    }
                }
            }
            let missing: Vec<Ty> =
                variants.iter().copied().filter(|v| !covered.contains(v)).collect();
            if !missing.is_empty() {
                let names: Vec<String> = missing.iter().map(|v| self.display(*v)).collect();
                self.emit(span, SemaErrorKind::Message(format!(
                    "non-exhaustive match: missing {}", names.join(", ")
                )));
            }
        } else {
            self.emit(span, SemaErrorKind::Message(
                "non-exhaustive match: add a `_` arm".into(),
            ));
        }
    }

    /// Does this pattern match every value of `sty` (covering the scrutinee)?
    fn is_irrefutable(&self, pattern: &Pattern, sty: Ty) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => true,
            PatternKind::Tuple { elems, rest: None } => {
                match self.tcx.kind(sty) {
                    TyKind::Tuple(ets) if ets.len() == elems.len() => {
                        let ets = ets.clone();
                        elems.iter().zip(ets).all(|(p, et)| self.is_irrefutable(p, et))
                    }
                    _ => false,
                }
            }
            PatternKind::TypeBinding { .. } => {
                // `T x` covers the scrutinee only when `T` is exactly its type
                // (i.e. a non-union match on its own type).
                !matches!(self.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic)
                    && self.results.pattern_types.get(&pattern.span) == Some(&sty)
            }
            _ => false,
        }
    }

    // -- builtin List<T> -----------------------------------------------------

    /// If `ty` is `List<E>`, return `E`.
    fn list_elem(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.list_def && args.len() == 1 => {
                Some(args[0])
            }
            _ => None,
        }
    }

    fn mk_list(&mut self, elem: Ty) -> Ty {
        let def = self.prog.list_def;
        self.tcx.mk_named(def, vec![elem])
    }

    fn check_list_lit(&mut self, elems: &[Expr], expected: Option<Ty>, span: Span) -> Ty {
        let exp_elem = expected.and_then(|e| self.list_elem(e));
        if elems.is_empty() {
            return match exp_elem {
                Some(e) => self.mk_list(e),
                None => {
                    self.emit(span, SemaErrorKind::Message(
                        "cannot infer the element type of an empty list; annotate it".into(),
                    ));
                    self.tcx.error
                }
            };
        }
        // The element type is the annotation if given, else the first element's.
        let elem = match exp_elem {
            Some(e) => e,
            None => self.check_expr(&elems[0], None),
        };
        for el in elems {
            let t = self.check_expr(el, Some(elem));
            self.expect(t, elem, el.span);
        }
        self.mk_list(elem)
    }

    fn map_kv(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.map_def && args.len() == 2 => {
                Some((args[0], args[1]))
            }
            _ => None,
        }
    }

    fn mk_map(&mut self, k: Ty, v: Ty) -> Ty {
        let def = self.prog.map_def;
        self.tcx.mk_named(def, vec![k, v])
    }

    /// A map key must be hashable/comparable. For now that means `str` or any
    /// integer type (matching the runtime's two hashing strategies).
    fn is_valid_map_key(&self, ty: Ty) -> bool {
        ty == self.tcx.str || matches!(self.tcx.kind(ty), TyKind::Int(_))
    }

    fn check_map_lit(&mut self, items: &[MapItem], expected: Option<Ty>, span: Span) -> Ty {
        let exp_kv = expected.and_then(|e| self.map_kv(e));
        // Determine K/V from the annotation, else from the first entry.
        let mut kv = exp_kv;
        if kv.is_none() {
            for it in items {
                if let MapItem::Entry { key, value, .. } = it {
                    let k = self.check_expr(key, None);
                    let v = self.check_expr(value, None);
                    kv = Some((k, v));
                    break;
                }
            }
        }
        let Some((kt, vt)) = kv else {
            self.emit(span, SemaErrorKind::Message(
                "cannot infer the key/value types of an empty map; annotate it or use `Map<K, V>()`".into(),
            ));
            return self.tcx.error;
        };
        if !self.is_valid_map_key(kt) && !self.tcx.is_error(kt) {
            self.emit(span, SemaErrorKind::Message(format!(
                "`{}` cannot be used as a map key (expected `str` or an integer type)",
                self.display(kt)
            )));
        }
        let map_ty = self.mk_map(kt, vt);
        for it in items {
            match it {
                MapItem::Entry { key, value, .. } => {
                    let k = self.check_expr(key, Some(kt));
                    self.expect(k, kt, key.span);
                    let v = self.check_expr(value, Some(vt));
                    self.expect(v, vt, value.span);
                }
                MapItem::Spread(base) => {
                    let bt = self.check_expr(base, Some(map_ty));
                    self.expect(bt, map_ty, base.span);
                }
            }
        }
        map_ty
    }

    /// Type-check a builtin `Map<K, V>` method call (`docs/18` §6).
    fn check_map_method(&mut self, kt: Ty, vt: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let i64t = self.tcx.int(IntTy::I64);
        let check_args = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "size" => { check_args(self, &[]); i64t }
            "is_empty" => { check_args(self, &[]); self.tcx.bool }
            "clear" => { check_args(self, &[]); self.tcx.null }
            "contains" => { check_args(self, &[kt]); self.tcx.bool }
            "get" => { check_args(self, &[kt]); self.tcx.mk_union([vt, self.tcx.null]) }
            "remove" => { check_args(self, &[kt]); self.tcx.mk_union([vt, self.tcx.null]) }
            "set" => { check_args(self, &[kt, vt]); self.tcx.null }
            "keys" => { check_args(self, &[]); self.mk_list(kt) }
            "values" => { check_args(self, &[]); self.mk_list(vt) }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`Map` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    fn check_index(&mut self, receiver: &Expr, index: &Expr) -> Ty {
        let rty = self.check_expr(receiver, None);
        if self.tcx.is_error(rty) {
            return self.tcx.error;
        }
        if let Some(elem) = self.list_elem(rty) {
            let i64t = self.tcx.int(IntTy::I64);
            let it = self.check_expr(index, Some(i64t));
            self.expect(it, i64t, index.span);
            return elem;
        }
        // `map[key]` — indexed read/write; panics on a missing key (`docs/18`).
        if let Some((kt, vt)) = self.map_kv(rty) {
            let it = self.check_expr(index, Some(kt));
            self.expect(it, kt, index.span);
            return vt;
        }
        self.emit(receiver.span, SemaErrorKind::Message(format!(
            "type `{}` cannot be indexed with `[]`", self.display(rty)
        )));
        self.tcx.error
    }

    /// Type-check a builtin `List<E>` method call.
    fn check_list_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let i64t = self.tcx.int(IntTy::I64);
        let check_args = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "push" => {
                check_args(self, &[elem]);
                self.tcx.null
            }
            "size" => {
                check_args(self, &[]);
                i64t
            }
            "is_empty" => {
                check_args(self, &[]);
                self.tcx.bool
            }
            "get" => {
                check_args(self, &[i64t]);
                self.tcx.mk_union([elem, self.tcx.null])
            }
            "set" => {
                check_args(self, &[i64t, elem]);
                self.tcx.null
            }
            // Higher-order methods take a closure (often written as a trailing
            // closure with an implicit `it`).
            "map" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                // Expected `(E) => U`; `U` is inferred from the closure body.
                let want = self.tcx.mk_func(vec![elem], self.tcx.error, false);
                let ct = self.check_expr(&args[0], Some(want));
                match self.tcx.kind(ct).clone() {
                    TyKind::Func { ret, .. } => self.mk_list(ret),
                    _ => self.tcx.error,
                }
            }
            "filter" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                let want = self.tcx.mk_func(vec![elem], self.tcx.bool, false);
                let ct = self.check_expr(&args[0], Some(want));
                self.expect(ct, want, args[0].span);
                self.mk_list(elem)
            }
            "each" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.null;
                }
                let want = self.tcx.mk_func(vec![elem], self.tcx.null, false);
                self.check_expr(&args[0], Some(want));
                self.tcx.null
            }
            "fold" => {
                if args.len() != 2 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 2, found: args.len() });
                    return self.tcx.error;
                }
                let acc = self.check_expr(&args[0], None);
                let want = self.tcx.mk_func(vec![acc, elem], acc, false);
                let ct = self.check_expr(&args[1], Some(want));
                self.expect(ct, want, args[1].span);
                acc
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`List` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Type-check a builtin `str` method call (`docs/18` §4).
    fn check_str_method(&mut self, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let str_ty = self.tcx.str;
        let i64t = self.tcx.int(IntTy::I64);
        let check = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "size" | "byte_size" => {
                check(self, &[]);
                i64t
            }
            "is_empty" => {
                check(self, &[]);
                self.tcx.bool
            }
            "contains" | "starts_with" | "ends_with" => {
                check(self, &[str_ty]);
                self.tcx.bool
            }
            "substring" => {
                check(self, &[i64t, i64t]);
                str_ty
            }
            "to_upper" | "to_lower" | "trim" => {
                check(self, &[]);
                str_ty
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`str` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Whether `ty` is an immutable value: cloning it can share the existing
    /// value (no observable mutation). Primitives, `char`, `bool`, `str`, and
    /// `null` qualify (`docs/15` §8 — `str` is immutable, so sharing is sound).
    fn is_immutable_value(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        )
    }

    /// Whether `ty` is safe to capture into a spawned thread by value: an
    /// immutable value, or a thread-safe channel endpoint (`Sender`/`Receiver`,
    /// whose struct just carries a synchronized channel's id) (`docs/20`).
    fn is_thread_shareable(&self, ty: Ty) -> bool {
        if self.is_immutable_value(ty) {
            return true;
        }
        matches!(self.tcx.kind(ty),
            TyKind::Named { def, .. }
                if *def == self.prog.sender_def
                    || *def == self.prog.receiver_def
                    || *def == self.prog.shared_def)
    }

    /// Resolve a builtin `.clone()`. Returns `Some(result type)` for the
    /// receiver kinds the compiler clones intrinsically (recording a
    /// [`CloneKind`] for codegen); `None` for user types, which clone through
    /// their own `Clone` impl. Emits an error for collections whose elements are
    /// not (yet) cloneable.
    fn check_builtin_clone(&mut self, rty: Ty, callee_span: Span, name_span: Span) -> Option<Ty> {
        use crate::sema::results::CloneKind;
        if self.is_immutable_value(rty) {
            self.results.clone_kinds.insert(callee_span, CloneKind::Identity);
            return Some(rty);
        }
        // A `Shared<T>` handle clones to another handle for the *same* cell
        // (`docs/20` §4: clone the handle, not the value). The handle is an
        // immutable id, so sharing it is the intended clone.
        if matches!(self.tcx.kind(rty),
            TyKind::Named { def, .. }
                if *def == self.prog.shared_def
                    || *def == self.prog.sender_def
                    || *def == self.prog.receiver_def)
        {
            self.results.clone_kinds.insert(callee_span, CloneKind::Identity);
            return Some(rty);
        }
        if let Some(elem) = self.list_elem(rty) {
            if self.is_immutable_value(elem) {
                self.results.clone_kinds.insert(callee_span, CloneKind::List);
                return Some(rty);
            }
            self.emit(name_span, SemaErrorKind::Message(format!(
                "cannot `clone` a `List` of `{}` (only immutable element types are \
                 cloneable so far; clone the elements explicitly)",
                self.display(elem)
            )));
            return Some(self.tcx.error);
        }
        if let Some((kt, vt)) = self.map_kv(rty) {
            if self.is_immutable_value(kt) && self.is_immutable_value(vt) {
                self.results.clone_kinds.insert(callee_span, CloneKind::Map);
                return Some(rty);
            }
            self.emit(name_span, SemaErrorKind::Message(
                "cannot `clone` a `Map` with mutable key/value types yet".into(),
            ));
            return Some(self.tcx.error);
        }
        None
    }

    /// Type-check `Thread.spawn(() => R)` / `Thread.spawn { … }` (`docs/20` §1).
    /// The single argument is a parameterless closure; the result is
    /// `JoinHandle<R>`. Captures must be immutable values (deep-cloning mutable
    /// captures across the spawn boundary is a follow-up — `docs/20` §1).
    fn check_thread_spawn(&mut self, args: &[Expr], trailing: Option<&Expr>, span: Span) -> Ty {
        let clo = match (args, trailing) {
            ([], Some(tc)) => tc,
            ([a], None) => a,
            _ => {
                self.emit(span, SemaErrorKind::Message(
                    "`Thread.spawn` takes a single closure argument".into(),
                ));
                for a in args {
                    self.check_expr(a, None);
                }
                return self.tcx.error;
            }
        };
        // Expect a parameterless closure; its return type is inferred.
        let want = self.tcx.mk_func(vec![], self.tcx.error, false);
        let cty = self.check_expr(clo, Some(want));
        let r = match self.tcx.kind(cty).clone() {
            TyKind::Func { params, ret, .. } if params.is_empty() => ret,
            TyKind::Error => return self.tcx.error,
            _ => {
                self.emit(clo.span, SemaErrorKind::Message(
                    "`Thread.spawn` expects a parameterless closure `() => R`".into(),
                ));
                return self.tcx.error;
            }
        };
        // A float result would be returned in a floating-point register, which
        // the integer-result thread shim cannot carry (a follow-up).
        if matches!(self.tcx.kind(r), TyKind::Float(_)) {
            self.emit(clo.span, SemaErrorKind::Message(
                "`Thread.spawn` cannot yet return a floating-point value".into(),
            ));
        }
        // Captures must be safe to share across threads: an immutable value, or
        // a thread-safe handle (`Sender`/`Receiver` — the channel itself is
        // synchronized; the struct only carries an id). Other managed values
        // would need a deep clone at the boundary (a follow-up — `docs/20` §1).
        if let Some(info) = self.results.closures.get(&clo.span).cloned() {
            for (_, cap_ty) in &info.captures {
                if !self.is_thread_shareable(*cap_ty) {
                    self.emit(clo.span, SemaErrorKind::Message(format!(
                        "`Thread.spawn` can only capture immutable values or channel \
                         endpoints so far; captured value of type `{}` would need a \
                         deep clone across the thread boundary (`docs/20` §1)",
                        self.display(*cap_ty)
                    )));
                }
            }
        }
        self.results.thread_spawns.insert(span, r);
        self.tcx.mk_named(self.prog.join_handle_def, vec![r])
    }

    /// `JoinHandle<R>.join(): Joined<R> | Panicked` and `.detach(): null`
    /// (`docs/20` §1).
    fn check_join_handle_method(&mut self, r: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        if !args.is_empty() {
            self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
        }
        match name.name.as_str() {
            "join" => {
                self.results.thread_joins.insert(span, r);
                let joined = self.tcx.mk_named(self.prog.joined_def, vec![r]);
                let panicked = self.tcx.mk_named(self.prog.panicked_def, Vec::new());
                self.tcx.mk_union([joined, panicked])
            }
            "detach" => self.tcx.null,
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`JoinHandle` has no method `{other}`"
                )));
                self.tcx.error
            }
        }
    }

    /// Try to interpret `recv_name.method(args)` as a static method call
    /// (`docs/09` §6, `docs/10`). Returns `Some(result type)` when `recv_name`
    /// is a concrete type or an in-scope generic parameter; `None` otherwise (so
    /// the caller falls through to the instance-method path).
    fn try_static_call(
        &mut self,
        recv_name: &str,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        generics: &[Type],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Option<Ty> {
        // A trailing closure is the call's final argument, as for instance calls.
        let merged: Vec<Expr>;
        let arg_slice: &[Expr] = match trailing {
            Some(tc) => {
                let mut v = args.to_vec();
                v.push(tc.clone());
                merged = v;
                &merged
            }
            None => args,
        };
        // (a) `T.static_method()` — a generic parameter, resolved via its bounds.
        if let Some(pty) = self.cur_generics.get(recv_name).copied() {
            if let TyKind::Param(pdef) = self.tcx.kind(pty).clone() {
                return Some(self.check_bound_static_call(pdef, pty, callee, method, arg_slice, span));
            }
        }
        // (b) `Type.static_method()` — a concrete (extendable) type.
        if let Some(def) = self.prog.resolve_type_in(self.current_module(), recv_name) {
            if matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
                return Some(self.check_type_static_call(def, callee, method, arg_slice, generics, span));
            }
        }
        None
    }

    /// `T.static_method(args)` where `T` is a generic parameter: the method must
    /// be a *static* method declared by one of `T`'s interface bounds. Codegen
    /// monomorphizes it to the concrete impl (`docs/10`).
    fn check_bound_static_call(
        &mut self,
        param: DefId,
        pty: Ty,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let Some((mdef, iface, iargs)) = self.resolve_bound_method(param, &method.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(method.span, SemaErrorKind::Message(format!(
                "no static method `{}` on type parameter `{}` through its bounds",
                method.name,
                self.display(pty)
            )));
            return self.tcx.error;
        };
        if !self.prog.def(mdef).is_static {
            self.emit(method.span, SemaErrorKind::Message(format!(
                "`{}` is an instance method; call it on a value, not on the type",
                method.name
            )));
        }
        self.results.resolutions.insert(callee.span, ValueRes::Method(mdef));
        self.results.static_calls.insert(callee.span);
        self.results.static_recv.insert(callee.span, pty);
        let (params, ret) = self.iface_method_sig(mdef, iface, &iargs, pty);
        if args.len() != params.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: params.len(), found: args.len() });
        }
        for (a, pt) in args.iter().zip(&params) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        ret
    }

    /// `Type.static_method(args)` for a concrete extendable type: resolve a
    /// static method declared in an `extend` of `Type` (`docs/09` §6).
    fn check_type_static_call(
        &mut self,
        struct_def: DefId,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        explicit_generics: &[Type],
        span: Span,
    ) -> Ty {
        // Form the receiver nominal type. Non-generic types have no arguments;
        // a generic type uses its own parameters (its static methods are
        // resolved structurally — concrete inference of the type's arguments
        // from a static call is a follow-up).
        let struct_gens = self.prog.def(struct_def).generics.clone();
        let recv_args: Vec<Ty> = struct_gens.iter().map(|g| self.tcx.mk_param(*g)).collect();
        let recv_ty = self.tcx.mk_named(struct_def, recv_args);
        let Some((mdef, ext_subst)) = self.resolve_method(recv_ty, &method.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(method.span, SemaErrorKind::Message(format!(
                "type `{}` has no static method `{}`",
                self.prog.def(struct_def).name, method.name
            )));
            return self.tcx.error;
        };
        if !self.prog.def(mdef).is_static {
            self.emit(method.span, SemaErrorKind::Message(format!(
                "`{}` is an instance method on `{}`; call it on a value",
                method.name, self.prog.def(struct_def).name
            )));
        }
        self.results.resolutions.insert(callee.span, ValueRes::Method(mdef));
        self.results.static_calls.insert(callee.span);
        self.results.static_recv.insert(callee.span, recv_ty);

        // Build the full substitution: the extend's generics (solved by the
        // receiver), then the method's own generics (from explicit `<...>`).
        let mut subst = ext_subst.clone();
        let env = self.local_env();
        let method_gens = self.prog.def(mdef).generics.clone();
        for (g, t) in method_gens.iter().zip(explicit_generics) {
            let gt = self.lower_ty(t, &env);
            subst.insert(*g, gt);
        }
        let (menv, _) = self.fn_env(mdef);
        let Some(ItemKind::Function(f)) = self.prog.def(mdef).item.clone() else {
            return self.tcx.error;
        };
        let param_tys: Vec<Ty> = f
            .params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Normal { ty, .. } => {
                    let t = self.lower_ty(ty, &menv);
                    Some(self.subst_ty(t, &subst))
                }
                ParamKind::SelfParam => None,
            })
            .collect();
        let ret = match &f.return_type {
            Some(t) => {
                let t = self.lower_ty(t, &menv);
                self.subst_ty(t, &subst)
            }
            None => self.tcx.null,
        };
        // Record monomorphization args: the extend's generics, then the method's.
        let parent = self.prog.def(mdef).parent;
        let ext_gens = parent.map(|p| self.prog.def(p).generics.clone()).unwrap_or_default();
        let mut targs: Vec<Ty> = ext_gens
            .iter()
            .map(|g| ext_subst.get(g).copied().unwrap_or(self.tcx.error))
            .collect();
        for g in &method_gens {
            targs.push(subst.get(g).copied().unwrap_or(self.tcx.error));
        }
        if !targs.is_empty() {
            self.results.call_type_args.insert(callee.span, targs);
        }
        if args.len() != param_tys.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: param_tys.len(), found: args.len() });
        }
        for (a, pt) in args.iter().zip(&param_tys) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        ret
    }

    /// Type-check `channel<T>(): (Sender<T>, Receiver<T>)` (`docs/20` §2).
    fn check_channel_new(&mut self, generics: &[Type], args: &[Expr], span: Span) -> Ty {
        if generics.len() != 1 {
            self.emit(span, SemaErrorKind::Message(
                "`channel` needs exactly one explicit type argument: `channel<T>()`".into(),
            ));
            return self.tcx.error;
        }
        let env = self.local_env();
        let elem = self.lower_ty(&generics[0], &env);
        // Only immutable element types are shared across threads for now (no
        // clone-on-send yet — `docs/20` §3); matches `Thread.spawn` captures.
        if !self.is_immutable_value(elem) && !self.tcx.is_error(elem) {
            self.emit(span, SemaErrorKind::Message(format!(
                "`channel` element type `{}` must be immutable so far (only \
                 primitives and `str` can cross threads without a deep clone)",
                self.display(elem)
            )));
        }
        if !args.is_empty() {
            self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
        }
        self.results.channel_news.insert(span);
        let sender = self.tcx.mk_named(self.prog.sender_def, vec![elem]);
        let receiver = self.tcx.mk_named(self.prog.receiver_def, vec![elem]);
        self.tcx.mk_tuple(vec![sender, receiver])
    }

    /// `block_on(fut): Out` (`docs/21` §6) — run a future to completion on the
    /// current thread. The argument must be a `Future<Out>`; the result is `Out`.
    fn check_block_on(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        }
        let fty = self.check_expr(&args[0], None);
        match self.future_output(fty) {
            Some(out) => {
                self.results.block_ons.insert(span, out);
                out
            }
            None => {
                if !self.tcx.is_error(fty) {
                    let t = self.display(fty);
                    self.emit(args[0].span, SemaErrorKind::Message(format!(
                        "`block_on` requires a `Future`, but `{t}` is not one"
                    )));
                }
                self.tcx.error
            }
        }
    }

    /// `spawn(fut): JoinHandle<Out>` (`docs/21` §6) — drive a future on a worker
    /// thread. The argument must be a `Future<Out>`.
    fn check_async_spawn(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        }
        let fty = self.check_expr(&args[0], None);
        match self.future_output(fty) {
            Some(out) => {
                self.results.async_spawns.insert(span, out);
                self.tcx.mk_named(self.prog.join_handle_def, vec![out])
            }
            None => {
                if !self.tcx.is_error(fty) {
                    let t = self.display(fty);
                    self.emit(args[0].span, SemaErrorKind::Message(format!(
                        "`spawn` requires a `Future`, but `{t}` is not one"
                    )));
                }
                self.tcx.error
            }
        }
    }

    /// `Sender<T>` / `Receiver<T>` builtin methods (`docs/20` §2).
    fn check_channel_method(&mut self, def: DefId, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let is_sender = def == self.prog.sender_def;
        match (is_sender, name.name.as_str()) {
            (true, "send") => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                } else {
                    let at = self.check_expr(&args[0], Some(elem));
                    self.expect(at, elem, args[0].span);
                }
                self.tcx.null
            }
            (false, "recv") => {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                elem
            }
            (false, "try_recv") => {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.tcx.mk_union([elem, self.tcx.null])
            }
            _ => {
                let tn = if is_sender { "Sender" } else { "Receiver" };
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`{tn}` has no method `{}`", name.name
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Type-check `Shared.new(value): Shared<T>` (`docs/20` §4). `T` is inferred
    /// from the value.
    fn check_shared_new(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        }
        let elem = self.check_expr(&args[0], None);
        self.results.shared_news.insert(span);
        self.tcx.mk_named(self.prog.shared_def, vec![elem])
    }

    /// `Shared<T>` builtin methods (`docs/20` §4): `lock`/`try_lock` run a
    /// closure under the mutex with exclusive access to the value.
    fn check_shared_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        match name.name.as_str() {
            "lock" | "try_lock" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                // The body is `(T) => R`; `R` is inferred from the closure.
                let want = self.tcx.mk_func(vec![elem], self.tcx.error, false);
                let cty = self.check_expr(&args[0], Some(want));
                let r = match self.tcx.kind(cty).clone() {
                    TyKind::Func { ret, .. } => ret,
                    _ => self.tcx.error,
                };
                if name.name == "lock" {
                    r
                } else {
                    let busy = self.tcx.mk_named(self.prog.lock_busy_def, Vec::new());
                    self.tcx.mk_union([r, busy])
                }
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`Shared` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// `T.MIN`/`T.MAX` (integers) and `f*.INFINITY`/`NEG_INFINITY`/`NAN`
    /// (`docs/18` §10). Returns the constant's type, or `None` if `tyname` is not
    /// a primitive numeric type with that constant.
    fn check_num_constant(&mut self, tyname: &str, name: &Ident, field_span: Span) -> Option<Ty> {
        use crate::sema::results::NumIntrinsic;
        if let Some(it) = IntTy::from_name(tyname) {
            let ty = self.tcx.int(it);
            let intr = match name.name.as_str() {
                "MIN" => NumIntrinsic::IntBound { ty, max: false },
                "MAX" => NumIntrinsic::IntBound { ty, max: true },
                _ => return None,
            };
            self.results.num_intrinsics.insert(field_span, intr);
            return Some(ty);
        }
        if let Some(ft) = FloatTy::from_name(tyname) {
            let ty = self.tcx.float(ft);
            let kind = match name.name.as_str() {
                "INFINITY" => 0u8,
                "NEG_INFINITY" => 1,
                "NAN" => 2,
                _ => return None,
            };
            self.results.num_intrinsics.insert(field_span, NumIntrinsic::FloatConst { ty, kind });
            return Some(ty);
        }
        None
    }

    /// Numeric-namespace methods on a primitive type (`docs/18` §10, `docs/14`
    /// §5): the `{wrapping,saturating,checked,overflowing}_{add,sub,mul}` integer
    /// families and the `f*.is_nan`/`is_infinite`/`is_finite` float predicates.
    fn check_num_method(&mut self, tyname: &str, name: &Ident, args: &[Expr], span: Span) -> Option<Ty> {
        use crate::sema::results::NumIntrinsic;
        if let Some(ft) = FloatTy::from_name(tyname) {
            let fty = self.tcx.float(ft);
            let kind = match name.name.as_str() {
                "is_nan" => 0u8,
                "is_infinite" => 1,
                "is_finite" => 2,
                _ => return None,
            };
            self.check_num_args(args, &[fty], span);
            self.results.num_intrinsics.insert(span, NumIntrinsic::FloatPred { ty: fty, kind });
            return Some(self.tcx.bool);
        }
        let it = IntTy::from_name(tyname)?;
        let ity = self.tcx.int(it);
        let (family, base) = if let Some(b) = name.name.strip_prefix("wrapping_") {
            (0u8, b)
        } else if let Some(b) = name.name.strip_prefix("saturating_") {
            (1, b)
        } else if let Some(b) = name.name.strip_prefix("checked_") {
            (2, b)
        } else if let Some(b) = name.name.strip_prefix("overflowing_") {
            (3, b)
        } else {
            return None;
        };
        let op = match base {
            "add" => 0u8,
            "sub" => 1,
            "mul" => 2,
            _ => return None,
        };
        self.check_num_args(args, &[ity, ity], span);
        self.results.num_intrinsics.insert(span, NumIntrinsic::IntArith { ty: ity, family, op });
        // Result type by family: wrapping/saturating → T; checked → T | null;
        // overflowing → (T, bool).
        Some(match family {
            2 => self.tcx.mk_union([ity, self.tcx.null]),
            3 => self.tcx.mk_tuple(vec![ity, self.tcx.bool]),
            _ => ity,
        })
    }

    /// Check positional args against expected primitive types (for numeric
    /// intrinsics).
    fn check_num_args(&mut self, args: &[Expr], expect: &[Ty], span: Span) {
        if args.len() != expect.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
        }
        for (a, e) in args.iter().zip(expect) {
            let at = self.check_expr(a, Some(*e));
            self.expect(at, *e, a.span);
        }
    }

    // -- methods -------------------------------------------------------------

    fn check_method_call(
        &mut self,
        callee: &Expr,
        receiver: &Expr,
        name: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let rty = self.check_expr(receiver, None);
        if self.tcx.is_error(rty) {
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        }
        // `.clone()` (`docs/10`/`docs/15`): a builtin for primitives, `str`, and
        // immutable-element `List`/`Map`. User/derived `clone` methods resolve
        // through the normal method path below.
        if name.name == "clone" && args.is_empty() {
            if let Some(t) = self.check_builtin_clone(rty, callee.span, name.span) {
                return t;
            }
        }
        // Builtin `List<T>` methods are resolved specially.
        if let Some(elem) = self.list_elem(rty) {
            return self.check_list_method(elem, name, args, span);
        }
        // Builtin `Map<K, V>` methods.
        if let Some((kt, vt)) = self.map_kv(rty) {
            return self.check_map_method(kt, vt, name, args, span);
        }
        // Builtin `str` methods.
        if rty == self.tcx.str {
            return self.check_str_method(name, args, span);
        }
        // Builtin `JoinHandle<R>` methods (`docs/20` §1).
        if let TyKind::Named { def, args: targs } = self.tcx.kind(rty).clone() {
            if def == self.prog.join_handle_def && self.prog.join_handle_def != DefId(0) {
                let r = targs.first().copied().unwrap_or(self.tcx.error);
                return self.check_join_handle_method(r, name, args, span);
            }
            // Builtin `Sender<T>` / `Receiver<T>` methods (`docs/20` §2).
            if (def == self.prog.sender_def || def == self.prog.receiver_def)
                && self.prog.sender_def != DefId(0)
            {
                let elem = targs.first().copied().unwrap_or(self.tcx.error);
                return self.check_channel_method(def, elem, name, args, span);
            }
            // Builtin `Shared<T>` methods (`docs/20` §4).
            if def == self.prog.shared_def && self.prog.shared_def != DefId(0) {
                let elem = targs.first().copied().unwrap_or(self.tcx.error);
                return self.check_shared_method(elem, name, args, span);
            }
            // `Future<T>.cancel()` (`docs/21` §8): for the compute-only futures
            // we build there is no I/O registration to release, so cancel is a
            // safe no-op. Callable repeatedly.
            if def == self.prog.future_def && self.prog.future_def != DefId(0)
                && name.name == "cancel"
            {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.results.future_cancels.insert(callee.span);
                return self.tcx.null;
            }
        }
        // A method on a generic type parameter resolves through its bounds to
        // an interface method (monomorphized to the concrete impl in codegen).
        if let TyKind::Param(p) = self.tcx.kind(rty).clone() {
            return self.check_bound_method_call(p, rty, callee, name, args, span);
        }
        // A method on an interface object dispatches dynamically (via vtable).
        if let TyKind::Named { def, .. } = self.tcx.kind(rty).clone() {
            if self.prog.def(def).kind == DefKind::Interface {
                return self.check_dyn_method_call(def, rty, callee, name, args, span);
            }
        }
        let Some((method_def, ext_subst)) = self.resolve_method(rty, &name.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(name.span, SemaErrorKind::Message(format!(
                "no method `{}` on type `{}`", name.name, self.display(rty)
            )));
            return self.tcx.error;
        };
        self.results.resolutions.insert(callee.span, ValueRes::Method(method_def));

        let (env, _) = self.fn_env(method_def);
        let Some(ItemKind::Function(f)) = self.prog.def(method_def).item.clone() else {
            return self.tcx.error;
        };
        // Parameter/return types are written in terms of the `extend`'s generic
        // params; substitute the solved bindings for this concrete receiver.
        let param_tys: Vec<Ty> = f
            .params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Normal { ty, .. } => {
                    let t = self.lower_ty(ty, &env);
                    Some(self.subst_ty(t, &ext_subst))
                }
                ParamKind::SelfParam => None,
            })
            .collect();
        let ret = match &f.return_type {
            Some(t) => {
                let t = self.lower_ty(t, &env);
                self.subst_ty(t, &ext_subst)
            }
            None => self.tcx.null,
        };
        // Record the extend's generic arguments (in declaration order) so codegen
        // monomorphizes the method to this receiver's instantiation.
        if let Some(parent) = self.prog.def(method_def).parent {
            let ext_gens = self.prog.def(parent).generics.clone();
            if !ext_gens.is_empty() {
                let targs: Vec<Ty> = ext_gens
                    .iter()
                    .map(|g| ext_subst.get(g).copied().unwrap_or(self.tcx.error))
                    .collect();
                self.results.call_type_args.insert(callee.span, targs);
            }
        }
        if args.len() != param_tys.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: param_tys.len(),
                found: args.len(),
            });
        }
        for (a, pt) in args.iter().zip(&param_tys) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        ret
    }

    /// If `ity` implements the iterator protocol — has a method
    /// `next(self): Item<U> | Done` — return its element type `U` and the
    /// resolved method. Drives `for` over user types (`docs/18` §8).
    fn iterator_elem(&mut self, ity: Ty) -> Option<(Ty, DefId, Vec<Ty>, Ty, Ty)> {
        if self.tcx.is_error(ity) {
            return None;
        }
        // A bounded type parameter (`T: Iterator<U>`) or an interface object
        // drives the loop through its `next` interface method (resolved to the
        // concrete impl in codegen).
        let (next, ret, next_targs) = if let TyKind::Param(p) = self.tcx.kind(ity).clone() {
            let (method, iface, iargs) = self.resolve_bound_method(p, "next")?;
            let (_, ret) = self.iface_method_sig(method, iface, &iargs, ity);
            (method, ret, Vec::new())
        } else if self.is_interface(ity) {
            let TyKind::Named { def: iface, args } = self.tcx.kind(ity).clone() else {
                return None;
            };
            let method = (0..self.prog.defs.len() as u32).map(DefId).find(|&d| {
                let def = self.prog.def(d);
                def.kind == DefKind::InterfaceMethod && def.parent == Some(iface) && def.name == "next"
            })?;
            let (_, ret) = self.iface_method_sig(method, iface, &args, ity);
            (method, ret, Vec::new())
        } else {
            let (next, ext_subst) = self.resolve_method(ity, "next")?;
            // The enclosing extend's generic arguments, to monomorphize `next`.
            let next_targs: Vec<Ty> = match self.prog.def(next).parent {
                Some(p) => self.prog.def(p).generics.clone()
                    .iter()
                    .map(|g| ext_subst.get(g).copied().unwrap_or(self.tcx.error))
                    .collect(),
                None => Vec::new(),
            };
            let (env, _) = self.fn_env(next);
            let Some(ItemKind::Function(f)) = self.prog.def(next).item.clone() else {
                return None;
            };
            let ret = match &f.return_type {
                Some(t) => {
                    let t = self.lower_ty(t, &env);
                    self.subst_ty(t, &ext_subst)
                }
                None => return None,
            };
            (next, ret, next_targs)
        };
        let members = match self.tcx.kind(ret).clone() {
            TyKind::Union(ms) => ms,
            _ => vec![ret],
        };
        let mut item = None;
        let mut done = None;
        for m in members {
            match self.tcx.kind(m).clone() {
                TyKind::Named { def, args } if def == self.prog.item_def && args.len() == 1 => {
                    item = Some((args[0], m));
                }
                TyKind::Named { def, .. } if def == self.prog.done_def => {
                    done = Some(m);
                }
                _ => {}
            }
        }
        match (item, done) {
            (Some((u, item_ty)), Some(done_ty)) => Some((u, next, next_targs, item_ty, done_ty)),
            _ => None,
        }
    }

    /// Resolve the `AsyncIterator<T>` protocol on `ity` (`docs/21` §10): a
    /// `next_async(self): Future<Item<T> | Done>` method. Returns the loop's
    /// resolution, or `None` if `ity` is not an async stream.
    fn async_iterator_elem(&mut self, ity: Ty) -> Option<crate::sema::results::ForAsyncIter> {
        if self.tcx.is_error(ity) {
            return None;
        }
        let (next_async, ext_subst) = self.resolve_method(ity, "next_async")?;
        let next_targs: Vec<Ty> = match self.prog.def(next_async).parent {
            Some(p) => self.prog.def(p).generics.clone()
                .iter()
                .map(|g| ext_subst.get(g).copied().unwrap_or(self.tcx.error))
                .collect(),
            None => Vec::new(),
        };
        let (env, _) = self.fn_env(next_async);
        let Some(ItemKind::Function(f)) = self.prog.def(next_async).item.clone() else {
            return None;
        };
        // The return type is `Future<Item<T> | Done>`; unwrap to `Item<T> | Done`.
        let ret = self.lower_ty(f.return_type.as_ref()?, &env);
        let ret = self.subst_ty(ret, &ext_subst);
        let union_ty = self.future_output(ret)?;
        let members = match self.tcx.kind(union_ty).clone() {
            TyKind::Union(ms) => ms,
            _ => vec![union_ty],
        };
        let mut item = None;
        let mut done = None;
        for m in members {
            match self.tcx.kind(m).clone() {
                TyKind::Named { def, args } if def == self.prog.item_def && args.len() == 1 => {
                    item = Some((args[0], m));
                }
                TyKind::Named { def, .. } if def == self.prog.done_def => {
                    done = Some(m);
                }
                _ => {}
            }
        }
        let ((elem, item_ty), done_ty) = (item?, done?);
        Some(crate::sema::results::ForAsyncIter {
            elem, next_async, next_targs, iter_ty: ity, item_ty, done_ty, union_ty,
        })
    }

    /// The interface bounds (`T: A + B`) on a generic-parameter def, lowered to
    /// `(interface def, interface type args)` pairs against the owner's env.
    fn bound_ifaces(&mut self, param: DefId) -> Vec<(DefId, Vec<Ty>)> {
        let bounds = self.prog.def(param).param_bounds.clone();
        if bounds.is_empty() {
            return Vec::new();
        }
        let owner = self.prog.def(param).parent;
        let env = match owner {
            Some(o) => self.def_env(o, None),
            None => self.local_env(),
        };
        let module = owner.map_or(ModId::ROOT, |o| self.prog.def(o).module);
        let mut out = Vec::new();
        for b in &bounds {
            let TypeKind::Named { name, generics } = &b.kind else { continue };
            let Some(idef) = self.prog.resolve_type_in(module, &name.name) else {
                continue;
            };
            if self.prog.def(idef).kind != DefKind::Interface {
                continue;
            }
            let args: Vec<Ty> = generics.iter().map(|g| self.lower_ty(g, &env)).collect();
            out.push((idef, args));
        }
        out
    }

    /// Structural match: does `pat` (which may contain `Param`s) match `val`?
    /// Used to test whether an `extend` target applies to a concrete type.
    fn ty_matches(&self, pat: Ty, val: Ty) -> bool {
        match (self.tcx.kind(pat).clone(), self.tcx.kind(val).clone()) {
            (TyKind::Param(_), _) => true,
            (TyKind::Named { def: d1, args: a1 }, TyKind::Named { def: d2, args: a2 }) => {
                d1 == d2 && a1.len() == a2.len()
                    && a1.iter().zip(&a2).all(|(p, v)| self.ty_matches(*p, *v))
            }
            (TyKind::Tuple(p), TyKind::Tuple(v)) => {
                p.len() == v.len() && p.iter().zip(&v).all(|(a, b)| self.ty_matches(*a, *b))
            }
            (TyKind::Ptr(p), TyKind::Ptr(v)) => self.ty_matches(p, v),
            _ => pat == val,
        }
    }

    /// Does `ty` implement interface `iface` via some visible `extend` block?
    fn type_implements(&mut self, ty: Ty, iface: DefId) -> bool {
        // An interface object of the same interface trivially satisfies it (a
        // `dyn I` value can be used where `T: I` is required).
        if matches!(self.tcx.kind(ty), TyKind::Named { def, .. } if *def == iface) {
            return true;
        }
        // `Clone` is intrinsic for immutable values and immutable-element
        // collections (`docs/15` §8); user types satisfy it via a `clone` impl,
        // handled by the general `extend`-scan below.
        if iface == self.prog.clone_def && iface != DefId(0) {
            if self.is_immutable_value(ty) {
                return true;
            }
            if let Some(elem) = self.list_elem(ty) {
                return self.is_immutable_value(elem);
            }
            if let Some((kt, vt)) = self.map_kv(ty) {
                return self.is_immutable_value(kt) && self.is_immutable_value(vt);
            }
        }
        let module = self.current_module();
        let extends = self.prog.module(module).extends.clone();
        for ext in extends {
            let Some(ItemKind::Extend(e)) = self.prog.def(ext).item.clone() else { continue };
            let mut env = TypeEnv::new(self.prog.def(ext).module);
            for g in self.prog.def(ext).generics.clone() {
                let nm = self.prog.def(g).name.clone();
                let pty = self.tcx.mk_param(g);
                env.generics.insert(nm, pty);
            }
            let target = self.lower_ty(&e.target, &env);
            if !self.ty_matches(target, ty) {
                continue;
            }
            for itf in &e.interfaces {
                if let TypeKind::Named { name, .. } = &itf.kind {
                    if self.prog.resolve_type_in(module, &name.name) == Some(iface) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Check that every type argument satisfies its parameter's bounds.
    fn check_bounds(&mut self, gens: &[DefId], args: &[Ty], span: Span) {
        for (g, &arg) in gens.iter().zip(args) {
            if self.tcx.is_error(arg) {
                continue;
            }
            for (iface, _iargs) in self.bound_ifaces(*g) {
                if !self.type_implements(arg, iface) {
                    let an = self.display(arg);
                    let inm = self.prog.def(iface).name.clone();
                    self.emit(span, SemaErrorKind::Message(format!(
                        "type `{an}` does not implement `{inm}`, required by this bound"
                    )));
                }
            }
        }
    }

    /// Find an interface method `name` reachable through a type parameter's
    /// bounds. Returns the interface-method def and the bound's type arguments.
    fn resolve_bound_method(&mut self, param: DefId, name: &str) -> Option<(DefId, DefId, Vec<Ty>)> {
        for (iface, iargs) in self.bound_ifaces(param) {
            for id in 0..self.prog.defs.len() {
                let d = DefId(id as u32);
                let def = self.prog.def(d);
                if def.kind == DefKind::InterfaceMethod
                    && def.parent == Some(iface)
                    && def.name == name
                {
                    return Some((d, iface, iargs));
                }
            }
        }
        None
    }

    /// The (non-self) parameter types and return type of an interface method,
    /// lowered with the interface's generics bound to `iargs` and `Self` bound
    /// to `self_ty` (the calling type parameter).
    fn iface_method_sig(
        &mut self,
        method: DefId,
        iface: DefId,
        iargs: &[Ty],
        self_ty: Ty,
    ) -> (Vec<Ty>, Ty) {
        let module = self.prog.def(method).module;
        let mut env = TypeEnv::new(module);
        for (g, a) in self.prog.def(iface).generics.clone().iter().zip(iargs) {
            env.generics.insert(self.prog.def(*g).name.clone(), *a);
        }
        for g in self.prog.def(method).generics.clone() {
            let pty = self.tcx.mk_param(g);
            env.generics.insert(self.prog.def(g).name.clone(), pty);
        }
        env.self_ty = Some(self_ty);
        let Some(ItemKind::Function(f)) = self.prog.def(method).item.clone() else {
            return (Vec::new(), self.tcx.error);
        };
        let params = f
            .params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Normal { ty, .. } => Some(self.lower_ty(ty, &env)),
                ParamKind::SelfParam => None,
            })
            .collect();
        let ret = match &f.return_type {
            Some(t) => self.lower_ty(t, &env),
            None => self.tcx.null,
        };
        (params, ret)
    }

    /// Type-check `x.method(args)` where `x: T` and `T` is a generic parameter:
    /// the method must be declared by one of `T`'s interface bounds.
    fn check_bound_method_call(
        &mut self,
        param: DefId,
        rty: Ty,
        callee: &Expr,
        name: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let Some((method, iface, iargs)) = self.resolve_bound_method(param, &name.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(name.span, SemaErrorKind::Message(format!(
                "no method `{}` is available on type parameter `{}` through its bounds",
                name.name,
                self.display(rty)
            )));
            return self.tcx.error;
        };
        // Record the interface method; codegen monomorphizes it to the concrete
        // `extend` impl of whatever the type parameter is instantiated with.
        self.results.resolutions.insert(callee.span, ValueRes::Method(method));
        let (params, ret) = self.iface_method_sig(method, iface, &iargs, rty);
        if args.len() != params.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: params.len(),
                found: args.len(),
            });
        }
        for (a, pt) in args.iter().zip(&params) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        ret
    }

    /// Type-check `obj.method(args)` where `obj` has an interface (object) type.
    /// Dispatch is dynamic; codegen routes through the object's vtable.
    fn check_dyn_method_call(
        &mut self,
        iface: DefId,
        rty: Ty,
        callee: &Expr,
        name: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let iargs: Vec<Ty> = match self.tcx.kind(rty).clone() {
            TyKind::Named { args, .. } => args,
            _ => Vec::new(),
        };
        let method = (0..self.prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = self.prog.def(d);
            def.kind == DefKind::InterfaceMethod && def.parent == Some(iface) && def.name == name.name
        });
        let Some(method) = method else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(name.span, SemaErrorKind::Message(format!(
                "interface `{}` has no method `{}`",
                self.display(rty),
                name.name
            )));
            return self.tcx.error;
        };
        self.results.resolutions.insert(callee.span, ValueRes::Method(method));
        let (params, ret) = self.iface_method_sig(method, iface, &iargs, rty);
        if args.len() != params.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: params.len(),
                found: args.len(),
            });
        }
        for (a, pt) in args.iter().zip(&params) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        ret
    }

    /// Find an inherent method `name` for `recv_ty` among visible `extend`
    /// blocks (orphan rule keeps the candidate set local), returning the method
    /// def and the substitution that maps the `extend`'s generic parameters to
    /// `recv_ty`'s concrete arguments (`extend<T> Pair<T>` against `Pair<i64>`
    /// binds `T → i64`; empty for a concrete `extend`).
    /// The `to_str(self): str` method for `ty`, if one exists (hand-written or
    /// `@Derive(ToStr)`-synthesised). Used to make a user type interpolatable.
    fn tostr_method(&mut self, ty: Ty) -> Option<DefId> {
        if !matches!(self.tcx.kind(ty), TyKind::Named { .. }) {
            return None;
        }
        let (mdef, subst) = self.resolve_method(ty, "to_str")?;
        // It must take only `self` and return `str`.
        let (env, _) = self.fn_env(mdef);
        let Some(ItemKind::Function(f)) = self.prog.def(mdef).item.clone() else {
            return None;
        };
        let takes_only_self = f.params.iter().all(|p| matches!(p.kind, ParamKind::SelfParam));
        let ret = match &f.return_type {
            Some(t) => {
                let lowered = self.lower_ty(t, &env);
                self.subst_ty(lowered, &subst)
            }
            None => self.tcx.null,
        };
        (takes_only_self && ret == self.tcx.str).then_some(mdef)
    }

    fn resolve_method(&mut self, recv_ty: Ty, name: &str) -> Option<(DefId, HashMap<DefId, Ty>)> {
        for id in 0..self.prog.defs.len() {
            let d = DefId(id as u32);
            if self.prog.def(d).kind != DefKind::ExtendMethod
                || self.prog.def(d).name != name
            {
                continue;
            }
            let parent = self.prog.def(d).parent?;
            let Some(ItemKind::Extend(e)) = self.prog.def(parent).item.clone() else {
                continue;
            };
            let mut env = TypeEnv::new(self.prog.def(parent).module);
            for g in self.prog.def(parent).generics.clone() {
                let nm = self.prog.def(g).name.clone();
                let pty = self.tcx.mk_param(g);
                env.generics.insert(nm, pty);
            }
            let target = self.lower_ty(&e.target, &env);
            if self.ty_matches(target, recv_ty) {
                let mut map = HashMap::new();
                self.unify(target, recv_ty, &mut map);
                return Some((d, map));
            }
        }
        None
    }

    /// Check a call to a generic free function: infer (or take explicit) type
    /// arguments, check arguments against the substituted parameter types, and
    /// record the instantiation for monomorphization.
    fn check_generic_call(
        &mut self,
        def: DefId,
        callee: &Expr,
        args: &[Expr],
        explicit: &[Type],
        span: Span,
    ) -> Ty {
        self.results.resolutions.insert(callee.span, ValueRes::Function(def));
        let gens = self.prog.def(def).generics.clone();
        let (params, ret) = match self.prog.def(def).item.clone() {
            Some(ItemKind::Function(f)) => (f.params, f.return_type),
            Some(ItemKind::Extern(ExternItem::Function(f))) => (f.params, f.return_type),
            _ => return self.tcx.error,
        };
        let env = self.def_env(def, None); // generic params → Param(g)

        // The normal (non-self) parameter AST types.
        let param_tys: Vec<Ty> = params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Normal { ty, .. } => Some(self.lower_ty(ty, &env)),
                ParamKind::SelfParam => None,
            })
            .collect();

        // Solve the type parameters.
        let mut map: HashMap<DefId, Ty> = HashMap::new();
        if !explicit.is_empty() {
            let local = self.local_env();
            for (g, t) in gens.iter().zip(explicit) {
                let ty = self.lower_ty(t, &local);
                map.insert(*g, ty);
            }
            for a in args {
                self.check_expr(a, None);
            }
        } else {
            // Infer from argument types.
            for (i, a) in args.iter().enumerate() {
                let aty = self.check_expr(a, None);
                if let Some(pty) = param_tys.get(i) {
                    self.unify(*pty, aty, &mut map);
                }
            }
        }

        if args.len() != param_tys.len() {
            self.emit(span, SemaErrorKind::ArgCount {
                expected: param_tys.len(),
                found: args.len(),
            });
        }

        // Record the instantiation arguments in declaration order.
        let type_args: Vec<Ty> =
            gens.iter().map(|g| map.get(g).copied().unwrap_or(self.tcx.error)).collect();
        self.results.call_type_args.insert(callee.span, type_args.clone());
        // Enforce each parameter's interface bounds against its argument.
        self.check_bounds(&gens, &type_args, span);

        // Check each argument against its substituted parameter type.
        for (i, a) in args.iter().enumerate() {
            if let Some(pty) = param_tys.get(i) {
                let expected = self.subst_ty(*pty, &map);
                let aty = self.results.expr_types.get(&a.span).copied().unwrap_or(self.tcx.error);
                self.expect(aty, expected, a.span);
            }
        }

        match &ret {
            Some(t) => {
                let r = self.lower_ty(t, &env);
                self.subst_ty(r, &map)
            }
            None => self.tcx.null,
        }
    }

    fn check_unary(&mut self, op: UnaryOp, operand: &Expr, op_span: Span) -> Ty {
        let ty = self.check_expr(operand, None);
        if self.tcx.is_error(ty) {
            return self.tcx.error;
        }
        match op {
            UnaryOp::Neg => {
                if self.is_numeric(ty) {
                    ty
                } else {
                    self.op_error("-", ty, op_span)
                }
            }
            UnaryOp::Not => {
                if ty == self.tcx.bool || self.is_integer(ty) {
                    ty
                } else {
                    self.op_error("!", ty, op_span)
                }
            }
            UnaryOp::BitNot => {
                if self.is_integer(ty) {
                    ty
                } else {
                    self.op_error("~", ty, op_span)
                }
            }
        }
    }

    fn check_binary(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        op_span: Span,
    ) -> Ty {
        use BinaryOp::*;
        // Logical operators are built-in on bool and short-circuit.
        if matches!(op, And | Or) {
            let l = self.check_expr(left, Some(self.tcx.bool));
            let r = self.check_expr(right, Some(self.tcx.bool));
            self.expect(l, self.tcx.bool, left.span);
            self.expect(r, self.tcx.bool, right.span);
            return self.tcx.bool;
        }

        let l = self.check_expr(left, None);
        let r = self.check_expr(right, Some(l));
        if self.tcx.is_error(l) || self.tcx.is_error(r) {
            return self.tcx.error;
        }

        // Operator overloading: a user nominal operand dispatches to its
        // `extend` method (`a + b` → `Add.add`, `a == b` → `Eq.eq`, …).
        if let Some(result) = self.try_operator_overload(op, l, r, right, op_span) {
            return result;
        }

        match op {
            // `str + str` is concatenation (str implements Add<str, str>).
            Add if l == self.tcx.str => {
                self.expect(r, self.tcx.str, right.span);
                self.tcx.str
            }
            Add | Sub | Mul | Div | Rem => {
                if self.is_numeric(l) && l == r {
                    l
                } else if self.is_numeric(l) {
                    self.expect(r, l, right.span);
                    l
                } else {
                    self.op_error(binop_str(op), l, op_span)
                }
            }
            Eq | Ne => {
                self.expect(r, l, right.span);
                self.tcx.bool
            }
            Lt | Le | Gt | Ge => {
                if self.is_ordered(l) {
                    self.expect(r, l, right.span);
                } else {
                    self.op_error(binop_str(op), l, op_span);
                }
                self.tcx.bool
            }
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if self.is_integer(l) {
                    self.expect(r, l, right.span);
                    l
                } else {
                    self.op_error(binop_str(op), l, op_span)
                }
            }
            And | Or => unreachable!("handled above"),
        }
    }

    /// Resolve an overloaded operator to an `extend` method when the left
    /// operand is a user nominal type. Returns `None` for builtin operands
    /// (numerics, `str`, `bool`, …) so the primitive handling applies.
    fn try_operator_overload(
        &mut self,
        op: BinaryOp,
        l: Ty,
        r: Ty,
        right: &Expr,
        op_span: Span,
    ) -> Option<Ty> {
        use BinaryOp::*;
        if !matches!(self.tcx.kind(l), TyKind::Named { .. }) {
            return None;
        }
        let name = match op {
            Add => "add",
            Sub => "sub",
            Mul => "mul",
            Div => "div",
            Rem => "mod",
            BitAnd => "bitand",
            BitOr => "bitor",
            BitXor => "bitxor",
            Shl => "shl",
            Shr => "shr",
            Eq | Ne => "eq",
            Lt => "lt",
            Le => "le",
            Gt => "gt",
            Ge => "ge",
            And | Or => return None,
        };
        let Some((mdef, op_subst)) = self.resolve_method(l, name) else {
            self.emit(op_span, SemaErrorKind::Message(format!(
                "operator `{}` is not implemented for `{}`",
                binop_str(op),
                self.display(l)
            )));
            return Some(self.tcx.error);
        };
        self.results.operator_methods.insert(op_span, mdef);

        let (env, _) = self.fn_env(mdef);
        let Some(ItemKind::Function(f)) = self.prog.def(mdef).item.clone() else {
            return Some(self.tcx.error);
        };
        // Check the right operand against the method's (first non-self) param.
        let rhs_ty = f.params.iter().find_map(|p| match &p.kind {
            ParamKind::Normal { ty, .. } => {
                let t = self.lower_ty(ty, &env);
                Some(self.subst_ty(t, &op_subst))
            }
            ParamKind::SelfParam => None,
        });
        if let Some(rt) = rhs_ty {
            self.expect(r, rt, right.span);
        }
        // Equality is always `bool`; otherwise use the method's return type.
        match op {
            Eq | Ne => Some(self.tcx.bool),
            _ => Some(match &f.return_type {
                Some(t) => {
                    let t = self.lower_ty(t, &env);
                    self.subst_ty(t, &op_subst)
                }
                None => self.tcx.null,
            }),
        }
    }

    fn check_if(
        &mut self,
        cond: &Expr,
        then_block: &Block,
        else_branch: Option<&ElseBranch>,
        expected: Option<Ty>,
    ) -> Ty {
        let cty = self.check_expr(cond, Some(self.tcx.bool));
        if !self.tcx.is_error(cty) && cty != self.tcx.bool {
            let found = self.display(cty);
            self.emit(cond.span, SemaErrorKind::NonBoolCondition { found });
        }
        // `if x is T` narrows `x` to `T` in the then-branch and to the
        // complement in the else-branch (`docs/12` §4).
        let facts = self.narrow_facts(cond);

        let then_ty = {
            let saved = facts.map(|(id, t, _)| (id, self.narrowings.insert(id, t)));
            let ty = self.check_block(then_block, expected);
            self.restore_narrowing(saved);
            ty
        };
        match else_branch {
            None => self.tcx.null,
            Some(ElseBranch::Block(b)) => {
                let saved = facts.map(|(id, _, t)| (id, self.narrowings.insert(id, t)));
                let else_ty = self.check_block(b, expected);
                self.restore_narrowing(saved);
                self.tcx.mk_union([then_ty, else_ty])
            }
            Some(ElseBranch::If(e)) => {
                let saved = facts.map(|(id, _, t)| (id, self.narrowings.insert(id, t)));
                let else_ty = self.check_expr(e, expected);
                self.restore_narrowing(saved);
                self.tcx.mk_union([then_ty, else_ty])
            }
        }
    }

    /// Extract a narrowing fact `(local, then_type, else_type)` from a
    /// condition of the form `ident is T`.
    fn narrow_facts(&mut self, cond: &Expr) -> Option<(LocalId, Ty, Ty)> {
        let ExprKind::Cast { op: CastOp::Is, expr: inner, ty, .. } = &cond.kind else {
            return None;
        };
        let ExprKind::Ident(name) = &inner.kind else { return None };
        let (lty, id) = self.lookup(&name.name)?;
        let env = self.local_env();
        let t = self.lower_ty(ty, &env);
        // The complement is relative to the binding's *current* (possibly
        // already narrowed) type, so `else if` chains compose.
        let base = self.narrowings.get(&id).copied().unwrap_or(lty);
        let then_t = t;
        let remaining: Vec<Ty> = self
            .tcx
            .variants(base)
            .into_iter()
            .filter(|v| !self.tcx.variants(t).contains(v))
            .collect();
        let else_t = if remaining.is_empty() { base } else { self.tcx.mk_union(remaining) };
        Some((id, then_t, else_t))
    }

    fn restore_narrowing(&mut self, saved: Option<(LocalId, Option<Ty>)>) {
        if let Some((id, old)) = saved {
            match old {
                Some(o) => {
                    self.narrowings.insert(id, o);
                }
                None => {
                    self.narrowings.remove(&id);
                }
            }
        }
    }

    // -- literals ------------------------------------------------------------

    fn check_int_lit(&mut self, lit: &IntLit, expected: Option<Ty>, span: Span) -> Ty {
        // Rule order (docs/01 §4): suffix, then annotation/expected, then i64.
        let ty = if let Some(suffix) = &lit.suffix {
            match IntTy::from_name(suffix) {
                Some(it) => self.tcx.int(it),
                None => {
                    self.emit(span, SemaErrorKind::Message(format!(
                        "`{suffix}` is not a valid integer suffix"
                    )));
                    self.tcx.error
                }
            }
        } else if let Some(exp) = expected.map(|e| self.peel_for_int(e)) {
            exp.unwrap_or(self.tcx.int(IntTy::I64))
        } else {
            self.tcx.int(IntTy::I64)
        };
        // Range check against the chosen fixed-width type.
        if let TyKind::Int(it) = self.tcx.kind(ty) {
            self.check_int_range(lit, *it, span);
        }
        ty
    }

    /// If `expected` is (or narrows to) a single integer type, return it.
    fn peel_for_int(&self, expected: Ty) -> Option<Ty> {
        match self.tcx.kind(expected) {
            TyKind::Int(_) => Some(expected),
            _ => None,
        }
    }

    fn check_int_range(&mut self, lit: &IntLit, it: IntTy, span: Span) {
        let Some(bits) = it.bits() else { return }; // skip isize/usize
        let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
        let radix = match lit.base {
            IntBase::Dec => 10,
            IntBase::Hex => 16,
            IntBase::Oct => 8,
            IntBase::Bin => 2,
        };
        let Ok(v) = u128::from_str_radix(&digits, radix) else { return };
        let max: u128 = if it.is_signed() {
            (1u128 << (bits - 1)) - 1
        } else {
            (1u128 << bits) - 1
        };
        // Unary minus is applied separately; the literal itself is unsigned, so
        // the signed positive max is the bound here.
        if v > max {
            self.emit(span, SemaErrorKind::Message(format!(
                "literal `{}` does not fit in `{}`",
                digits,
                it.name()
            )));
        }
    }

    fn check_float_lit(&mut self, lit: &FloatLit, expected: Option<Ty>) -> Ty {
        if let Some(suffix) = &lit.suffix {
            match FloatTy::from_name(suffix) {
                Some(ft) => return self.tcx.float(ft),
                None => return self.tcx.error,
            }
        }
        if let Some(exp) = expected {
            if matches!(self.tcx.kind(exp), TyKind::Float(_)) {
                return exp;
            }
        }
        self.tcx.float(FloatTy::F64)
    }

    // -- assignability & helpers --------------------------------------------

    /// Is a value of type `found` usable where `expected` is required?
    fn assignable(&self, found: Ty, expected: Ty) -> bool {
        if found == expected
            || self.tcx.is_error(found)
            || self.tcx.is_error(expected)
            || self.tcx.is_never(found)
        {
            return true;
        }
        // Implicit widening to `dynamic` from any type (docs/12 §5).
        if matches!(self.tcx.kind(expected), TyKind::Dynamic) {
            return true;
        }
        // Implicit widening to an interface object: `found` implements the
        // interface `expected` (docs/11 §5 — interface used as a value type).
        if self.implements_dyn(found, expected) {
            return true;
        }
        // Union widening: every variant of `found` is a variant of `expected`.
        self.tcx.is_union_subtype(found, expected)
    }

    /// Whether `ty` is an interface (object) type.
    fn is_interface(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty),
            TyKind::Named { def, .. } if self.prog.def(*def).kind == DefKind::Interface)
    }

    /// Whether `expected` is an interface type that `found` (a nominal type)
    /// implements via a visible `extend` block.
    fn implements_dyn(&self, found: Ty, expected: Ty) -> bool {
        let TyKind::Named { def: idef, .. } = self.tcx.kind(expected) else { return false };
        if self.prog.def(*idef).kind != DefKind::Interface {
            return false;
        }
        let TyKind::Named { def: cdef, .. } = self.tcx.kind(found) else { return false };
        self.results.iface_impls.contains_key(&(*cdef, *idef))
    }

    fn expect(&mut self, found: Ty, expected: Ty, span: Span) {
        if !self.assignable(found, expected) {
            let e = self.display(expected);
            let f = self.display(found);
            self.emit(span, SemaErrorKind::TypeMismatch { expected: e, found: f });
            return;
        }
        // Record an implicit widening so codegen can box the value. Widening
        // into a union or `dynamic` from a narrower type needs a runtime box;
        // an already-union/already-equal value needs none.
        if found != expected
            && !self.tcx.is_error(found)
            && !self.tcx.is_never(found)
            && matches!(self.tcx.kind(expected), TyKind::Union(_) | TyKind::Dynamic)
        {
            self.results.adjustments.insert(span, Adjust::Widen(expected));
        }
        // Coerce a concrete value to an interface object (build its vtable box).
        if found != expected
            && !self.tcx.is_error(found)
            && !self.tcx.is_never(found)
            && self.implements_dyn(found, expected)
        {
            self.results.adjustments.insert(span, Adjust::WidenDyn(expected));
        }
    }

    fn is_numeric(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), TyKind::Int(_) | TyKind::Float(_))
    }
    /// Types the runtime can stringify today (the `as str` set, plus `str`).
    fn is_stringifiable(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                | TyKind::Str | TyKind::Null
        )
    }
    fn is_integer(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), TyKind::Int(_))
    }
    /// Types with a built-in ordering (`docs/02` §7): numerics, `char`, `str`.
    fn is_ordered(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Char | TyKind::Str
        )
    }

    fn op_error(&mut self, op: &'static str, ty: Ty, span: Span) -> Ty {
        let t = self.display(ty);
        self.emit(span, SemaErrorKind::InvalidOperator { op, ty: t });
        self.tcx.error
    }

    // -- scopes --------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    /// Introduce a binding in the innermost scope, returning its fresh id.
    /// `span` is the binding occurrence (the name in a param or pattern), so a
    /// resolution is recorded there for the code generator to map id → slot.
    fn bind(&mut self, name: &str, span: Span, ty: Ty) -> LocalId {
        let id = LocalId(self.next_local);
        self.next_local += 1;
        self.results.local_types.insert(id, ty);
        self.results.resolutions.insert(span, ValueRes::Local(id));
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), (ty, id));
        }
        id
    }
    fn lookup(&self, name: &str) -> Option<(Ty, LocalId)> {
        for scope in self.scopes.iter().rev() {
            if let Some(&entry) = scope.get(name) {
                return Some(entry);
            }
        }
        None
    }

    fn bind_pattern(&mut self, pattern: &Pattern, ty: Ty) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                self.bind(&name.name, name.span, ty);
            }
            PatternKind::Wildcard => {}
            PatternKind::Tuple { elems, rest: None } => {
                if let TyKind::Tuple(ts) = self.tcx.kind(ty).clone() {
                    if ts.len() == elems.len() {
                        for (p, t) in elems.iter().zip(ts) {
                            self.bind_pattern(p, t);
                        }
                        return;
                    }
                }
                // Shape mismatch: bind names to error to keep going.
                for p in elems {
                    self.bind_pattern(p, self.tcx.error);
                }
            }
            _ => {
                // Other irrefutable patterns (record destructuring, etc.)
                // land with struct support.
                self.emit(pattern.span, SemaErrorKind::Message(
                    "this binding pattern is not yet supported".into(),
                ));
            }
        }
    }

    // -- plumbing ------------------------------------------------------------

    fn current_module(&self) -> ModId {
        self.cur_module
    }

    fn local_env(&mut self) -> TypeEnv {
        let mut env = TypeEnv::new(self.current_module());
        env.generics = self.cur_generics.clone();
        env.self_ty = self.cur_self_ty;
        env
    }

    fn lower_ty(&mut self, ty: &Type, env: &TypeEnv) -> Ty {
        let mut lw = Lowerer::new(self.prog, &mut *self.tcx, &mut *self.errors);
        lw.lower(ty, env)
    }

    fn display(&self, ty: Ty) -> String {
        let prog = self.prog;
        self.tcx.display(ty, &|d: DefId| prog.def(d).name.clone())
    }

    fn emit(&mut self, span: Span, kind: SemaErrorKind) {
        self.errors.push(SemaError::new(kind, span));
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

#[cfg(test)]
mod tests {
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
        // `var x: i64 | str = 1` widens the i64 literal — an adjustment is
        // recorded so codegen boxes it.
        let src = "function f() { var x: i64 | str = 1; }";
        let (tokens, _) = lex(src, FileId(0));
        let (module, _) = parse(src, &tokens);
        let prog = Program::collect(&module);
        let mut tcx = TyCtxt::new();
        let mut errors = Vec::new();
        let mut ck = Checker::new(&prog, &mut tcx, &mut errors);
        ck.check_program();
        assert!(!ck.results.adjustments.is_empty(), "expected a widening adjustment");
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
}
