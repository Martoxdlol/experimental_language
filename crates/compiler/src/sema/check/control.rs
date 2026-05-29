//! Type checker: `?` propagation and `match` exhaustiveness (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- `?` propagation -----------------------------------------------------

    /// `expr?` (`docs/13` §2): partition `expr`'s type against the enclosing
    /// return type `R`. Variants of `expr` that are also variants of `R` are
    /// *failures* (early-returned); the rest are *successes* and become the
    /// expression's value. When `expr`'s type is a non-union wrapper that
    /// implements `Try<Output, Residual>` (`docs/13` §3), the partition is
    /// performed against the union returned by `branch(self)`.
    pub(crate) fn check_try(&mut self, inner: &Expr, q_span: Span) -> Ty {
        let raw_et = self.check_expr(inner, None);
        if self.tcx.is_error(raw_et) {
            return self.tcx.error;
        }
        // `?` on a `*T | null` (NPO) raw pointer is not supported (it has no
        // tagged box to partition) — use an explicit null check (`docs/19` §2).
        if self.is_npo_union(raw_et) {
            self.emit(q_span, SemaErrorKind::Message(
                "`?` on a nullable pointer `*T | null` is not supported; \
                 check it explicitly with `if p is null { … }` (`docs/19` §2)"
                    .into(),
            ));
            return self.tcx.error;
        }
        // `?` classifies each variant of the operand into success / direct
        // failure / converted failure (via `FromResidual`).
        //   * **Union operand:** every variant is a *candidate failure* —
        //     variants in R are direct failures, variants outside R may still
        //     propagate via `FromResidual`, anything else stays a success
        //     (the spec's "the rest are the success value", `docs/13` §2).
        //   * **`Try<O, R>` operand:** `branch(self)` produces `O | R`; the
        //     `Output` variants are *always* successes, and the `Residual`
        //     variants are candidate failures (direct or converted).
        let r = self.ret_ty;
        let r_vars = self.tcx.variants(r);
        // The `Try` impl's `branch` shape, if the operand is a `Try` wrapper —
        // handed to the HIR `Try` node at the end (was the `try_branches` table).
        let mut try_branch: Option<TryBranch> = None;
        let (always_success, failure_candidates): (Vec<Ty>, Vec<Ty>) = if matches!(
            self.tcx.kind(raw_et),
            TyKind::Union(_) | TyKind::Dynamic
        ) {
            (Vec::new(), self.tcx.variants(raw_et))
        } else if let Some(tb) = self.find_try_impl(raw_et, q_span) {
            let s = self.tcx.variants(tb.output);
            let f = self.tcx.variants(tb.residual);
            try_branch = Some(tb);
            (s, f)
        } else {
            // No way to propagate from this type — emit a clear message
            // covering both the "wrong shape" and "no Try impl" cases.
            let nm = self.display(raw_et);
            self.emit(q_span, SemaErrorKind::Message(format!(
                "nothing to propagate here: `{nm}` is not a union and has no \
                 `Try` impl; remove the `?`"
            )));
            return self.tcx.error;
        };
        // Classify each candidate failure variant. For the union case a
        // variant outside R with no `FromResidual` impl stays a success (the
        // historical lenient behaviour, `docs/13` §2); for the Try case the
        // same situation is a hard error because Try's contract is that the
        // residual *always* propagates.
        let mut conversions: Vec<(Ty, DefId, Ty)> = Vec::new();
        let mut failures: Vec<Ty> = Vec::new();
        let mut successes: Vec<Ty> = always_success.clone();
        let is_try = try_branch.is_some();
        for v in failure_candidates {
            if r_vars.contains(&v) {
                failures.push(v);
            } else if let Some((method, target)) = self.find_residual_conversion(v, &r_vars) {
                conversions.push((v, method, target));
            } else if is_try {
                let vn = self.display(v);
                let rn = self.display(r);
                self.emit(q_span, SemaErrorKind::Message(format!(
                    "residual variant `{vn}` from `branch` cannot propagate \
                     into `{rn}`; add a `FromResidual<{vn}>` impl on a \
                     variant of the return type"
                )));
            } else {
                successes.push(v);
            }
        }
        if failures.is_empty() && conversions.is_empty() {
            self.emit(q_span, SemaErrorKind::Message(
                "nothing to propagate here; remove the `?`".into(),
            ));
            return raw_et;
        }
        if successes.is_empty() {
            self.emit(q_span, SemaErrorKind::Message(
                "this expression always returns; use a `match` instead".into(),
            ));
            return self.tcx.error;
        }
        // Hand `branch` shape + residual conversions to the HIR `Try` node
        // (consumed right after this returns); set after `inner` is fully
        // checked so nested `?` doesn't clobber the slots.
        let _ = q_span;
        self.pending_try_branch.set(try_branch);
        self.pending_residuals.set((!conversions.is_empty()).then_some(conversions));
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

    /// Look up a `Try<Output, Residual>` implementation for `operand_ty`
    /// (`docs/13` §3). Scans every `extend <gens> T: Try<O, R>` block whose
    /// target unifies with `operand_ty`, returning the `branch` method, the
    /// solved extend type-args (for codegen monomorphization), and the
    /// resulting `Output | Residual` union.
    pub(crate) fn find_try_impl(&mut self, operand_ty: Ty, _q_span: Span) -> Option<TryBranch> {
        let try_def = self.prog.try_def;
        if try_def == DefId(0) {
            return None;
        }
        for id in 0..self.prog.defs.len() {
            let ext = DefId(id as u32);
            if self.prog.def(ext).kind != DefKind::Extend {
                continue;
            }
            let Some(ItemKind::Extend(e_item)) = self.prog.def(ext).item.clone() else { continue };
            // Build a lowering env: each extend generic becomes a fresh `Param`.
            let mut env = TypeEnv::new(self.prog.def(ext).module);
            let ext_gens = self.prog.def(ext).generics.clone();
            for g in &ext_gens {
                let name = self.prog.def(*g).name.clone();
                env.generics.insert(name, self.tcx.mk_param(*g));
            }
            let target = self.lower_ty(&e_item.target, &env);
            // Solve the extend's generics from the receiver type.
            let mut subst: HashMap<DefId, Ty> = HashMap::new();
            self.unify(target, operand_ty, &mut subst);
            // Every extend generic must be solved (and the substituted target
            // must equal `operand_ty`) for this impl to apply.
            if ext_gens.iter().any(|g| subst.get(g).is_none()) {
                continue;
            }
            let solved_target = self.subst_ty(target, &subst);
            if solved_target != operand_ty {
                continue;
            }
            // Find the `Try` interface in this extend's clause list and extract
            // its `Output`/`Residual` type arguments.
            let mut try_iface_args: Option<(Ty, Ty)> = None;
            for itf in &e_item.interfaces {
                let TypeKind::Named { name, generics } = &itf.kind else { continue };
                if generics.len() != 2 {
                    continue;
                }
                let module = self.prog.def(ext).module;
                if self.prog.resolve_type_in(module, &name.name) != Some(try_def) {
                    continue;
                }
                let o_raw = self.lower_ty(&generics[0], &env);
                let r_raw = self.lower_ty(&generics[1], &env);
                let o = self.subst_ty(o_raw, &subst);
                let r = self.subst_ty(r_raw, &subst);
                try_iface_args = Some((o, r));
                break;
            }
            let Some((output, residual)) = try_iface_args else { continue };
            let Some(method) = self.extend_method(ext, "branch") else { continue };
            // Monomorphization arguments for `branch` (the extend's generics in
            // declaration order — the method takes no own generics in `Try`).
            let targs: Vec<Ty> = ext_gens
                .iter()
                .map(|g| subst.get(g).copied().unwrap_or(self.tcx.error))
                .collect();
            // The monomorphization args travel on `TryBranch.targs` (consumed by
            // codegen from the HIR `Try` node) — no separate span side table.
            let union_ty = self.tcx.mk_union([output, residual]);
            return Some(TryBranch { method, targs, union_ty, output, residual });
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
        // A `*T | null` (NPO) value is a raw nullable pointer, not a tagged box,
        // so `match` cannot dispatch on it yet — use `if p is *T { … }` /
        // `if p is null { … }` flow narrowing instead (`docs/19` §2).
        if self.is_npo_union(sty) {
            self.emit(span, SemaErrorKind::Message(
                "`match` on a nullable pointer `*T | null` is not yet supported; \
                 use `if p is null { … } else { … }` (`docs/19` §2)"
                    .into(),
            ));
        }
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
                // The matched variant type is recomputed where needed (the HIR
                // `Pattern` node and exhaustiveness) via `pattern_test_ty` — no
                // `pattern_types` side table.
                let env = self.local_env();
                let t = self.lower_ty(ty, &env);
                if let Some(b) = binding {
                    self.bind(&b.name, b.span, t);
                }
            }
            PatternKind::UnitPath(path) => {
                let module = self.current_module();
                if self.prog.resolve_type_in(module, &path.name.name).is_none() {
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
            // `Rect(w, h)` — a tuple-struct variant pattern. Resolve the matched
            // variant type (a union member with this struct's `def`, else the
            // struct itself) and bind the positional field sub-patterns.
            PatternKind::TupleStruct { path, fields, .. } => {
                let vt = self.struct_pattern_ty(path, sty);
                if let TyKind::Named { def, args } = self.tcx.kind(vt).clone() {
                    if let Some(fts) = self.tuple_fields(def, &args) {
                        for (i, p) in fields.iter().enumerate() {
                            self.check_pattern(p, fts.get(i).copied().unwrap_or(self.tcx.error));
                        }
                        return;
                    }
                }
                for p in fields {
                    self.check_pattern(p, self.tcx.error);
                }
            }
            // `Circle { radius }` — a record-struct variant pattern.
            PatternKind::RecordStruct { path, fields, .. } => {
                let vt = self.struct_pattern_ty(path, sty);
                let rfs = match self.tcx.kind(vt).clone() {
                    TyKind::Named { def, args } => self.record_fields(def, &args),
                    _ => None,
                };
                for fp in fields {
                    let fty = rfs
                        .as_ref()
                        .and_then(|fs| fs.iter().find(|(n, _)| *n == fp.name.name))
                        .map(|(_, t)| *t)
                        .unwrap_or(self.tcx.error);
                    match &fp.pattern {
                        Some(sub) => self.check_pattern(sub, fty),
                        None => {
                            self.bind(&fp.name.name, fp.name.span, fty);
                        }
                    }
                }
            }
            // `A | B | C` — an or-pattern matches if any alternative does. The
            // alternatives must not bind variables (codegen can't know which one
            // matched), so each is restricted to a non-binding pattern.
            PatternKind::Or(alts) => {
                for alt in alts {
                    if self.pattern_binds(alt) {
                        self.emit(alt.span, SemaErrorKind::Message(
                            "an alternative in an `|` or-pattern may not bind variables".into(),
                        ));
                    }
                    self.check_pattern(alt, sty);
                }
            }
            // `[a, b]` / `[head, ..tail]` — list patterns (refutable: a length
            // test). Bind the fixed elements (and the rest as a `List<T>`).
            PatternKind::List { elems, rest } => {
                let elem = self.list_elem(sty).unwrap_or_else(|| {
                    if !self.tcx.is_error(sty) {
                        self.emit(pattern.span, SemaErrorKind::Message(format!(
                            "list pattern on non-list type `{}`", self.display(sty)
                        )));
                    }
                    self.tcx.error
                });
                for p in elems {
                    self.check_pattern(p, elem);
                }
                if let Some((_, r)) = rest {
                    if let Some(name) = &r.name {
                        let list_ty = self.mk_list(elem);
                        self.bind(&name.name, name.span, list_ty);
                    }
                }
            }
            _ => self.emit(pattern.span, SemaErrorKind::Message(
                "this pattern is not yet supported".into(),
            )),
        }
    }

    /// Accumulate the union-variant types a pattern covers (for exhaustiveness),
    /// flattening or-patterns and recognizing the `null` literal.
    fn collect_covered(&mut self, pattern: &Pattern, covered: &mut Vec<Ty>) {
        match &pattern.kind {
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.collect_covered(alt, covered);
                }
            }
            PatternKind::Literal(e) if matches!(e.kind, ExprKind::Null) => {
                covered.push(self.tcx.null);
            }
            _ => {
                if let Some(t) = self.pattern_test_ty(pattern) {
                    covered.push(t);
                }
            }
        }
    }

    /// Whether `pattern` binds any variable (used to reject binding alternatives
    /// inside an or-pattern, where codegen can't tell which alternative matched).
    pub(crate) fn pattern_binds(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Binding(_) => true,
            PatternKind::TypeBinding { binding, .. } => binding.is_some(),
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::UnitPath(_) => false,
            PatternKind::Tuple { elems, .. } | PatternKind::List { elems, .. } => {
                elems.iter().any(|p| self.pattern_binds(p))
            }
            PatternKind::TupleStruct { fields, .. } => fields.iter().any(|p| self.pattern_binds(p)),
            PatternKind::RecordStruct { fields, .. } => fields
                .iter()
                .any(|fp| fp.pattern.as_ref().map_or(true, |p| self.pattern_binds(p))),
            PatternKind::Or(alts) => alts.iter().any(|p| self.pattern_binds(p)),
        }
    }

    /// The matched variant type of a struct pattern (`Path(..)` / `Path { .. }`)
    /// given the scrutinee type `sty`: the union member sharing the path's
    /// struct `def` (carrying its concrete generic args), else the bare struct.
    pub(crate) fn struct_pattern_ty(&mut self, path: &TypePath, sty: Ty) -> Ty {
        let module = self.current_module();
        let Some(def) = self.prog.resolve_type_in(module, &path.name.name) else {
            self.emit(path.span, SemaErrorKind::UnknownType { name: path.name.name.clone() });
            return self.tcx.error;
        };
        for v in self.tcx.variants(sty) {
            if let TyKind::Named { def: vd, .. } = self.tcx.kind(v) {
                if *vd == def {
                    return v;
                }
            }
        }
        if let TyKind::Named { def: sd, .. } = self.tcx.kind(sty) {
            if *sd == def {
                return sty;
            }
        }
        self.tcx.mk_named(def, Vec::new())
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
                self.collect_covered(&a.pattern, &mut covered);
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

    /// The matched-variant type of a `TypeBinding`/`UnitPath` pattern (was the
    /// `pattern_types` side table), recomputed on demand for exhaustiveness and
    /// the HIR `Pattern` node. `None` for other pattern kinds.
    pub(crate) fn pattern_test_ty(&mut self, pattern: &Pattern) -> Option<Ty> {
        match &pattern.kind {
            PatternKind::TypeBinding { ty, .. } => {
                let env = self.local_env();
                Some(self.lower_ty(ty, &env))
            }
            PatternKind::UnitPath(path)
            | PatternKind::TupleStruct { path, .. }
            | PatternKind::RecordStruct { path, .. } => {
                let module = self.current_module();
                let def = self.prog.resolve_type_in(module, &path.name.name)?;
                Some(self.tcx.mk_named(def, Vec::new()))
            }
            _ => None,
        }
    }

    /// Does this pattern match every value of `sty` (covering the scrutinee)?
    pub(crate) fn is_irrefutable(&mut self, pattern: &Pattern, sty: Ty) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => true,
            PatternKind::Tuple { elems, rest: None } => {
                match self.tcx.kind(sty).clone() {
                    TyKind::Tuple(ets) if ets.len() == elems.len() => {
                        elems.iter().zip(ets).all(|(p, et)| self.is_irrefutable(p, et))
                    }
                    _ => false,
                }
            }
            PatternKind::TypeBinding { .. } => {
                // `T x` covers the scrutinee only when `T` is exactly its type
                // (i.e. a non-union match on its own type).
                !matches!(self.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic)
                    && self.pattern_test_ty(pattern) == Some(sty)
            }
            // A lone `[..]` / `[..rest]` (no fixed elements) matches any list, so
            // it is irrefutable; any fixed-length list pattern is a length test
            // and needs a catch-all arm.
            PatternKind::List { elems, rest } => elems.is_empty() && rest.is_some(),
            // An or-pattern is irrefutable if any alternative is.
            PatternKind::Or(alts) => alts.iter().any(|p| self.is_irrefutable(p, sty)),
            // A struct pattern covers the scrutinee only when it is exactly that
            // single (non-union) struct type and its fields are all irrefutable.
            PatternKind::TupleStruct { path, fields, rest: None } => {
                if matches!(self.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic) {
                    return false;
                }
                let vt = self.struct_pattern_ty(path, sty);
                if vt != sty {
                    return false;
                }
                match self.tcx.kind(sty).clone() {
                    TyKind::Named { def, args } => match self.tuple_fields(def, &args) {
                        Some(fts) => fields
                            .iter()
                            .enumerate()
                            .all(|(i, p)| self.is_irrefutable(p, fts.get(i).copied().unwrap_or(self.tcx.error))),
                        None => false,
                    },
                    _ => false,
                }
            }
            PatternKind::RecordStruct { path, fields, has_rest: _ } => {
                if matches!(self.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic) {
                    return false;
                }
                let vt = self.struct_pattern_ty(path, sty);
                if vt != sty {
                    return false;
                }
                let rfs = match self.tcx.kind(sty).clone() {
                    TyKind::Named { def, args } => self.record_fields(def, &args),
                    _ => None,
                };
                fields.iter().all(|fp| match &fp.pattern {
                    Some(sub) => {
                        let fty = rfs
                            .as_ref()
                            .and_then(|fs| fs.iter().find(|(n, _)| *n == fp.name.name))
                            .map(|(_, t)| *t)
                            .unwrap_or(self.tcx.error);
                        self.is_irrefutable(sub, fty)
                    }
                    None => true, // a bare field binding is irrefutable
                })
            }
            _ => false,
        }
    }

}
