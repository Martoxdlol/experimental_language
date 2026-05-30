//! Type checker: expression checking, operators, `await`, closures (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- expressions ---------------------------------------------------------

    pub(crate) fn check_expr(&mut self, expr: &Expr, expected: Option<Ty>) -> Ty {
        let ty = self.check_expr_inner(expr, expected);
        // The checker builds the typed HIR node for every expression as it checks
        // it; its `.ty` (or its inner type, under a baked `Adjust`) is the checked
        // type that `results.expr_ty` reports — no separate `expr_types` table.
        if let Some(node) = self.build_hir_node(expr, ty) {
            self.node_hir.insert(expr.span, node);
        }
        ty
    }

    /// Construct the [`crate::hir::Expr`] for `expr` directly during checking,
    /// for the node kinds that have been migrated off the span side tables.
    /// Returns `None` for kinds still lowered from tables (HIR lowering falls
    /// back to its table-driven construction for those). The node carries its
    /// *raw* (pre-coercion) type — the `Adjust` wrapper is applied by lowering.
    pub(crate) fn build_hir_node(&mut self, expr: &Expr, ty: Ty) -> Option<crate::hir::Expr> {
        use crate::hir::{self, ExprKind as H};
        let kind = match &expr.kind {
            ExprKind::Int(lit) => H::Int(crate::hir::parse_int_lit(lit)),
            ExprKind::Float(lit) => H::Float(crate::hir::parse_float_lit(lit)),
            ExprKind::Bool(b) => H::Bool(*b),
            ExprKind::Null => H::Null,
            ExprKind::Char(c) => H::Char(crate::hir::parse_char_lit(&c.raw).unwrap_or(0)),
            // A resolved value name (`self` resolves to its local too); a
            // narrowed read bakes in its `Unbox` via `build_name_node`.
            ExprKind::Ident(_) | ExprKind::SelfExpr => {
                let res = self.resolution(expr.span)?;
                return Some(self.build_name_node(res, ty, expr.span));
            }
            ExprKind::Underscore => H::Discard,
            // Parentheses are transparent in the HIR; mirror the inner node at
            // this span so `expr_ty` (which reads `node_hir`) resolves either span.
            ExprKind::Paren(inner) => return self.hir_child(inner),
            // Composites: buildable once every child's HIR node exists.
            ExprKind::Tuple(elems) => {
                H::Tuple(elems.iter().map(|e| self.hir_child(e)).collect::<Option<_>>()?)
            }
            ExprKind::List(elems) => {
                H::List(elems.iter().map(|e| self.hir_child(e)).collect::<Option<_>>()?)
            }
            ExprKind::Unary { op, operand, .. } => H::Unary {
                op: crate::hir::lower_unop(*op),
                operand: Box::new(self.hir_child(operand)?),
                overload: self.hir_overload(),
            },
            ExprKind::Binary { op, left, right, .. } => H::Binary {
                op: crate::hir::lower_binop(*op),
                left: Box::new(self.hir_child(left)?),
                right: Box::new(self.hir_child(right)?),
                overload: self.hir_overload(),
            },
            ExprKind::Cast { op, expr: inner, .. } => H::Cast {
                op: crate::hir::lower_castop(*op),
                expr: Box::new(self.hir_child(inner)?),
                // `check_cast` stashed the resolved target; otherwise the node's
                // own type is the target.
                target: self.pending_cast_target.take().unwrap_or(ty),
            },
            ExprKind::Ref { expr: inner, .. } => H::Ref(Box::new(self.hir_child(inner)?)),
            ExprKind::Deref { expr: inner, .. } => H::Deref(Box::new(self.hir_child(inner)?)),
            ExprKind::TupleIndex { receiver, index, .. } => H::TupleIndex {
                receiver: Box::new(self.hir_child(receiver)?),
                index: *index,
            },
            ExprKind::Index { receiver, index } => H::Index {
                receiver: Box::new(self.hir_child(receiver)?),
                index: Box::new(self.hir_child(index)?),
            },
            ExprKind::Try { expr: inner, .. } => H::Try {
                expr: Box::new(self.hir_child(inner)?),
                branch: self.pending_try_branch.take(),
                residual_conversions: self.pending_residuals.take().unwrap_or_default(),
            },
            ExprKind::Await { expr: inner, .. } => H::Await {
                expr: Box::new(self.hir_child(inner)?),
                output: self.pending_await.take()?,
            },
            ExprKind::Spawn { expr: inner, .. } => H::Spawn {
                expr: Box::new(self.hir_child(inner)?),
                output: self.pending_spawn.take()?,
            },
            ExprKind::Field { receiver, name } => {
                // A numeric-namespace constant (`i32.MAX`, `f64.NAN`) is an
                // intrinsic, not a field access — recognized via the shared
                // `num_constant_of` recognizer (was the `num_intrinsics` table).
                if let ExprKind::Ident(recv) = &receiver.kind {
                    if self.resolution(receiver.span).is_none() {
                        if let Some(intr) =
                            crate::sema::results::num_constant_of(self.tcx, &recv.name, &name.name)
                        {
                            return Some(hir::Expr {
                                kind: H::Intrinsic {
                                    intrinsic: crate::hir::Intrinsic::Num(intr),
                                    args: vec![],
                                },
                                ty,
                                span: expr.span,
                            });
                        }
                    }
                }
                let recv_ty = self.expr_ty(receiver.span)?;
                let field = self.hir_field_ref(recv_ty, &name.name);
                H::Field { receiver: Box::new(self.hir_child(receiver)?), field }
            }
            ExprKind::Call { .. } => self.build_call_kind(expr, ty)?,
            ExprKind::Continue => H::Continue,
            ExprKind::Return(v) => H::Return(match v {
                Some(e) => Some(Box::new(self.hir_child(e)?)),
                None => None,
            }),
            ExprKind::Break(v) => H::Break(match v {
                Some(e) => Some(Box::new(self.hir_child(e)?)),
                None => None,
            }),
            ExprKind::If { cond, then_block, else_branch } => H::If {
                cond: Box::new(self.hir_child(cond)?),
                then_block: self.build_block(then_block),
                else_branch: self.build_else(else_branch.as_ref()),
            },
            ExprKind::Match { scrutinee, arms } => {
                let mut hir_arms = Vec::with_capacity(arms.len());
                for a in arms {
                    hir_arms.push(crate::hir::MatchArm {
                        pattern: self.build_pattern(&a.pattern),
                        guard: match &a.guard {
                            Some(g) => Some(self.hir_child(g)?),
                            None => None,
                        },
                        body: self.hir_child(&a.body)?,
                        span: a.span,
                    });
                }
                H::Match { scrutinee: Box::new(self.hir_child(scrutinee)?), arms: hir_arms }
            }
            ExprKind::Block(b) => H::Block(self.build_block(b)),
            ExprKind::Loop(b) => H::Loop(self.build_block(b)),
            ExprKind::While { cond, body } => H::While {
                cond: Box::new(self.hir_child(cond)?),
                body: self.build_block(body),
            },
            ExprKind::For { pattern, in_async, iter, body } => H::For {
                pattern: self.build_pattern(pattern),
                iter: Box::new(self.hir_child(iter)?),
                body: self.build_block(body),
                driver: self.build_for_driver(iter, *in_async),
                in_async: *in_async,
            },
            ExprKind::Closure { is_async, body, .. } => {
                // The closure body must be built first (so its nested closures
                // consume their own slots) before we take *this* closure's info.
                let body = Box::new(self.hir_child(body)?);
                match self.pending_closure.take() {
                    Some(info) => H::Closure {
                        params: info.params,
                        captures: info.captures,
                        ret: info.ret,
                        is_async: *is_async,
                        body,
                    },
                    None => H::Error,
                }
            }
            ExprKind::AsyncBlock(block) => {
                let body = self.build_block(block);
                match self.pending_async.take() {
                    Some(info) => H::AsyncBlock {
                        output: info.output,
                        params: info.params,
                        captures: info.captures,
                        body,
                    },
                    None => H::Error,
                }
            }
            // An anonymous `function(){…}` expression is not lowered by codegen
            // today (it falls through to `Error`), mirroring `lower_expr_kind`.
            ExprKind::AnonFn(_) => H::Error,
            ExprKind::Str(s) => H::Str(self.hir_str_parts(s)?),
            ExprKind::MapLit(items) => H::Map(self.hir_map_entries(items)?),
            ExprKind::StructLit { fields, spread, .. } => {
                let (def, type_args) = match self.tcx.kind(ty) {
                    crate::ty::TyKind::Named { def, args } => (*def, args.clone()),
                    _ => (crate::ids::DefId(0), Vec::new()),
                };
                let mut hir_fields = Vec::with_capacity(fields.len());
                for fi in fields {
                    let value = match &fi.value {
                        Some(e) => self.hir_child(e)?,
                        // Field-init shorthand `Foo { x }` — `check_struct_lit`
                        // already built the (narrowed/widened) `Name` node for the
                        // local at the field name's span.
                        None => self.node_hir.get(&fi.name.span).cloned()?,
                    };
                    hir_fields.push(crate::hir::FieldInit {
                        index: self.hir_field_index(def, &fi.name.name),
                        name: fi.name.name.clone(),
                        value,
                        span: fi.span,
                    });
                }
                let spread = match spread {
                    Some(e) => Some(Box::new(self.hir_child(e)?)),
                    None => None,
                };
                H::Struct { def, type_args, fields: hir_fields, spread }
            }
        };
        Some(hir::Expr { kind, ty, span: expr.span })
    }

    /// The already-built HIR node for a child expression (skipping `Paren`
    /// grouping, which the HIR drops), or `None` if that child kind has not yet
    /// been migrated — in which case the enclosing node can't be checker-built
    /// and falls back to table-driven lowering.
    fn hir_child(&self, mut e: &Expr) -> Option<crate::hir::Expr> {
        while let ExprKind::Paren(inner) = &e.kind {
            e = inner;
        }
        // The stored node already carries its coercion: a narrowing `Unbox` baked
        // by `build_name_node`, or a widening baked in place by `expect`.
        self.node_hir.get(&e.span).cloned()
    }

    /// Like [`hir_child`] but total: an expression the checker could not build
    /// (only possible in an already-erroring program) degrades to an `Error`
    /// node, so body construction never bails. This is what lets the checker
    /// emit a `Block` for every function — including malformed ones — and is why
    /// the old table-driven `lower` recovery path is gone.
    fn hir_child_or_error(&self, mut e: &Expr) -> crate::hir::Expr {
        while let ExprKind::Paren(inner) = &e.kind {
            e = inner;
        }
        self.node_hir.get(&e.span).cloned().unwrap_or_else(|| crate::hir::Expr {
            kind: crate::hir::ExprKind::Error,
            ty: self.expr_ty(e.span).unwrap_or(self.tcx.error),
            span: e.span,
        })
    }

    /// Build a value-name HIR node, baking in a flow-narrowing `Unbox` coercion
    /// when the use is a narrowed read of a boxed local (was the narrowing write
    /// into the `adjustments` table). The narrowing is *recovered structurally*:
    /// a `Local` whose declared (wide) type is a boxed union / `dynamic` /
    /// interface but whose use type `ty` here is a single concrete variant must
    /// unbox at the use — exactly the `was_boxed && now_single` test `check_ident`
    /// applied. No side table is consulted.
    /// Record a value-name / callee-dispatch / binding resolution at `span` by
    /// storing its `Name` HIR node in `node_hir` (was `resolutions.insert`).
    /// `results.resolution(span)` reads it back. The node also serves as the
    /// expression's HIR node for a value-name use.
    pub(crate) fn record_res(
        &mut self,
        span: Span,
        res: crate::sema::results::ValueRes,
        ty: Ty,
    ) {
        let node = self.build_name_node(res, ty, span);
        self.node_hir.insert(span, node);
    }

    pub(crate) fn build_name_node(
        &self,
        res: crate::sema::results::ValueRes,
        ty: Ty,
        span: Span,
    ) -> crate::hir::Expr {
        use crate::sema::results::{Adjust, ValueRes};
        let name = crate::hir::Expr { kind: crate::hir::ExprKind::Name(res), ty, span };
        if let ValueRes::Local(id) = res {
            if let Some(wide) = self.hir.local_ty(id) {
                let was_boxed = (matches!(self.tcx.kind(wide), TyKind::Union(_) | TyKind::Dynamic)
                    && !self.is_npo_union(wide))
                    || self.is_interface(wide);
                let now_single = !matches!(self.tcx.kind(ty), TyKind::Union(_) | TyKind::Dynamic)
                    && !self.is_interface(ty);
                if was_boxed && now_single {
                    return crate::hir::Expr {
                        kind: crate::hir::ExprKind::Adjust {
                            adjust: Adjust::Unbox(ty),
                            expr: Box::new(name),
                        },
                        ty,
                        span,
                    };
                }
            }
        }
        name
    }

    /// Bake a widening coercion (`Widen` / `WidenDyn`) onto the already-built HIR
    /// node at `span`, wrapping it in an `Adjust` (was the `adjustments` table).
    /// The coercion is recorded by the parent's `expect`, *after* the child node
    /// was built and stored, so the node exists. Any coercion previously baked at
    /// this span is replaced (wrapping the *raw* node), matching the old
    /// last-write-wins `HashMap`.
    pub(crate) fn bake_coercion(&mut self, span: Span, adjust: crate::sema::results::Adjust) {
        use crate::sema::results::Adjust;
        let Some(node) = self.node_hir.get(&span) else { return };
        let raw = match &node.kind {
            crate::hir::ExprKind::Adjust { expr, .. } => (**expr).clone(),
            _ => node.clone(),
        };
        let ty = match adjust {
            Adjust::Widen(t) | Adjust::Unbox(t) | Adjust::WidenDyn(t) => t,
        };
        self.node_hir.insert(
            span,
            crate::hir::Expr {
                kind: crate::hir::ExprKind::Adjust { adjust, expr: Box::new(raw) },
                ty,
                span,
            },
        );
    }

    /// Resolve a field access to its struct/index/name (mirrors
    /// `lower::field_ref`).
    fn hir_field_ref(&self, recv_ty: Ty, name: &str) -> crate::hir::FieldRef {
        let def = match self.tcx.kind(recv_ty) {
            crate::ty::TyKind::Named { def, .. } => *def,
            _ => crate::ids::DefId(0),
        };
        crate::hir::FieldRef {
            struct_def: def,
            index: self.hir_field_index(def, name),
            name: name.to_string(),
        }
    }

    /// The positional index of field `name` within struct `def` (mirrors
    /// `lower::field_index`).
    fn hir_field_index(&self, def: crate::ids::DefId, name: &str) -> u32 {
        use crate::sema::results::StructFields as SF;
        match self.hir.structs.get(&def) {
            Some(SF::Record(fs)) => fs.iter().position(|(n, _)| n == name).unwrap_or(0) as u32,
            Some(SF::Tuple(_)) => name.parse().unwrap_or(0),
            _ => name.parse().unwrap_or(0),
        }
    }

    /// Build the HIR string parts (mirrors `lower::lower_str_parts`); returns
    /// `None` if any interpolation hole's expression has not been migrated.
    fn hir_str_parts(&self, s: &StringLit) -> Option<Vec<crate::hir::StrPart>> {
        use crate::hir::StrPart;
        // The `(to_str method, targs)` per interpolation hole, in source order,
        // as the `Str` check arm stashed them. Pop one per `Interp` part.
        let mut holes = self.pending_stringify.take().unwrap_or_default();
        let mut out = Vec::with_capacity(s.parts.len());
        for p in &s.parts {
            out.push(match p {
                StringPart::Text { text, .. } => StrPart::Text(text.clone()),
                StringPart::Ident(id) => {
                    let res = self.resolution(id.span)?;
                    // `$x` may be flow-narrowed (a union unboxed to a single
                    // variant inside an `is` branch) — `build_name_node` bakes the
                    // `Unbox` in, just as a `${expr}` hole gets it via `hir_child`.
                    let expr = self.build_name_node(res, self.expr_ty(id.span)?, id.span);
                    let (stringify, stringify_targs) = holes.pop_front().unwrap_or((None, Vec::new()));
                    StrPart::Interp { expr: Box::new(expr), stringify, stringify_targs }
                }
                StringPart::Expr(e) => {
                    let expr = Box::new(self.hir_child(e)?);
                    let (stringify, stringify_targs) = holes.pop_front().unwrap_or((None, Vec::new()));
                    StrPart::Interp { expr, stringify, stringify_targs }
                }
            });
        }
        Some(out)
    }

    /// Build the HIR map entries (mirrors `lower::lower_map_items`); returns
    /// `None` if any key/value/spread sub-expression is not yet migrated.
    fn hir_map_entries(&self, items: &[MapItem]) -> Option<Vec<crate::hir::MapEntry>> {
        use crate::hir::MapEntry;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            out.push(match it {
                MapItem::Entry { key, value, .. } => MapEntry::Kv {
                    key: self.hir_child(key)?,
                    value: self.hir_child(value)?,
                },
                MapItem::Spread(base) => MapEntry::Spread(self.hir_child(base)?),
            });
        }
        Some(out)
    }

    /// Build a [`crate::hir::Block`] (the checker's emitted body block). Total —
    /// unbuildable sub-expressions degrade to `Error` via `hir_child_or_error`.
    pub(crate) fn build_block(&mut self, b: &Block) -> crate::hir::Block {
        let mut stmts = Vec::with_capacity(b.stmts.len());
        for s in &b.stmts {
            if let Some(stmt) = self.build_stmt(s) {
                stmts.push(stmt);
            }
        }
        let trailing = b.trailing.as_ref().map(|e| Box::new(self.hir_child_or_error(e)));
        let ty = b
            .trailing
            .as_ref()
            .map(|e| self.expr_ty(e.span).unwrap_or(self.tcx.error))
            .unwrap_or(self.tcx.null);
        crate::hir::Block { stmts, trailing, ty, span: b.span }
    }

    /// Build a statement. `None` only for a block-level item with no def (which
    /// has no runtime effect and is dropped from the block).
    fn build_stmt(&mut self, s: &Stmt) -> Option<crate::hir::Stmt> {
        use crate::hir::StmtKind as SK;
        let kind = match &s.kind {
            StmtKind::Var(lv) => SK::Let {
                pattern: self.build_pattern(&lv.pattern),
                init: self.hir_child_or_error(&lv.init),
            },
            StmtKind::Assign { target, value } => SK::Assign {
                target: self.hir_child_or_error(target),
                value: self.hir_child_or_error(value),
            },
            StmtKind::Expr(e) => SK::Expr(self.hir_child_or_error(e)),
            StmtKind::Item(item) => SK::Item(self.span_def(item.span)?),
        };
        Some(crate::hir::Stmt { kind, span: s.span })
    }

    /// The [`DefId`] of the item declared at `span` (mirrors lowering's
    /// `span2def` index), or `None` if none.
    fn span_def(&self, span: Span) -> Option<crate::ids::DefId> {
        self.prog
            .defs
            .iter()
            .position(|d| d.span == span)
            .map(|i| crate::ids::DefId(i as u32))
    }

    /// Build the `else` branch of an `if`. `None` = no `else`.
    fn build_else(&mut self, else_branch: Option<&ElseBranch>) -> Option<Box<crate::hir::Expr>> {
        match else_branch {
            None => None,
            Some(ElseBranch::Block(b)) => {
                let block = self.build_block(b);
                let ty = block.ty;
                Some(Box::new(crate::hir::Expr {
                    kind: crate::hir::ExprKind::Block(block),
                    ty,
                    span: b.span,
                }))
            }
            Some(ElseBranch::If(e)) => Some(Box::new(self.hir_child_or_error(e))),
        }
    }

    /// Build a pattern (total — sub-expressions/sub-patterns that the checker
    /// could not build degrade to `Error`/`Wildcard`).
    fn build_pattern(&mut self, p: &Pattern) -> crate::hir::Pattern {
        use crate::hir::PatternKind as PK;
        // The pattern's HIR type. Only `TypeBind`/`UnitPath` carry a meaningful
        // `test_ty` (the matched variant, used by codegen); other kinds' `.ty` is
        // informational. The matched type is recomputed here (was `pattern_types`)
        // — the function's generic env is still active at body-build time.
        let (kind, ty) = match &p.kind {
            PatternKind::Wildcard => (PK::Wildcard, self.tcx.error),
            PatternKind::Binding(id) => (PK::Bind(self.hir_local_at(id.span)), self.tcx.error),
            PatternKind::Literal(e) => {
                let inner = self.hir_child_or_error(e);
                let ty = inner.ty;
                (PK::Literal(Box::new(inner)), ty)
            }
            PatternKind::TypeBinding { ty: ast_ty, binding } => {
                let env = self.local_env();
                let test_ty = self.lower_ty(ast_ty, &env);
                let bind = binding.as_ref().map(|i| self.hir_local_at(i.span));
                (PK::TypeBind { test_ty, bind }, test_ty)
            }
            PatternKind::UnitPath(tp) => {
                let def = self.hir_path_def(tp);
                let test_ty = self.tcx.mk_named(def, Vec::new());
                (PK::UnitPath { def, test_ty }, test_ty)
            }
            PatternKind::TupleStruct { path, fields, rest } => {
                let def = self.hir_path_def(path);
                let fields = fields.iter().map(|f| self.build_pattern(f)).collect();
                (PK::TupleStruct { def, fields, rest: rest.as_ref().map(|r| self.build_rest(r)) },
                 self.tcx.error)
            }
            PatternKind::RecordStruct { path, fields, has_rest } => {
                let def = self.hir_path_def(path);
                let fs = fields.iter().map(|f| self.build_field_pattern(def, f)).collect();
                (PK::RecordStruct { def, fields: fs, has_rest: *has_rest }, self.tcx.error)
            }
            PatternKind::Tuple { elems, rest } => {
                let elems = elems.iter().map(|e| self.build_pattern(e)).collect();
                (PK::Tuple { elems, rest: rest.as_ref().map(|(i, r)| (*i, self.build_rest(r))) },
                 self.tcx.error)
            }
            PatternKind::List { elems, rest } => {
                let elems = elems.iter().map(|e| self.build_pattern(e)).collect();
                (PK::List { elems, rest: rest.as_ref().map(|(i, r)| (*i, self.build_rest(r))) },
                 self.tcx.error)
            }
            PatternKind::Or(ps) => {
                let ps = ps.iter().map(|p| self.build_pattern(p)).collect();
                (PK::Or(ps), self.tcx.error)
            }
        };
        crate::hir::Pattern { kind, ty, span: p.span }
    }

    fn build_field_pattern(
        &mut self,
        def: crate::ids::DefId,
        f: &FieldPattern,
    ) -> crate::hir::FieldPattern {
        let pattern = match &f.pattern {
            Some(p) => self.build_pattern(p),
            None => crate::hir::Pattern {
                kind: crate::hir::PatternKind::Bind(self.hir_local_at(f.name.span)),
                ty: self.expr_ty(f.name.span).unwrap_or(self.tcx.error),
                span: f.name.span,
            },
        };
        crate::hir::FieldPattern {
            index: self.hir_field_index(def, &f.name.name),
            name: f.name.name.clone(),
            pattern,
            span: f.span,
        }
    }

    fn build_rest(&self, r: &RestPattern) -> crate::hir::RestPattern {
        crate::hir::RestPattern {
            bind: r.name.as_ref().map(|i| self.hir_local_at(i.span)),
            span: r.span,
        }
    }

    /// The local bound at a binding-occurrence span (mirrors `lower::local_at`).
    fn hir_local_at(&self, span: Span) -> crate::ids::LocalId {
        match self.resolution(span) {
            Some(crate::sema::results::ValueRes::Local(id)) => id,
            _ => crate::ids::LocalId(u32::MAX),
        }
    }

    /// Resolve a type path to its def (mirrors `lower::path_def`).
    fn hir_path_def(&self, path: &TypePath) -> crate::ids::DefId {
        self.prog
            .resolve_type_in(self.cur_module, &path.name.name)
            .unwrap_or(crate::ids::DefId(0))
    }

    /// The `for` loop driver `check_for` stashed for this node (was the
    /// `for_iters` / `for_maps` / `for_async_iters` tables); falls back to the
    /// `List` fast path for error-recovery when none was recorded.
    fn build_for_driver(&self, iter: &Expr, _in_async: bool) -> crate::hir::ForDriver {
        self.pending_for_driver.take().unwrap_or_else(|| {
            let iter_ty = self.expr_ty(iter.span).unwrap_or(self.tcx.error);
            let elem = self.hir_list_elem(iter_ty).unwrap_or(self.tcx.error);
            crate::hir::ForDriver::ListFast { elem }
        })
    }

    fn hir_list_elem(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(ty) {
            crate::ty::TyKind::Named { def, args }
                if *def == self.prog.list_def && !args.is_empty() =>
            {
                Some(args[0])
            }
            _ => None,
        }
    }

    /// Map child expressions to their built HIR nodes (mirrors
    /// `lower::lower_exprs`); `None` if any child has not yet been migrated.
    fn hir_args(&self, xs: &[&Expr]) -> Option<Vec<crate::hir::Expr>> {
        xs.iter().map(|e| self.hir_child(e)).collect()
    }

    /// The NPO (null-pointer-optimized) pointee of `ty` (mirrors
    /// `lower::npo_pointee`): the pointer payload of a `*T | null` union.
    fn hir_npo_pointee(&self, ty: Ty) -> Ty {
        for v in self.tcx.variants(ty) {
            if let crate::ty::TyKind::Ptr(p) = self.tcx.kind(v) {
                return *p;
            }
        }
        self.tcx.error
    }

    /// Whether a method-call receiver has a builtin type the backend dispatches
    /// structurally (mirrors `lower::is_builtin_recv`).
    fn hir_is_builtin_recv(&self, receiver: &Expr) -> Option<bool> {
        let ty = self.expr_ty(receiver.span)?;
        Some(match self.tcx.kind(ty) {
            crate::ty::TyKind::Str => true,
            crate::ty::TyKind::Named { def, .. } => {
                let p = self.prog;
                *def == p.list_def
                    || *def == p.map_def
                    || (p.sender_def != crate::ids::DefId(0) && *def == p.sender_def)
                    || (p.receiver_def != crate::ids::DefId(0) && *def == p.receiver_def)
                    || (p.shared_def != crate::ids::DefId(0) && *def == p.shared_def)
            }
            _ => false,
        })
    }

    /// The call type arguments recorded at `span` (mirrors `lower::type_args`).

    /// Classify a call into its HIR [`ExprKind`] exactly as `lower::lower_call`
    /// does (folding the marker-set tables into explicit `Intrinsic`/`CallKind`
    /// variants). Returns `None` if any operand has not yet been migrated, so
    /// lowering falls back to its table-driven path for the whole call.
    fn build_call_kind(&self, ce: &Expr, raw_ty: Ty) -> Option<crate::hir::ExprKind> {
        use crate::hir::{CallKind, ExprKind as H, Intrinsic};
        use crate::ids::DefId;
        use crate::sema::results::{num_method_of, ValueRes};
        use crate::ty::TyKind;
        let ExprKind::Call { callee, args, trailing_closure, .. } = &ce.kind else {
            unreachable!("build_call_kind on non-call");
        };
        // A trailing closure is the call's final argument.
        let mut all: Vec<&Expr> = args.iter().collect();
        if let Some(tc) = trailing_closure {
            all.push(tc);
        }
        // Build an `Intrinsic` node from the given operand expressions.
        let intrinsic = |intr: Intrinsic, ops: &[&Expr]| -> Option<H> {
            let args: Option<Vec<_>> = ops.iter().map(|e| self.hir_child(e)).collect();
            Some(H::Intrinsic { intrinsic: intr, args: args? })
        };
        // Finish a `Call`, attaching callee provenance.
        let finish = |kind: CallKind, cargs: Vec<crate::hir::Expr>| -> Option<H> {
            let callee_span = match &callee.kind {
                ExprKind::Field { name, .. } => name.span,
                _ => callee.span,
            };
            // Mirror `lower::expr_ty`: a missing callee type (common for a
            // method-callee `Field` span) falls back to the error type rather
            // than bailing the whole node out to table-driven lowering.
            let callee_ty = self.expr_ty(callee.span).unwrap_or(self.tcx.error);
            Some(H::Call { kind, args: cargs, callee_span, callee_ty })
        };
        // Consume the call-classification facts the check methods stashed for
        // this node (was the `clone_kinds` / `static_calls`+`static_recv` /
        // `foreign_flex` tables). Take all up front so no branch leaves a slot
        // set for a sibling call's build.
        let pending_clone = self.pending_clone_kind.take();
        let pending_static = self.pending_static_recv.take();
        let pending_flex = self.pending_foreign_flex.take();
        let pending_targs = self.pending_type_args.take().unwrap_or_default();

        // --- payload-free prelude builtins recognized by callee shape -------
        if self.resolution(callee.span).is_none() {
            let head1 = &all[..1.min(all.len())];
            if let ExprKind::Ident(n) = &callee.kind {
                match n.name.as_str() {
                    "channel" => return intrinsic(Intrinsic::ChannelNew, &[]),
                    "yield_now" => return intrinsic(Intrinsic::YieldNow, &[]),
                    "sleep" => return intrinsic(Intrinsic::AsyncSleep, head1),
                    "timeout" => {
                        // `output` = the awaited future's `T` (read-only: the
                        // first arg is a `Future<T>`).
                        let out = all
                            .first()
                            .and_then(|a| self.expr_ty(a.span))
                            .and_then(|t| match self.tcx.kind(t) {
                                TyKind::Named { def, args }
                                    if *def == self.prog.future_def && args.len() == 1 =>
                                {
                                    Some(args[0])
                                }
                                _ => None,
                            })
                            .unwrap_or(self.tcx.error);
                        return intrinsic(
                            Intrinsic::AsyncTimeout { output: out },
                            &all[..2.min(all.len())],
                        );
                    }
                    _ => {}
                }
            }
            if let ExprKind::Field { receiver, name } = &callee.kind {
                if let ExprKind::Ident(recv) = &receiver.kind {
                    match (recv.name.as_str(), name.name.as_str()) {
                        ("Shared", "new") => return intrinsic(Intrinsic::SharedNew, &all),
                        ("CString", "from_str") => {
                            return intrinsic(Intrinsic::CStringFromStr, head1)
                        }
                        ("CStr", "to_str") => return intrinsic(Intrinsic::CStrToStr, head1),
                        ("Foreign", "free") => return intrinsic(Intrinsic::ForeignFree, head1),
                        ("Foreign", "realloc") => {
                            return intrinsic(Intrinsic::ForeignRealloc, &all[..2.min(all.len())])
                        }
                        ("Foreign", "alloc") | ("Foreign", "alloc_zeroed") => {
                            let ty = self.hir_npo_pointee(raw_ty);
                            let zeroed = name.name == "alloc_zeroed";
                            return intrinsic(Intrinsic::ForeignAlloc { ty, zeroed }, &[]);
                        }
                        _ => {}
                    }
                }
            }
        }
        // `Thread.spawn { … }`.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if name.name == "spawn" && self.resolution(callee.span).is_none() {
                if matches!(&receiver.kind, ExprKind::Ident(rn) if rn.name == "Thread") {
                    let out = match self.tcx.kind(raw_ty) {
                        TyKind::Named { args, .. } => args.first().copied().unwrap_or(self.tcx.error),
                        _ => self.tcx.error,
                    };
                    return intrinsic(Intrinsic::ThreadSpawn { output: out }, &all);
                }
            }
        }
        // --- marker-set intrinsics keyed by the call span -------------------
        if let Some((t, e)) = pending_flex {
            return intrinsic(Intrinsic::ForeignFlex { ty: t, elem: e }, &all[..1.min(all.len())]);
        }
        // `fut.cancel()`.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if name.name == "cancel" && self.resolution(callee.span).is_none() {
                let rty = self.expr_ty(receiver.span)?;
                let fut = self.prog.future_def;
                if fut != DefId(0)
                    && matches!(self.tcx.kind(rty), TyKind::Named { def, .. } if *def == fut)
                {
                    return intrinsic(Intrinsic::FutureCancel, &[receiver]);
                }
            }
        }
        // Numeric-namespace method.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if self.resolution(callee.span).is_none() {
                if let ExprKind::Ident(recv) = &receiver.kind {
                    if let Some(intr) = num_method_of(self.tcx, &recv.name, &name.name) {
                        return intrinsic(Intrinsic::Num(intr), &all);
                    }
                }
            }
        }
        // `JoinHandle<R>.join()`.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if name.name == "join" && self.resolution(callee.span).is_none() {
                let rty = self.expr_ty(receiver.span)?;
                let jh = self.prog.join_handle_def;
                let out = match self.tcx.kind(rty) {
                    TyKind::Named { def, args } if jh != DefId(0) && *def == jh => {
                        Some(args.first().copied().unwrap_or(self.tcx.error))
                    }
                    _ => None,
                };
                if let Some(out) = out {
                    return intrinsic(Intrinsic::ThreadJoin { output: out }, &[receiver]);
                }
            }
        }
        // Empty builtin collection constructor `List<T>()` / `Map<K,V>()`.
        if self.resolution(callee.span).is_none() {
            let type_name = match &callee.kind {
                ExprKind::Ident(n) => Some(n.name.as_str()),
                ExprKind::Field { receiver, name } if name.name == "new" => match &receiver.kind {
                    ExprKind::Ident(rn) => Some(rn.name.as_str()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(tn) = type_name {
                if let Some(def) = self.prog.resolve_type_in(self.cur_module, tn) {
                    if def == self.prog.list_def || def == self.prog.map_def {
                        return intrinsic(Intrinsic::CollectionCtor, &[]);
                    }
                }
            }
        }
        if let ExprKind::Field { receiver, .. } = &callee.kind {
            if let Some(kind) = pending_clone {
                return intrinsic(Intrinsic::Clone(kind), &[receiver]);
            }
        }
        // --- builtin List/Map/str/Sender/Receiver/Shared methods ------------
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if self.resolution(callee.span).is_none() && self.hir_is_builtin_recv(receiver)? {
                let mut cargs = vec![self.hir_child(receiver)?];
                cargs.extend(self.hir_args(&all)?);
                return finish(CallKind::BuiltinMethod { name: name.name.clone() }, cargs);
            }
        }
        // --- a call through a closure / function-pointer value --------------
        let res = self.resolution(callee.span);
        let is_value =
            matches!(res, Some(ValueRes::Local(_)) | Some(ValueRes::Global(_)) | None);
        if is_value {
            let cty = self.expr_ty(callee.span)?;
            if matches!(self.tcx.kind(cty), TyKind::Func { is_extern: false, .. }) {
                let callee_hir = Box::new(self.hir_child(callee)?);
                let cargs = self.hir_args(&all)?;
                return finish(CallKind::Closure { callee: callee_hir }, cargs);
            }
        }
        // --- resolution-directed dispatch -----------------------------------
        match res {
            Some(ValueRes::Function(d)) => {
                let kind = if self.prog.def(d).kind == DefKind::ExternFunction {
                    CallKind::Extern { def: d }
                } else {
                    CallKind::Direct { def: d, type_args: pending_targs.clone() }
                };
                finish(kind, self.hir_args(&all)?)
            }
            Some(ValueRes::Builtin(b)) => finish(CallKind::Builtin(b), self.hir_args(&all)?),
            Some(ValueRes::StructCtor(d)) => {
                let type_args = match self.tcx.kind(raw_ty) {
                    TyKind::Named { args, .. } => args.clone(),
                    _ => Vec::new(),
                };
                finish(CallKind::TupleCtor { def: d, type_args }, self.hir_args(&all)?)
            }
            Some(ValueRes::Method(d)) => {
                let type_args = pending_targs;
                if let Some(recv) = pending_static {
                    let kind = CallKind::Method {
                        def: d,
                        type_args,
                        recv_static: Some(recv),
                        is_static: true,
                    };
                    finish(kind, self.hir_args(&all)?)
                } else if let ExprKind::Field { receiver, .. } = &callee.kind {
                    let mut cargs = vec![self.hir_child(receiver)?];
                    cargs.extend(self.hir_args(&all)?);
                    let kind =
                        CallKind::Method { def: d, type_args, recv_static: None, is_static: false };
                    finish(kind, cargs)
                } else {
                    Some(H::Error)
                }
            }
            _ => Some(H::Error),
        }
    }

    /// The operator-overload target for the operator expression currently being
    /// built. `try_operator_overload` stashes it in `pending_overload` the moment
    /// it resolves the method; this consumes it (the value lives only between
    /// `check_expr_inner` returning and `build_hir_node` running for the same
    /// node, so a single transient slot suffices and no side table is kept).
    fn hir_overload(&self) -> Option<crate::hir::OpOverload> {
        self.pending_overload.take()
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
                // primitives that `as str` covers are accepted. For each hole
                // (in source order) record the `(to_str method, targs)` the HIR
                // `Str` node needs — `None` for a builtin-typed hole codegen
                // formats directly (was `stringify_methods` + `call_type_args`).
                let mut holes: std::collections::VecDeque<(Option<crate::ids::DefId>, Vec<Ty>)> =
                    std::collections::VecDeque::new();
                for part in &s.parts {
                    let (pty, pspan) = match part {
                        StringPart::Expr(e) => (self.check_expr(e, None), e.span),
                        StringPart::Ident(id) => {
                            let t = self.check_ident(&id.name, id.span);
                            // `check_ident` bypasses the `check_expr` wrapper, so
                            // build the hole's HIR `Name` node here (baking any
                            // narrowing) — `expr_ty` reads its type and
                            // `hir_str_parts` reuses it.
                            if let Some(res) = self.resolution(id.span) {
                                let node = self.build_name_node(res, t, id.span);
                                self.node_hir.insert(id.span, node);
                            }
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
                            holes.push_back((Some(mdef), targs));
                        } else {
                            holes.push_back((None, Vec::new()));
                            let t = self.display(pty);
                            self.emit(pspan, SemaErrorKind::Message(format!(
                                "cannot interpolate `{t}`: it has no `to_str(): str` \
                                 method (add one or `@Derive(ToStr)`)"
                            )));
                        }
                    } else {
                        // A builtin-typed hole — codegen formats it directly.
                        holes.push_back((None, Vec::new()));
                    }
                }
                self.pending_stringify.set(Some(holes));
                self.tcx.str
            }
            ExprKind::Ident(name) => self.check_ident(&name.name, expr.span),
            ExprKind::SelfExpr => match (self.self_local, self.lookup("self")) {
                (Some(id), Some((ty, _))) => {
                    // `self` used inside a closure / `async { … }` block is a
                    // capture, like any other enclosing local.
                    self.record_capture(id, ty);
                    self.record_res(expr.span, ValueRes::Local(id), ty);
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
            ExprKind::Ref { expr: inner, amp_span } => self.check_ref(inner, *amp_span),
            ExprKind::Deref { expr: inner, star_span } => self.check_deref(inner, *star_span),
            ExprKind::Await { expr: inner, kw_span } => self.check_await(inner, *kw_span),
            ExprKind::Spawn { expr: inner, kw_span } => self.check_spawn(inner, *kw_span),
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
                let (elem, driver) = match self.async_iterator_elem(ity) {
                    Some(info) => {
                        let elem = info.elem;
                        (elem, Some(crate::hir::ForDriver::AsyncIter(info)))
                    }
                    None => {
                        if !self.tcx.is_error(ity) {
                            self.emit(iter.span, SemaErrorKind::Message(format!(
                                "`{}` is not an async stream: it has no \
                                 `next_async(self): Future<Item<T> | Done>` method",
                                self.display(ity)
                            )));
                        }
                        (self.tcx.error, None)
                    }
                };
                self.push_scope();
                self.bind_pattern(pattern, elem);
                self.loops.push(LoopFrame { is_loop: false, break_types: Vec::new() });
                self.check_block(body, None);
                self.loops.pop();
                self.pop_scope();
                // Hand the loop driver to the HIR `For` node — set after the body
                // so a nested `for` cannot clobber the slot (was `for_async_iters`).
                self.pending_for_driver.set(driver);
                self.tcx.null
            }
            ExprKind::For { pattern, iter, body, .. } => {
                let ity = self.check_expr(iter, None);
                let (elem, driver) = match self.list_elem(ity) {
                    Some(e) => (e, Some(crate::hir::ForDriver::ListFast { elem: e })),
                    None if self.tcx.is_error(ity) => (self.tcx.error, None),
                    None if self.map_kv(ity).is_some() => {
                        // `for entry in map` yields `Entry<K, V>` (docs/18 §6).
                        let (kt, vt) = self.map_kv(ity).unwrap();
                        let entry_ty = self.tcx.mk_named(self.prog.entry_def, vec![kt, vt]);
                        (entry_ty, Some(crate::hir::ForDriver::Map { key: kt, value: vt, entry: entry_ty }))
                    }
                    None if matches!(self.tcx.kind(ity), TyKind::Str) => {
                        // `for ch in s` ≡ `for ch in s.chars()` (docs/18 §4).
                        (self.tcx.char, Some(crate::hir::ForDriver::StrChars))
                    }
                    None => match self.iterator_elem(ity) {
                        Some((elem, next, next_targs, item_ty, done_ty)) => {
                            let info = crate::sema::results::ForIter {
                                elem, next, next_targs, iter_ty: ity, done_ty, item_ty,
                            };
                            (elem, Some(crate::hir::ForDriver::Iter(info)))
                        }
                        None => {
                            self.emit(iter.span, SemaErrorKind::Message(format!(
                                "`{}` is not iterable: it is not a `List` and has no \
                                 `next(self): Item<T> | Done` method",
                                self.display(ity)
                            )));
                            (self.tcx.error, None)
                        }
                    },
                };
                self.push_scope();
                self.bind_pattern(pattern, elem);
                self.loops.push(LoopFrame { is_loop: false, break_types: Vec::new() });
                self.check_block(body, None);
                self.loops.pop();
                self.pop_scope();
                // Hand the loop driver to the HIR `For` node — set after the body
                // so a nested `for` cannot clobber the slot (was `for_iters` /
                // `for_maps`).
                self.pending_for_driver.set(driver);
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
                    None => self.emit(expr.span, SemaErrorKind::LoopControlOutsideLoop {
                        kw: "break",
                    }),
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
                    self.emit(expr.span, SemaErrorKind::LoopControlOutsideLoop {
                        kw: "continue",
                    });
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
            let _ = span;
            self.pending_async.set(Some(crate::sema::results::AsyncInfo {
                output: body_out,
                params: param_locals,
                captures: frame.captures,
            }));
            return self.tcx.mk_func(param_tys, fut_ty, false);
        }
        let _ = span;
        self.pending_closure.set(Some(crate::sema::results::ClosureInfo {
            params: param_locals,
            captures: frame.captures,
            ret: body_out,
        }));
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
                let _ = kw_span;
                self.pending_await.set(Some(out));
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

    /// Type-check `spawn EXPR` (`docs/21` §6): schedule a future on the async
    /// executor. `EXPR` must be a `Future<Output>`; the result is also
    /// `Future<Output>` — the spawn-handle is itself an awaitable future, in
    /// the style of JavaScript/Dart and Tokio's `JoinHandle`.
    pub(crate) fn check_spawn(&mut self, inner: &Expr, kw_span: Span) -> Ty {
        let fty = self.check_expr(inner, None);
        if self.tcx.is_error(fty) {
            return self.tcx.error;
        }
        match self.future_output(fty) {
            Some(out) => {
                let _ = kw_span;
                self.pending_spawn.set(Some(out));
                self.tcx.mk_named(self.prog.future_def, vec![out])
            }
            None => {
                let t = self.display(fty);
                self.emit(inner.span, SemaErrorKind::Message(format!(
                    "`spawn` requires a `Future`, but `{t}` is not one"
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
        let _ = span;
        self.pending_async.set(Some(crate::sema::results::AsyncInfo {
            output: out,
            params: Vec::new(),
            captures: frame.captures,
        }));
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
            self.record_capture(id, ty);
            // Flow narrowing: if this local is narrowed in the current branch,
            // report the narrowed type. The unbox-at-use coercion (when narrowed
            // from a boxed union / interface to a single variant) is recovered
            // structurally by `build_name_node` (which `record_res` calls) from
            // the local's declared type vs this use type.
            let result = self.narrowings.get(&id).copied().unwrap_or(ty);
            self.record_res(span, ValueRes::Local(id), result);
            return result;
        }
        // A module-level value: function, var, or extern function/var.
        let module = self.current_module();
        if let Some(def) = self.prog.resolve_value_in(module, name) {
            // A prelude marker function (`print`/`panic`/…, imported by name)
            // lowers to its builtin intrinsic (`docs/17` §17.8).
            if let Some(b) = self.prog.builtin_of_def(def) {
                let t = self.builtin_ty(b);
                self.record_res(span, ValueRes::Builtin(b), t);
                return t;
            }
            let res = self.value_res(def);
            let t = self.value_def_ty(def);
            self.record_res(span, res, t);
            return t;
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
        let from = self.check_expr(inner, None);
        // Hand the resolved target to the `Cast` HIR node (consumed right after
        // this returns); set it *after* checking `inner` so a nested cast child
        // doesn't clobber the slot. `cast_span` is the node's own span.
        let _ = cast_span;
        self.pending_cast_target.set(Some(to));
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
        // Raw-pointer reinterpretation: `*A as *B` is a no-op at runtime — the
        // user vouches for the layout (`docs/19` §2). A pointer also converts
        // to/from a pointer-width integer (`usize`/`isize`).
        let from_ptr = matches!(self.tcx.kind(from), TyKind::Ptr(_));
        let to_ptr = matches!(self.tcx.kind(to), TyKind::Ptr(_));
        if from_ptr && to_ptr {
            return true;
        }
        let is_ptr_int = |t: Ty| matches!(self.tcx.kind(t), TyKind::Int(IntTy::Usize | IntTy::Isize));
        if (from_ptr && is_ptr_int(to)) || (to_ptr && is_ptr_int(from)) {
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
        Some(ty)
    }

    /// Whether `name` is an *imported* toolchain free-function intrinsic
    /// (`channel`/`sleep`/…): not shadowed by a local, and resolving to a
    /// built-in (`__builtins__`) value def. A program must `import` the name for
    /// the intrinsic to be recognized (`docs/17` §17.8).
    fn intr_fn(&self, name: &str) -> bool {
        self.lookup(name).is_none()
            && self
                .prog
                .resolve_value_in(self.current_module(), name)
                .is_some_and(|d| self.prog.is_builtin_def(d))
    }

    /// As [`Self::intr_fn`], for an imported toolchain *namespace* type
    /// (`Thread`/`Shared`/`Foreign`/`CString`/`CStr`): resolves to a built-in
    /// type def and is not shadowed by a local.
    fn intr_ns(&self, name: &str) -> bool {
        self.lookup(name).is_none()
            && self
                .prog
                .resolve_type_in(self.current_module(), name)
                .is_some_and(|d| self.prog.is_builtin_def(d))
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
        // A prelude marker function (`print`/`println`/`panic`/`panic_with`/
        // `exit`/`abort`, imported by name) lowers to its builtin intrinsic
        // (`docs/17` §17.8). Handled here so it takes priority over the generic-
        // function path (`panic_with<T>` is generic), unless shadowed by a local.
        if let ExprKind::Ident(name) = &callee.kind {
            if self.lookup(&name.name).is_none() {
                if let Some(def) = self.prog.resolve_value_in(self.current_module(), &name.name) {
                    if let Some(b) = self.prog.builtin_of_def(def) {
                        let t = self.builtin_ty(b);
                        self.record_res(callee.span, ValueRes::Builtin(b), t);
                        return self.check_args_against(t, args, trailing, span);
                    }
                }
            }
        }
        // `channel<T>()` (`docs/20` §2): construct a message-passing channel.
        if let ExprKind::Ident(name) = &callee.kind {
            if name.name == "channel" && self.intr_fn("channel") {
                return self.check_channel_new(_generics, args, span);
            }
        }
        // `yield_now()` (`docs/21`): a `Future<null>` that suspends once.
        if let ExprKind::Ident(name) = &callee.kind {
            if name.name == "yield_now" && self.intr_fn("yield_now") {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                return self.tcx.mk_named(self.prog.future_def, vec![self.tcx.null]);
            }
            // `sleep(ms)` (`docs/21` §9): a `Future<null>` completing after a delay.
            if name.name == "sleep" && self.intr_fn("sleep") {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                } else {
                    let i64t = self.tcx.int(IntTy::I64);
                    let a = self.check_expr(&args[0], Some(i64t));
                    self.expect(a, i64t, args[0].span);
                }
                return self.tcx.mk_named(self.prog.future_def, vec![self.tcx.null]);
            }
            // `timeout(fut, ms): Future<T | TimedOut>` (`docs/21` §9): race a
            // future against a deadline.
            if name.name == "timeout" && self.intr_fn("timeout") {
                if args.len() != 2 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 2, found: args.len() });
                    return self.tcx.error;
                }
                let ft = self.check_expr(&args[0], None);
                let i64t = self.tcx.int(IntTy::I64);
                let m = self.check_expr(&args[1], Some(i64t));
                self.expect(m, i64t, args[1].span);
                let Some(out) = self.future_output(ft) else {
                    if !self.tcx.is_error(ft) {
                        self.emit(args[0].span, SemaErrorKind::Message(format!(
                            "`timeout` expects a future as its first argument, found `{}`",
                            self.display(ft)
                        )));
                    }
                    return self.tcx.error;
                };
                let timedout = self.tcx.mk_named(self.prog.timed_out_def, vec![]);
                let union = self.tcx.mk_union([out, timedout]);
                return self.tcx.mk_named(self.prog.future_def, vec![union]);
            }
        }
        // `Shared.new(value)` (`docs/20` §4): construct a mutex-protected cell.
        // Recognised before the static-method path (which would not find `new`).
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(recv) = &receiver.kind {
                if recv.name == "Shared" && name.name == "new" && self.intr_ns(&recv.name) {
                    return self.check_shared_new(args, span);
                }
            }
        }
        // `Foreign.alloc<T>()` / `Foreign.alloc_zeroed<T>()` / `Foreign.free(p)`
        // (`docs/19` §5): manual foreign-heap allocation.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let ExprKind::Ident(recv) = &receiver.kind {
                if recv.name == "Foreign" && self.intr_ns(&recv.name) {
                    return self.check_foreign_builtin(&name.name, _generics, args, span);
                }
                // `CString.from_str(s)` / `CStr.to_str(p)` (`docs/19` §6).
                if recv.name == "CString" && name.name == "from_str" && self.intr_ns(&recv.name) {
                    return self.check_cstring_from_str(args, span);
                }
                if recv.name == "CStr" && name.name == "to_str" && self.intr_ns(&recv.name) {
                    return self.check_cstr_to_str(args, span);
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
                if m.name == "Thread" && name.name == "spawn" && self.intr_ns(&m.name) {
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
        // trailing closure (`xs.map { … }`) is the final argument. Explicit
        // method-level generics (`b.map<U>(...)`) are threaded through.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if let Some(tc) = trailing {
                let mut all = args.to_vec();
                all.push(tc.clone());
                return self.check_method_call_with_generics(
                    callee, receiver, name, &all, _generics, span,
                );
            }
            return self.check_method_call_with_generics(
                callee, receiver, name, args, _generics, span,
            );
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
        self.record_res(callee.span, self.value_res(def), self.tcx.error);
        let callee_ty = self.value_def_ty(def);
        self.check_args_against(callee_ty, args, trailing, span)
    }

}
