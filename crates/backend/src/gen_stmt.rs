//! Per-function codegen: statements, blocks, patterns, and assignment (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- statements & blocks -------------------------------------------------

    pub(crate) fn gen_block(&mut self, block: &Block) -> CgResult<Option<Value>> {
        for stmt in &block.stmts {
            self.gen_stmt(stmt)?;
            if self.term {
                // Unreachable code after a diverging statement; stop.
                return Ok(None);
            }
        }
        match &block.trailing {
            Some(e) => self.gen_expr(e),
            None => Ok(None),
        }
    }

    pub(crate) fn gen_stmt(&mut self, stmt: &Stmt) -> CgResult<()> {
        match &stmt.kind {
            StmtKind::Var(local) => {
                let init_ty = self.cx.analysis.results.expr_ty(local.init.span)
                    .unwrap_or(self.cx.analysis.tcx.error);
                let val = self.gen_expr(&local.init)?;
                self.bind_pattern(&local.pattern, val, init_ty)?;
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                let v = self.gen_expr(value)?;
                self.gen_assign(target, v)?;
                Ok(())
            }
            StmtKind::Expr(e) => {
                self.gen_expr(e)?;
                Ok(())
            }
            StmtKind::Item(_) => Ok(()),
        }
    }

    pub(crate) fn bind_pattern(&mut self, pattern: &Pattern, val: Option<Value>, ty: Ty) -> CgResult<()> {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                if let Some(v) = val {
                    let ct = self.b.func.dfg.value_type(v);
                    let local = match self.cx.analysis.results.resolution(name.span) {
                        Some(ValueRes::Local(id)) => id,
                        _ => return Err(CodegenError::new(name.span, "unresolved binding")),
                    };
                    self.bind_local(local, ct, v);
                }
                Ok(())
            }
            PatternKind::Wildcard => Ok(()),
            // Irrefutable tuple/struct destructuring: load each element from the
            // aggregate pointer and bind the sub-pattern.
            PatternKind::Tuple { elems, rest: None } => {
                let ptr = val.ok_or_else(|| {
                    CodegenError::new(pattern.span, "destructured value has no pointer")
                })?;
                let layout = self.layout_for_ty(ty).ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple pattern on non-aggregate")
                })?;
                let elem_tys = match self.cx.analysis.tcx.kind(ty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => Some(self.b.ins().load(
                            *ct,
                            MemFlags::trusted(),
                            ptr,
                            layout.offsets[i] as i32,
                        )),
                        _ => None,
                    };
                    self.bind_pattern(sub, elem_val, elem_tys[i])?;
                }
                Ok(())
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable")),
        }
    }

    pub(crate) fn gen_assign(&mut self, target: &Expr, val: Option<Value>) -> CgResult<()> {
        match &target.kind {
            ExprKind::Ident(_) => {
                let local = self.resolve_local(target.span)?;
                if let Some(v) = val {
                    self.write_local(local, v, target.span)?;
                }
                Ok(())
            }
            ExprKind::Underscore => Ok(()),
            ExprKind::Field { receiver, name } => {
                self.gen_field_store(receiver, &name.name, val)
            }
            ExprKind::TupleIndex { receiver, index, .. } => {
                self.gen_field_store(receiver, &index.to_string(), val)
            }
            ExprKind::Index { receiver, index } => self.gen_index_store(receiver, index, val),
            _ => Err(CodegenError::new(target.span, "assignment target not yet lowerable")),
        }
    }

}
