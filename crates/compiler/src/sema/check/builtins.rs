//! Type checker: builtin `List`/`Map`/`str`/channel/Shared/thread/async (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- builtin List<T> -----------------------------------------------------

    /// If `ty` is `List<E>`, return `E`.
    pub(crate) fn list_elem(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.list_def && args.len() == 1 => {
                Some(args[0])
            }
            _ => None,
        }
    }

    pub(crate) fn mk_list(&mut self, elem: Ty) -> Ty {
        let def = self.prog.list_def;
        self.tcx.mk_named(def, vec![elem])
    }

    pub(crate) fn check_list_lit(&mut self, elems: &[Expr], expected: Option<Ty>, span: Span) -> Ty {
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

    pub(crate) fn map_kv(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.map_def && args.len() == 2 => {
                Some((args[0], args[1]))
            }
            _ => None,
        }
    }

    pub(crate) fn mk_map(&mut self, k: Ty, v: Ty) -> Ty {
        let def = self.prog.map_def;
        self.tcx.mk_named(def, vec![k, v])
    }

    /// A map key must be hashable/comparable. For now that means `str` or any
    /// integer type (matching the runtime's two hashing strategies).
    pub(crate) fn is_valid_map_key(&self, ty: Ty) -> bool {
        ty == self.tcx.str || matches!(self.tcx.kind(ty), TyKind::Int(_))
    }

    pub(crate) fn check_map_lit(&mut self, items: &[MapItem], expected: Option<Ty>, span: Span) -> Ty {
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
    pub(crate) fn check_map_method(&mut self, kt: Ty, vt: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
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

    pub(crate) fn check_index(&mut self, receiver: &Expr, index: &Expr) -> Ty {
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
    pub(crate) fn check_list_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
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
    pub(crate) fn check_str_method(&mut self, name: &Ident, args: &[Expr], span: Span) -> Ty {
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
    pub(crate) fn is_immutable_value(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        )
    }

    /// Whether `ty` is safe to capture into a spawned thread by value: an
    /// immutable value, or a thread-safe channel endpoint (`Sender`/`Receiver`,
    /// whose struct just carries a synchronized channel's id) (`docs/20`).
    pub(crate) fn is_thread_shareable(&self, ty: Ty) -> bool {
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
    pub(crate) fn check_builtin_clone(&mut self, rty: Ty, callee_span: Span, name_span: Span) -> Option<Ty> {
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
    pub(crate) fn check_thread_spawn(&mut self, args: &[Expr], trailing: Option<&Expr>, span: Span) -> Ty {
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

    /// `JoinHandle<R>.join(): Future<Joined<R> | Panicked>` and
    /// `.detach(): null` (`docs/20` §1).
    ///
    /// `join` is **async and non-blocking** (`docs/21`): you `await` the
    /// returned future (or drive it with `block_on`) instead of parking the
    /// calling OS thread. The future resolves when the worker finishes.
    pub(crate) fn check_join_handle_method(&mut self, r: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
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
                let union = self.tcx.mk_union([joined, panicked]);
                self.tcx.mk_named(self.prog.future_def, vec![union])
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
    pub(crate) fn try_static_call(
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
    pub(crate) fn check_bound_static_call(
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
    pub(crate) fn check_type_static_call(
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
    pub(crate) fn check_channel_new(&mut self, generics: &[Type], args: &[Expr], span: Span) -> Ty {
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

    /// `Sender<T>` / `Receiver<T>` builtin methods (`docs/20` §2).
    pub(crate) fn check_channel_method(&mut self, def: DefId, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
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
                // Async + non-blocking (`docs/20` §2 / `docs/21`): `recv()` builds
                // a `Future<T>` you `await` (or drive with `block_on`) rather than
                // parking the calling thread.
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.tcx.mk_named(self.prog.future_def, vec![elem])
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
    pub(crate) fn check_shared_new(&mut self, args: &[Expr], span: Span) -> Ty {
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
    pub(crate) fn check_shared_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
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
    pub(crate) fn check_num_constant(&mut self, tyname: &str, name: &Ident, field_span: Span) -> Option<Ty> {
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
    pub(crate) fn check_num_method(&mut self, tyname: &str, name: &Ident, args: &[Expr], span: Span) -> Option<Ty> {
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
    pub(crate) fn check_num_args(&mut self, args: &[Expr], expect: &[Ty], span: Span) {
        if args.len() != expect.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
        }
        for (a, e) in args.iter().zip(expect) {
            let at = self.check_expr(a, Some(*e));
            self.expect(at, *e, a.span);
        }
    }

}
