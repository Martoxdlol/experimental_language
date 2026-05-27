//! Type checker: `?` propagation and `match` exhaustiveness (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- `?` propagation -----------------------------------------------------

    /// `expr?` (`docs/13` §2): partition `expr`'s type against the enclosing
    /// return type `R`. Variants of `expr` that are also variants of `R` are
    /// *failures* (early-returned); the rest are *successes* and become the
    /// expression's value.
    pub(crate) fn check_try(&mut self, inner: &Expr, q_span: Span) -> Ty {
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
    pub(crate) fn find_residual_conversion(&mut self, e: Ty, r_vars: &[Ty]) -> Option<(DefId, Ty)> {
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
    pub(crate) fn extend_method(&self, ext: DefId, name: &str) -> Option<DefId> {
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

    pub(crate) fn check_match(
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
    pub(crate) fn check_pattern(&mut self, pattern: &Pattern, sty: Ty) {
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
    pub(crate) fn check_exhaustive(&mut self, sty: Ty, arms: &[MatchArm], span: Span) {
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
    pub(crate) fn is_irrefutable(&self, pattern: &Pattern, sty: Ty) -> bool {
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

}
