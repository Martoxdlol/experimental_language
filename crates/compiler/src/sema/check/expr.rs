//! Type checker: expression checking, operators, `await`, closures (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- expressions ---------------------------------------------------------

    pub(crate) fn check_expr(&mut self, expr: &Expr, expected: Option<Ty>) -> Ty {
        let ty = self.check_expr_inner(expr, expected);
        self.results.expr_types.insert(expr.span, ty);
        ty
    }

    pub(crate) fn check_expr_inner(&mut self, expr: &Expr, expected: Option<Ty>) -> Ty {
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
                        if let Some((mdef, targs)) = self.tostr_method(pty) {
                            self.results.stringify_methods.insert(pspan, mdef);
                            if !targs.is_empty() {
                                self.results.call_type_args.insert(pspan, targs);
                            }
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
    pub(crate) fn record_capture(&mut self, id: LocalId, ty: Ty) {
        for frame in self.closure_stack.iter_mut() {
            if id.0 < frame.first_local && !frame.captures.iter().any(|(c, _)| *c == id) {
                frame.captures.push((id, ty));
            }
        }
    }

    /// Type-check a closure `(params) => body`. Parameter types come from
    /// annotations or, failing that, from the expected function type; the body
    /// is checked in a fresh scope and its free variables become captures.
    pub(crate) fn check_closure(
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
    pub(crate) fn check_await(&mut self, inner: &Expr, kw_span: Span) -> Ty {
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
    pub(crate) fn check_async_block(&mut self, block: &Block, expected: Option<Ty>, span: Span) -> Ty {
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
    pub(crate) fn future_output(&mut self, ty: Ty) -> Option<Ty> {
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
    pub(crate) fn is_future_ty(&self, ty: Ty) -> bool {
        if self.tcx.is_error(ty) {
            return false;
        }
        matches!(self.tcx.kind(ty), TyKind::Named { def, .. } if *def == self.prog.future_def)
    }

    pub(crate) fn check_ident(&mut self, name: &str, span: Span) -> Ty {
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

    pub(crate) fn builtin_ty(&mut self, b: Builtin) -> Ty {
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
    pub(crate) fn check_cast(&mut self, op: CastOp, inner: &Expr, target: &Type, cast_span: Span) -> Ty {
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
    pub(crate) fn cast_ok(&self, from: Ty, to: Ty) -> bool {
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
    pub(crate) fn value_res(&self, def: DefId) -> ValueRes {
        match self.prog.def(def).kind {
            DefKind::Function | DefKind::ExternFunction => ValueRes::Function(def),
            DefKind::ModuleVar | DefKind::ExternVar => ValueRes::Global(def),
            DefKind::Struct => ValueRes::StructCtor(def),
            _ => ValueRes::Global(def),
        }
    }

    /// The type of a module-level value definition referenced by name.
    pub(crate) fn value_def_ty(&mut self, def: DefId) -> Ty {
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
    pub(crate) fn function_value_ty(&mut self, def: DefId) -> Ty {
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
    pub(crate) fn try_builtin_ctor(
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

    pub(crate) fn check_call(
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
    pub(crate) fn check_args_against(
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
    pub(crate) fn check_namespaced_call(
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

}
