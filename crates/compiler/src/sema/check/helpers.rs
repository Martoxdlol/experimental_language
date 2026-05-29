//! Type checker: literals, assignability, scopes, plumbing (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- literals ------------------------------------------------------------

    pub(crate) fn check_int_lit(&mut self, lit: &IntLit, expected: Option<Ty>, span: Span) -> Ty {
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
    pub(crate) fn peel_for_int(&self, expected: Ty) -> Option<Ty> {
        match self.tcx.kind(expected) {
            TyKind::Int(_) => Some(expected),
            _ => None,
        }
    }

    pub(crate) fn check_int_range(&mut self, lit: &IntLit, it: IntTy, span: Span) {
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

    pub(crate) fn check_float_lit(&mut self, lit: &FloatLit, expected: Option<Ty>) -> Ty {
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
    pub(crate) fn assignable(&self, found: Ty, expected: Ty) -> bool {
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
    pub(crate) fn is_interface(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty),
            TyKind::Named { def, .. } if self.prog.def(*def).kind == DefKind::Interface)
    }

    /// Whether `expected` is an interface type that `found` (a nominal type)
    /// implements via a visible `extend` block.
    pub(crate) fn implements_dyn(&self, found: Ty, expected: Ty) -> bool {
        let TyKind::Named { def: idef, .. } = self.tcx.kind(expected) else { return false };
        if self.prog.def(*idef).kind != DefKind::Interface {
            return false;
        }
        let TyKind::Named { def: cdef, .. } = self.tcx.kind(found) else { return false };
        self.hir.iface_impls.contains_key(&(*cdef, *idef))
    }

    pub(crate) fn expect(&mut self, found: Ty, expected: Ty, span: Span) {
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
            self.bake_coercion(span, Adjust::Widen(expected));
        }
        // Coerce a concrete value to an interface object (build its vtable box).
        if found != expected
            && !self.tcx.is_error(found)
            && !self.tcx.is_never(found)
            && self.implements_dyn(found, expected)
        {
            self.bake_coercion(span, Adjust::WidenDyn(expected));
        }
    }

    pub(crate) fn is_numeric(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), TyKind::Int(_) | TyKind::Float(_))
    }
    /// Types the runtime can stringify today (the `as str` set, plus `str`).
    pub(crate) fn is_stringifiable(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                | TyKind::Str | TyKind::Null
        )
    }
    pub(crate) fn is_integer(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), TyKind::Int(_))
    }
    /// Types with a built-in ordering (`docs/02` §7): numerics, `char`, `str`.
    pub(crate) fn is_ordered(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Char | TyKind::Str
        )
    }
    /// Types with an intrinsic `Hash` implementation (`docs/15` §7): primitives
    /// plus `str`. Floats are included for spec parity (NaN intentionally
    /// violates the `eq ⇒ hash` contract — the user's responsibility).
    pub(crate) fn is_hashable(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str
        )
    }

    pub(crate) fn op_error(&mut self, op: &'static str, ty: Ty, span: Span) -> Ty {
        let t = self.display(ty);
        self.emit(span, SemaErrorKind::InvalidOperator { op, ty: t });
        self.tcx.error
    }

    // -- scopes --------------------------------------------------------------

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    /// Introduce a binding in the innermost scope, returning its fresh id.
    /// `span` is the binding occurrence (the name in a param or pattern), so a
    /// resolution is recorded there for the code generator to map id → slot.
    pub(crate) fn bind(&mut self, name: &str, span: Span, ty: Ty) -> LocalId {
        let id = LocalId(self.next_local);
        self.next_local += 1;
        self.hir.local_types.insert(id, ty);
        self.hir.local_decls.insert(id, span);
        // Record the binding's resolution as a `Name(Local)` HIR node at its span
        // (was `resolutions.insert`); `resolution(span)`/`hir_local_at` read it.
        self.record_res(span, ValueRes::Local(id), ty);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), (ty, id));
        }
        id
    }
    pub(crate) fn lookup(&self, name: &str) -> Option<(Ty, LocalId)> {
        for scope in self.scopes.iter().rev() {
            if let Some(&entry) = scope.get(name) {
                return Some(entry);
            }
        }
        None
    }

    pub(crate) fn bind_pattern(&mut self, pattern: &Pattern, ty: Ty) {
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

    pub(crate) fn current_module(&self) -> ModId {
        self.cur_module
    }

    pub(crate) fn local_env(&mut self) -> TypeEnv {
        let mut env = TypeEnv::new(self.current_module());
        env.generics = self.cur_generics.clone();
        env.self_ty = self.cur_self_ty;
        env
    }

    pub(crate) fn lower_ty(&mut self, ty: &Type, env: &TypeEnv) -> Ty {
        let mut lw = Lowerer::new(self.prog, &mut *self.tcx, &mut *self.errors);
        lw.lower(ty, env)
    }

    pub(crate) fn display(&self, ty: Ty) -> String {
        let prog = self.prog;
        self.tcx.display(ty, &|d: DefId| prog.def(d).name.clone())
    }

    pub(crate) fn emit(&mut self, span: Span, kind: SemaErrorKind) {
        self.errors.push(SemaError::new(kind, span));
    }
}
