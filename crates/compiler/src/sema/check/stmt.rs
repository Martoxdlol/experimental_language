//! Type checker: statements: `var`, assignment, lvalues (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- statements ----------------------------------------------------------

    pub(crate) fn check_block(&mut self, block: &Block, expected: Option<Ty>) -> Ty {
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

    pub(crate) fn check_stmt(&mut self, stmt: &Stmt) {
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
                // never runs. Calling an `async` function returns a `Future` —
                // `await` it, bind it with `var f = …`, or `var _ = …` to
                // explicitly discard. `await EXPR` is allowed at the statement
                // level even when its own type is a future.
                if self.is_future_ty(t) && !matches!(e.kind, ExprKind::Await { .. }) {
                    self.emit(
                        e.span,
                        SemaErrorKind::Message(
                            "this `Future` is created but never used — `await` it, \
                         `spawn` it, or bind it (e.g. `var _ = …`)"
                                .into(),
                        ),
                    );
                }
            }
            StmtKind::Item(_) => {
                // Nested item declarations are collected/checked separately.
            }
        }
    }

    /// Check an assignment target and return the type it holds. An lvalue is not
    /// routed through [`Checker::check_expr`], so build its HIR node here too
    /// (Stage 5) — otherwise an enclosing block could not be checker-emitted.
    pub(crate) fn check_lvalue(&mut self, target: &Expr) -> Ty {
        let ty = self.check_lvalue_inner(target);
        if let Some(node) = self.build_hir_node(target, ty) {
            self.node_hir.insert(target.span, node);
        }
        ty
    }

    fn check_lvalue_inner(&mut self, target: &Expr) -> Ty {
        match &target.kind {
            ExprKind::Ident(name) => match self.lookup(&name.name) {
                Some((ty, id)) => {
                    // An assignment to an enclosing-scope binding is itself a
                    // capture (`docs/09` §7): the closure mutates the outer's
                    // cell. Without this, `name = s` inside a closure body
                    // resolves to the outer local but never makes it into the
                    // closure's `captures` list — codegen would then have no
                    // env slot to write through.
                    self.record_capture(id, ty);
                    self.record_res(target.span, ValueRes::Local(id), ty);
                    ty
                }
                None => {
                    // Not a local: an `extern var` C global is an assignable
                    // place (`docs/19` §4). Resolve it as a module value.
                    let module = self.current_module();
                    if let Some(def) = self.prog.resolve_value_in(module, &name.name) {
                        if self.prog.def(def).kind == DefKind::ExternVar {
                            let t = self.value_def_ty(def);
                            self.record_res(target.span, ValueRes::Global(def), t);
                            return t;
                        }
                    }
                    self.emit(
                        target.span,
                        SemaErrorKind::UnknownValue {
                            name: name.name.clone(),
                        },
                    );
                    self.tcx.error
                }
            },
            ExprKind::Underscore => self.tcx.error, // discard; accepts anything
            // Field and tuple-index targets are checked as expressions; the
            // result type is what the assigned value must satisfy.
            ExprKind::Field { receiver, name } => self.check_field(receiver, name, target.span),
            ExprKind::TupleIndex {
                receiver,
                index,
                index_span,
            } => self.check_tuple_index(receiver, *index, *index_span),
            ExprKind::Index { receiver, index } => self.check_index(receiver, index),
            // `*p = v` — assign through a raw pointer (`docs/19` §2). The pointee
            // type is what the assigned value must satisfy.
            ExprKind::Deref {
                expr: inner,
                star_span,
            } => self.check_deref(inner, *star_span),
            _ => self.check_expr(target, None),
        }
    }
}
