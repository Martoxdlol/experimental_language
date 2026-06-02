//! Type checker: function/method bodies and signatures (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- functions -----------------------------------------------------------

    /// Check a `test "name" { … }` body (`docs/23`): a zero-argument, unit-typed,
    /// non-async body. Emits a zero-param HIR signature and the body block so
    /// codegen compiles it as a callable function `otter_fusion test` runs.
    pub fn check_test(&mut self, def: DefId) {
        let Some(ItemKind::Test(t)) = self.prog.def(def).item.clone() else {
            return;
        };
        self.cur_module = self.prog.def(def).module;
        self.cur_generics.clear();
        self.cur_self_ty = None;
        self.in_async = false;
        self.ret_ty = self.tcx.null;
        self.scopes.clear();
        self.self_local = None;
        self.push_scope();
        self.hir.fn_sigs.insert(
            def,
            crate::hir::FnSig {
                params: Vec::new(),
                ret: self.tcx.null,
                async_output: None,
            },
        );
        // The body's trailing value (if any) is discarded — a test reports
        // pass/fail by completing vs. panicking, not by a return value.
        self.check_block(&t.body, None);
        let block = self.build_block(&t.body);
        self.fn_bodies.insert(def, block);
        self.pop_scope();
    }

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
        let mut async_output: Option<Ty> = None;
        let body_ret = if f.is_async {
            match self.future_output(ret_ty) {
                Some(out) => {
                    self.in_async = true;
                    async_output = Some(out);
                    out
                }
                None => {
                    self.emit(
                        f.return_type
                            .as_ref()
                            .map_or(self.prog.def(def).span, |t| t.span),
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
        self.scopes.clear();
        // `next_local` is NOT reset per function: local ids must be globally
        // unique because `results.local_types` is a program-wide map.
        self.self_local = None;
        self.push_scope();
        let mut param_sig: Vec<(LocalId, Ty)> = Vec::new();
        for p in &f.params {
            match &p.kind {
                ParamKind::SelfParam => {
                    // `self` binds to the receiver type; offset 0 in the params.
                    let sty = self_ty.unwrap_or(self.tcx.error);
                    let id = self.bind("self", p.span, sty);
                    self.self_local = Some(id);
                    param_sig.push((id, sty));
                }
                ParamKind::Normal { name, ty } => {
                    let pty = self.lower_ty(ty, &env);
                    let id = self.bind(&name.name, name.span, pty);
                    param_sig.push((id, pty));
                }
            }
        }
        // The checker emits the HIR signature directly (Stage 5): no separate
        // `fn_params`/`fn_return`/`async_fns` side tables.
        self.hir.fn_sigs.insert(
            def,
            crate::hir::FnSig {
                params: param_sig,
                ret: ret_ty,
                async_output,
            },
        );
        if let Some(body) = &f.body {
            let bty = self.check_block(body, Some(body_ret));
            // The body block's value is the function's result (the future's
            // `Output` for an async body).
            self.expect(bty, body_ret, body.span);
            // Stage 5: emit the whole body's HIR `Block` directly (every
            // expression was just checker-built into `node_hir`), so lowering
            // assembles the `Body` from it. `build_block` is total, so this is
            // populated for every function — `lower` keeps no fallback.
            let block = self.build_block(body);
            self.fn_bodies.insert(def, block);
        }
        self.in_async = prev_async;
        self.pop_scope();
    }

    /// Build the lowering env for a function-like def, plus its `self` type if
    /// it is a method (its parent is an `extend` block). The env carries the
    /// extend's generics and `Self`, then the method's own generics.
    pub(crate) fn fn_env(&mut self, def: DefId) -> (TypeEnv, Option<Ty>) {
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
    pub(crate) fn def_env(&mut self, def: DefId, self_ty: Option<Ty>) -> TypeEnv {
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
    pub(crate) fn subst_ty(&mut self, ty: Ty, map: &HashMap<DefId, Ty>) -> Ty {
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
            TyKind::Func {
                params,
                ret,
                is_extern,
            } => {
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
    pub(crate) fn unify(&mut self, pat: Ty, val: Ty, map: &mut HashMap<DefId, Ty>) {
        match (self.tcx.kind(pat).clone(), self.tcx.kind(val).clone()) {
            (TyKind::Param(d), _) => {
                map.entry(d).or_insert(val);
            }
            (TyKind::Union(pats), TyKind::Union(vals)) => {
                let mut remaining = vals;
                let mut params = Vec::new();
                for p in pats {
                    if matches!(self.tcx.kind(p), TyKind::Param(_)) {
                        params.push(p);
                        continue;
                    }
                    if let Some(idx) = remaining.iter().position(|&v| p == v) {
                        let v = remaining.remove(idx);
                        self.unify(p, v, map);
                    }
                }
                for p in params {
                    let TyKind::Param(def) = self.tcx.kind(p).clone() else {
                        continue;
                    };
                    if map.contains_key(&def) {
                        continue;
                    }
                    let bounds = self.bound_ifaces(def);
                    let idx = if bounds.is_empty() {
                        (!remaining.is_empty()).then_some(0)
                    } else {
                        remaining.iter().position(|&v| {
                            bounds
                                .iter()
                                .all(|(iface, _)| self.type_implements(v, *iface))
                        })
                    };
                    if let Some(idx) = idx {
                        map.insert(def, remaining.remove(idx));
                    }
                }
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
            (
                TyKind::Func {
                    params: pp,
                    ret: pr,
                    ..
                },
                TyKind::Func {
                    params: vp,
                    ret: vr,
                    ..
                },
            ) if pp.len() == vp.len() => {
                for (a, b) in pp.iter().zip(vp) {
                    self.unify(*a, b, map);
                }
                self.unify(pr, vr, map);
            }
            (TyKind::Ptr(p), TyKind::Ptr(v)) => self.unify(p, v, map),
            _ => {}
        }
    }
}
