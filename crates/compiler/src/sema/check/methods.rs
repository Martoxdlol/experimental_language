//! Type checker: method resolution, interfaces, bounds, dyn dispatch (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- methods -------------------------------------------------------------

    pub(crate) fn check_method_call(
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
    pub(crate) fn iterator_elem(&mut self, ity: Ty) -> Option<(Ty, DefId, Vec<Ty>, Ty, Ty)> {
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
    pub(crate) fn async_iterator_elem(&mut self, ity: Ty) -> Option<crate::sema::results::ForAsyncIter> {
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
    pub(crate) fn bound_ifaces(&mut self, param: DefId) -> Vec<(DefId, Vec<Ty>)> {
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
    pub(crate) fn ty_matches(&self, pat: Ty, val: Ty) -> bool {
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
    pub(crate) fn type_implements(&mut self, ty: Ty, iface: DefId) -> bool {
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
        // `Eq`/`Ord` are intrinsic for primitive scalars and `str` (`docs/15`):
        // they have built-in `==`/`<`. User types satisfy them through a derived
        // or hand-written `extend … : Eq`/`: Ord`, handled by the scan below.
        if iface == self.prog.eq_def && iface != DefId(0) && self.is_immutable_value(ty) {
            return true;
        }
        if iface == self.prog.ord_def && iface != DefId(0) && self.is_ordered(ty) {
            return true;
        }
        // `ToStr` is intrinsic for any directly-stringifiable value (primitives,
        // `str`, `null` — renderable via `as str`); user types via their impl.
        if iface == self.prog.to_str_def && iface != DefId(0) && self.is_stringifiable(ty) {
            return true;
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
    pub(crate) fn check_bounds(&mut self, gens: &[DefId], args: &[Ty], span: Span) {
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
    pub(crate) fn resolve_bound_method(&mut self, param: DefId, name: &str) -> Option<(DefId, DefId, Vec<Ty>)> {
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
    pub(crate) fn iface_method_sig(
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
    pub(crate) fn check_bound_method_call(
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
    pub(crate) fn check_dyn_method_call(
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
    /// Resolve a type's `to_str(self): str` method for string interpolation,
    /// returning the method def and the enclosing `extend`'s solved type
    /// arguments (empty for a non-generic `extend`) so the caller can record the
    /// monomorphization at the interpolation site.
    pub(crate) fn tostr_method(&mut self, ty: Ty) -> Option<(DefId, Vec<Ty>)> {
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
        if !(takes_only_self && ret == self.tcx.str) {
            return None;
        }
        // The extend's generic arguments, in declaration order, for codegen
        // monomorphization (`Box<i64>.to_str` vs `Box<str>.to_str`).
        let targs = self
            .prog
            .def(mdef)
            .parent
            .map(|parent| {
                self.prog
                    .def(parent)
                    .generics
                    .clone()
                    .iter()
                    .map(|g| subst.get(g).copied().unwrap_or(self.tcx.error))
                    .collect()
            })
            .unwrap_or_default();
        Some((mdef, targs))
    }

    pub(crate) fn resolve_method(&mut self, recv_ty: Ty, name: &str) -> Option<(DefId, HashMap<DefId, Ty>)> {
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
    pub(crate) fn check_generic_call(
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

    pub(crate) fn check_unary(&mut self, op: UnaryOp, operand: &Expr, op_span: Span) -> Ty {
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

    pub(crate) fn check_binary(
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
    pub(crate) fn try_operator_overload(
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
        // If the operator method lives in a *generic* `extend` (e.g. a derived
        // `eq`/`lt` on `Pair<A, B>`), record the extend's solved type arguments
        // so codegen monomorphizes the method to this operand's instantiation —
        // exactly as the general method-call path does.
        if let Some(parent) = self.prog.def(mdef).parent {
            let ext_gens = self.prog.def(parent).generics.clone();
            if !ext_gens.is_empty() {
                let targs: Vec<Ty> = ext_gens
                    .iter()
                    .map(|g| op_subst.get(g).copied().unwrap_or(self.tcx.error))
                    .collect();
                self.results.call_type_args.insert(op_span, targs);
            }
        }

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

    pub(crate) fn check_if(
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
    pub(crate) fn narrow_facts(&mut self, cond: &Expr) -> Option<(LocalId, Ty, Ty)> {
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

    pub(crate) fn restore_narrowing(&mut self, saved: Option<(LocalId, Option<Ty>)>) {
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

}
