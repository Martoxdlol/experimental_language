//! Type checker: struct literals, fields, generic inference (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- structs -------------------------------------------------------------

    /// Build a type-lowering env binding a struct/alias def's generic params to
    /// the supplied arguments.
    pub(crate) fn subst_env(&mut self, def: DefId, args: &[Ty]) -> TypeEnv {
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
    pub(crate) fn record_fields(&mut self, def: DefId, args: &[Ty]) -> Option<Vec<(String, Ty)>> {
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
    pub(crate) fn tuple_fields(&mut self, def: DefId, args: &[Ty]) -> Option<Vec<Ty>> {
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
    pub(crate) fn infer_struct_args(
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

    pub(crate) fn check_struct_lit(
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

    pub(crate) fn check_tuple_ctor(&mut self, def: DefId, callee: &Expr, args: &[Expr], span: Span) -> Ty {
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

    pub(crate) fn check_field(&mut self, receiver: &Expr, name: &Ident, field_span: Span) -> Ty {
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

    pub(crate) fn check_tuple_index(&mut self, receiver: &Expr, index: u32, span: Span) -> Ty {
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

}
