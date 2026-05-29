//! HIR-consuming code generation (migration Stage 3).
//!
//! A per-function generator that walks the typed [`compiler::hir`] tree instead
//! of the AST + span side-tables. It is built and tested *incrementally* beside
//! the AST path: [`crate::Codegen`] picks the HIR walk for any function whose
//! body uses only the forms covered here (see [`hir_supported_block`]) and
//! falls back to the AST walk otherwise, so the whole program always compiles
//! and `compile_hir` stays green while coverage grows. Once every form is
//! covered, the AST walk is deleted and this becomes the sole backend.
//!
//! Every *value-level* helper (arithmetic, comparisons, panics, calls, locals,
//! coercions) is shared with the AST path — only the tree walk differs, which
//! is exactly the win the HIR delivers: no span lookups, no re-derivation.

use super::*;
use compiler::hir;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- blocks & statements -------------------------------------------------

    pub(crate) fn h_block(&mut self, block: &hir::Block) -> CgResult<Option<Value>> {
        for stmt in &block.stmts {
            self.h_stmt(stmt)?;
            if self.term {
                return Ok(None);
            }
        }
        match &block.trailing {
            Some(e) => self.h_expr(e),
            None => Ok(None),
        }
    }

    fn h_stmt(&mut self, stmt: &hir::Stmt) -> CgResult<()> {
        match &stmt.kind {
            hir::StmtKind::Let { pattern, init } => {
                let init_ty = init.ty;
                let val = self.h_expr(init)?;
                self.h_bind_pattern(pattern, val, init_ty)
            }
            hir::StmtKind::Assign { target, value } => {
                let v = self.h_expr(value)?;
                self.h_assign(target, v)
            }
            hir::StmtKind::Expr(e) => {
                self.h_expr(e)?;
                Ok(())
            }
            hir::StmtKind::Item(_) => Ok(()),
        }
    }

    fn h_bind_pattern(&mut self, pattern: &hir::Pattern, val: Option<Value>, ty: Ty) -> CgResult<()> {
        match &pattern.kind {
            hir::PatternKind::Bind(local) => {
                if let Some(v) = val {
                    let ct = self.b.func.dfg.value_type(v);
                    self.bind_local(*local, ct, v);
                }
                Ok(())
            }
            hir::PatternKind::Wildcard => Ok(()),
            // Irrefutable tuple destructuring: load each element and bind it.
            hir::PatternKind::Tuple { elems, rest: None } => {
                let ptr = val
                    .ok_or_else(|| CodegenError::new(pattern.span, "destructured value has no pointer"))?;
                let layout = self
                    .layout_for_ty(ty)
                    .ok_or_else(|| CodegenError::new(pattern.span, "tuple pattern on non-aggregate"))?;
                let elem_tys = match self.cx.analysis.tcx.kind(ty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => {
                            Some(self.b.ins().load(*ct, MemFlags::trusted(), ptr, layout.offsets[i] as i32))
                        }
                        _ => None,
                    };
                    self.h_bind_pattern(sub, elem_val, elem_tys[i])?;
                }
                Ok(())
            }
            // Irrefutable tuple-struct destructuring `Pair(a, b)`: load each
            // field (positionally) from the struct's layout and bind it.
            hir::PatternKind::TupleStruct { fields, .. } => {
                let ptr = val
                    .ok_or_else(|| CodegenError::new(pattern.span, "destructured value has no pointer"))?;
                let layout = self
                    .layout_for_ty(ty)
                    .ok_or_else(|| CodegenError::new(pattern.span, "struct pattern on non-aggregate"))?;
                for (i, sub) in fields.iter().enumerate() {
                    let fv = self.h_load_field(ptr, &layout, i);
                    let fty = layout.tys.get(i).copied().unwrap_or(self.cx.analysis.tcx.error);
                    self.h_bind_pattern(sub, fv, fty)?;
                }
                Ok(())
            }
            // Irrefutable record destructuring `Point { x, y }` / `{ x: a, .. }`:
            // each field carries its index into the struct layout.
            hir::PatternKind::RecordStruct { fields, .. } => {
                let ptr = val
                    .ok_or_else(|| CodegenError::new(pattern.span, "destructured value has no pointer"))?;
                let layout = self
                    .layout_for_ty(ty)
                    .ok_or_else(|| CodegenError::new(pattern.span, "struct pattern on non-aggregate"))?;
                for fp in fields {
                    let i = fp.index as usize;
                    let fv = self.h_load_field(ptr, &layout, i);
                    let fty = layout.tys.get(i).copied().unwrap_or(self.cx.analysis.tcx.error);
                    self.h_bind_pattern(&fp.pattern, fv, fty)?;
                }
                Ok(())
            }
            _ => Err(CodegenError::new(pattern.span, "HIR codegen: pattern not yet supported")),
        }
    }

    /// Load field `i` of an aggregate at `ptr` per `layout`, or `None` for a
    /// zero-sized (no-clty) field.
    fn h_load_field(&mut self, ptr: Value, layout: &Layout, i: usize) -> Option<Value> {
        match layout.cltys.get(i) {
            Some(Some(ct)) => {
                let v = self.b.ins().load(*ct, MemFlags::trusted(), ptr, layout.offsets[i] as i32);
                Some(v)
            }
            _ => None,
        }
    }

    fn h_assign(&mut self, target: &hir::Expr, val: Option<Value>) -> CgResult<()> {
        match &target.kind {
            hir::ExprKind::Name(hir::Res::Local(local)) => {
                if let Some(v) = val {
                    self.write_local(*local, v, target.span)?;
                }
                Ok(())
            }
            // Assigning to an `extern var` C global (`docs/19` §4) stores through
            // its imported data symbol.
            hir::ExprKind::Name(hir::Res::Global(def))
                if self.cx.analysis.program.def(*def).kind == DefKind::ExternVar =>
            {
                let addr = self.extern_var_addr(*def);
                if let Some(v) = val {
                    self.b.ins().store(MemFlags::trusted(), v, addr, 0);
                }
                Ok(())
            }
            hir::ExprKind::Field { receiver, field } => {
                self.h_field_store(receiver, field.index as usize, val)
            }
            hir::ExprKind::TupleIndex { receiver, index } => {
                self.h_field_store(receiver, *index as usize, val)
            }
            hir::ExprKind::Index { receiver, index } => self.h_index_store(receiver, index, val),
            // `*p = v` — store a scalar through a raw pointer (`docs/19` §2).
            hir::ExprKind::Deref(inner) => {
                let p = self
                    .h_expr(inner)?
                    .ok_or_else(|| CodegenError::new(inner.span, "dereference operand has no value"))?;
                let zero = self.b.ins().iconst(PTR, 0);
                let is_null = self.b.ins().icmp(IntCC::Equal, p, zero);
                self.guard_panic(is_null, "dereference of a null pointer");
                if let Some(v) = val {
                    self.b.ins().store(MemFlags::trusted(), v, p, 0);
                }
                Ok(())
            }
            hir::ExprKind::Discard => Ok(()),
            _ => Err(CodegenError::new(target.span, "HIR codegen: assignment target not yet supported")),
        }
    }

    /// Store into a field/tuple position (mirrors the AST `gen_field_store`).
    fn h_field_store(&mut self, receiver: &hir::Expr, idx: usize, val: Option<Value>)
        -> CgResult<()>
    {
        let rty = receiver.ty;
        let layout = self
            .layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "field assignment on non-aggregate"))?;
        let ptr = self
            .h_expr(receiver)?
            .ok_or_else(|| CodegenError::new(receiver.span, "receiver has no value"))?;
        let off = *layout
            .offsets
            .get(idx)
            .ok_or_else(|| CodegenError::new(receiver.span, "field index out of range"))?
            as i32;
        if is_extern_struct_ty(self.cx.analysis, layout.tys[idx]) {
            if let Some(v) = val {
                let n = self.sizeof_ty(layout.tys[idx]);
                self.copy_bytes(ptr, off, v, n);
            }
            return Ok(());
        }
        if let (Some(v), Some(_)) = (val, layout.cltys[idx]) {
            self.b.ins().store(MemFlags::trusted(), v, ptr, off);
        }
        Ok(())
    }

    // -- expressions ---------------------------------------------------------

    pub(crate) fn h_expr(&mut self, e: &hir::Expr) -> CgResult<Option<Value>> {
        // Tag the instructions generated for this node with its source byte
        // offset, so Cranelift carries source provenance to the machine code
        // (the basis for `--emit`-level debugging and DWARF line tables). The
        // innermost expression being generated is the active location.
        self.b.set_srcloc(cranelift_codegen::ir::SourceLoc::new(e.span.lo.0));
        let v = self.h_expr_inner(e)?;
        // A managed-ref result is a GC root if it stays live across a later
        // allocation; mark it so Cranelift records it in stack maps (mirrors the
        // AST path's `gen_expr` + `result_is_managed_ref`). The HIR node's own
        // `ty` is already the post-coercion type and `is_managed_ptr`
        // special-cases NPO unions, so this single check covers every case
        // (`Widen`/`Unbox`/`WidenDyn` land on the explicit `Adjust` node's `ty`).
        if let Some(val) = v {
            let rty = resolve_shallow(self.cx.analysis, e.ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, rty) {
                self.b.declare_value_needs_stack_map(val);
            }
        }
        Ok(v)
    }

    fn h_expr_inner(&mut self, e: &hir::Expr) -> CgResult<Option<Value>> {
        use hir::ExprKind as K;
        let ty = e.ty;
        match &e.kind {
            K::Int(v) => {
                let ct = self.cx_clty(ty).unwrap_or(types::I64);
                Ok(Some(self.b.ins().iconst(ct, *v as i64)))
            }
            K::Float(v) => Ok(Some(match self.cx.analysis.tcx.kind(ty) {
                TyKind::Float(FloatTy::F32) => self.b.ins().f32const(*v as f32),
                _ => self.b.ins().f64const(*v),
            })),
            K::Bool(b) => Ok(Some(self.b.ins().iconst(types::I8, i64::from(*b)))),
            K::Char(c) => Ok(Some(self.b.ins().iconst(types::I32, *c as i64))),
            K::Null => Ok(None),
            K::Name(res) => self.h_name(*res, ty, e.span),
            K::Unary { op, operand, overload } => {
                // Only binary operators can be overloaded (`docs/15`); the checker
                // never records a unary overload, so `overload` is always `None`.
                debug_assert!(overload.is_none(), "unary operator overloads do not exist");
                self.h_unary(*op, operand, ty)
            }
            K::Binary { op, left, right, overload } => {
                if let Some(ov) = overload {
                    return self.h_binary_overload(*op, ov, left, right, e.span);
                }
                if matches!(op, hir::BinaryOp::And | hir::BinaryOp::Or) {
                    return self.h_logical(*op, left, right);
                }
                let lty = left.ty;
                let l = self
                    .h_expr(left)?
                    .ok_or_else(|| CodegenError::new(left.span, "operand has no value"))?;
                let r = self
                    .h_expr(right)?
                    .ok_or_else(|| CodegenError::new(right.span, "operand has no value"))?;
                self.emit_binop(hir_to_ast_binop(*op), lty, l, r)
            }
            K::If { cond, then_block, else_branch } => {
                self.h_if(cond, then_block, else_branch.as_deref(), ty)
            }
            K::Block(b) => self.h_block(b),
            K::Return(v) => {
                let val = match v {
                    Some(e) => self.h_expr(e)?,
                    None => None,
                };
                self.emit_return(val)?;
                Ok(None)
            }
            K::While { cond, body } => self.h_while(cond, body),
            K::Loop(b) => self.h_loop(b, ty),
            K::Break(v) => self.h_break(v.as_deref(), e.span),
            K::Continue => self.h_continue(e.span),
            K::Call { kind, args, .. } => self.h_call(kind, args, ty, e.span),
            K::Adjust { adjust, expr } => self.h_adjust(adjust, expr),
            K::Struct { def, type_args, fields, spread } => Ok(Some(self.h_struct_lit(
                *def,
                type_args,
                fields,
                spread.as_deref(),
                ty,
                e.span,
            )?)),
            K::Field { receiver, field } => self.h_field_at(receiver, field.index as usize),
            K::TupleIndex { receiver, index } => self.h_field_at(receiver, *index as usize),
            K::Cast { op, expr, target } => {
                let from = expr.ty;
                let opv = self.h_expr(expr)?;
                match op {
                    hir::CastOp::Is => self.emit_is(opv, from, *target, expr.span),
                    hir::CastOp::As => self.emit_cast(opv, from, *target, expr.span),
                }
            }
            K::Tuple(elems) => self.h_tuple(elems, ty),
            K::Str(parts) => self.h_str(parts).map(Some),
            K::List(elems) => self.h_list(elems, ty),
            K::Map(items) => self.h_map(items, ty, e.span),
            K::Index { receiver, index } => self.h_index_load(receiver, index),
            K::Ref(inner) => self.h_ref(inner),
            K::Deref(inner) => self.h_deref(inner, ty),
            K::Try { expr, branch, residual_conversions } => {
                self.h_try(expr, branch.as_ref(), residual_conversions, ty)
            }
            K::Match { scrutinee, arms } => self.h_match(scrutinee, arms, ty),
            K::For { pattern, iter, body, driver, .. } => self.h_for(pattern, iter, body, driver),
            K::Closure { params, captures, ret, is_async, body } => {
                self.h_closure(params, captures, *ret, *is_async, body, e.span)
            }
            K::Intrinsic { intrinsic, args } => self.h_intrinsic(intrinsic, args, ty, e.span),
            K::Spawn { expr, output } => {
                let fut = self
                    .h_expr(expr)?
                    .ok_or_else(|| CodegenError::new(expr.span, "spawn operand has no value"))?;
                self.emit_spawn(fut, *output)
            }
            K::Await { expr, output } => {
                let fut = self
                    .h_expr(expr)?
                    .ok_or_else(|| CodegenError::new(expr.span, "awaited expression has no value"))?;
                // Key the suspend site by the `Await` node's span — the same span
                // `h_scan_stmt_awaits` records, so the resume map in
                // `build_stateful_poll` matches.
                self.emit_await_suspend(fut, e.span, *output)
            }
            K::AsyncBlock { output, params, captures, body } => {
                self.h_async_block(*output, params, captures, body, e.span)
            }
            _ => Err(CodegenError::new(e.span, "HIR codegen: expression not yet supported")),
        }
    }

    fn h_name(&mut self, res: hir::Res, ty: Ty, span: Span) -> CgResult<Option<Value>> {
        match res {
            hir::Res::Local(local) => self
                .read_local(local)
                .map(Some)
                .ok_or_else(|| CodegenError::new(span, "use of unbound local")),
            // A unit struct used as a value carries no data — a null placeholder.
            hir::Res::StructCtor(_) => Ok(Some(self.b.ins().iconst(PTR, 0))),
            hir::Res::Global(def)
                if self.cx.analysis.program.def(def).kind == DefKind::ExternVar =>
            {
                let addr = self.extern_var_addr(def);
                match clty_of(self.cx.analysis, ty) {
                    Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), addr, 0))),
                    None => Ok(None),
                }
            }
            _ => Err(CodegenError::new(span, "HIR codegen: value reference not yet supported")),
        }
    }

    fn h_unary(&mut self, op: hir::UnaryOp, operand: &hir::Expr, ty: Ty) -> CgResult<Option<Value>> {
        let v = self
            .h_expr(operand)?
            .ok_or_else(|| CodegenError::new(operand.span, "operand has no value"))?;
        let is_float = matches!(self.cx.analysis.tcx.kind(ty), TyKind::Float(_));
        let is_bool = matches!(self.cx.analysis.tcx.kind(ty), TyKind::Bool);
        let out = match op {
            hir::UnaryOp::Neg if is_float => self.b.ins().fneg(v),
            hir::UnaryOp::Neg => self.b.ins().ineg(v),
            hir::UnaryOp::Not if is_bool => {
                let one = self.b.ins().iconst(types::I8, 1);
                self.b.ins().bxor(v, one)
            }
            hir::UnaryOp::Not => self.b.ins().bnot(v),
        };
        Ok(Some(out))
    }

    fn h_logical(&mut self, op: hir::BinaryOp, left: &hir::Expr, right: &hir::Expr)
        -> CgResult<Option<Value>>
    {
        let l = self
            .h_expr(left)?
            .ok_or_else(|| CodegenError::new(left.span, "operand has no value"))?;
        let rhs_block = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I8);
        match op {
            hir::BinaryOp::And => {
                self.b.ins().brif(l, rhs_block, &[], merge, &[l.into()]);
            }
            hir::BinaryOp::Or => {
                self.b.ins().brif(l, merge, &[l.into()], rhs_block, &[]);
            }
            _ => unreachable!(),
        }
        self.term = true;
        self.switch(rhs_block);
        let r = self
            .h_expr(right)?
            .ok_or_else(|| CodegenError::new(right.span, "operand has no value"))?;
        if !self.term {
            self.b.ins().jump(merge, &[r.into()]);
            self.term = true;
        }
        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// An overloaded binary operator `a <op> b` on a user type: call the
    /// resolved `extend` method (`docs/15`). `a != b` calls the type's `eq` and
    /// negates the result. The `extend`'s solved type arguments are carried on
    /// the node; resolve them through the current instance's substitution before
    /// monomorphizing the callee — mirrors the AST `gen_binary` overload path.
    fn h_binary_overload(
        &mut self,
        op: hir::BinaryOp,
        ov: &hir::OpOverload,
        left: &hir::Expr,
        right: &hir::Expr,
        op_span: Span,
    ) -> CgResult<Option<Value>> {
        let l = self
            .h_expr(left)?
            .ok_or_else(|| CodegenError::new(left.span, "operand has no value"))?;
        let r = self
            .h_expr(right)?
            .ok_or_else(|| CodegenError::new(right.span, "operand has no value"))?;
        let targs: Vec<Ty> = ov
            .type_args
            .iter()
            .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
            .collect();
        let result = self.emit_call(ov.method, targs, &[l, r], op_span)?;
        if matches!(op, hir::BinaryOp::Ne) {
            let v = result.ok_or_else(|| CodegenError::new(op_span, "`eq` returned no value"))?;
            let zero = self.b.ins().iconst(types::I8, 0);
            return Ok(Some(self.b.ins().icmp(IntCC::Equal, v, zero)));
        }
        Ok(result)
    }

    /// The `?` operator (`docs/13`). Mirrors the AST `gen_try`: if the operand
    /// has a `Try` impl, call `branch(self)` to get an `Output | Residual` union;
    /// otherwise the operand IS the union. Failure variants early-return (raw box
    /// when the function returns a union, unboxed otherwise); residual-conversion
    /// variants are routed through `from_residual`; the success path narrows the
    /// box to the expression's `success_ty`. All resolution data lives on the HIR
    /// node — no span-keyed `CheckResults` lookups.
    fn h_try(
        &mut self,
        operand: &hir::Expr,
        branch: Option<&hir::TryBranch>,
        residual_conversions: &[(Ty, DefId, Ty)],
        success_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let raw_v = self
            .h_expr(operand)?
            .ok_or_else(|| CodegenError::new(operand.span, "`?` operand has no value"))?;
        let (ptr, et) = if let Some(tb) = branch {
            let targs: Vec<Ty> = tb
                .targs
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect();
            let u = self
                .emit_call(tb.method, targs, &[raw_v], operand.span)?
                .ok_or_else(|| CodegenError::new(operand.span, "`branch` returned no value"))?;
            (u, resolve_shallow(self.cx.analysis, tb.union_ty, &self.subst))
        } else {
            (raw_v, resolve_shallow(self.cx.analysis, operand.ty, &self.subst))
        };
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);

        let r = self.ret_ty;
        let r_variants = self.cx.analysis.tcx.variants(r);
        let conv_residuals: Vec<Ty> =
            residual_conversions.iter().map(|(rv, _, _)| *rv).collect();
        let failures: Vec<Ty> = match branch {
            Some(tb) => self
                .cx
                .analysis
                .tcx
                .variants(resolve_shallow(self.cx.analysis, tb.residual, &self.subst))
                .into_iter()
                .filter(|v| !conv_residuals.contains(v))
                .collect(),
            None => self
                .cx
                .analysis
                .tcx
                .variants(et)
                .into_iter()
                .filter(|v| r_variants.contains(v))
                .filter(|v| !conv_residuals.contains(v))
                .collect(),
        };

        for fv in failures {
            let fid = {
                let id = self.type_id_of(fv);
                self.b.ins().iconst(types::I64, id)
            };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, fid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            let ret_val = if matches!(self.cx.analysis.tcx.kind(r), TyKind::Union(_) | TyKind::Dynamic) {
                Some(ptr)
            } else {
                clty_of(self.cx.analysis, r)
                    .map(|ct| self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))
            };
            self.emit_return(ret_val)?;

            self.switch(next);
        }

        for (residual, method, target) in residual_conversions.iter().copied() {
            let residual = resolve_shallow(self.cx.analysis, residual, &self.subst);
            let target = resolve_shallow(self.cx.analysis, target, &self.subst);
            let rid = {
                let id = self.type_id_of(residual);
                self.b.ins().iconst(types::I64, id)
            };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, rid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            let payload = match clty_of(self.cx.analysis, residual) {
                Some(ct) => self.b.ins().load(ct, MemFlags::trusted(), ptr, 8),
                None => self.b.ins().iconst(PTR, 0),
            };
            let converted = self
                .emit_call(method, Vec::new(), &[payload], operand.span)?
                .ok_or_else(|| CodegenError::new(operand.span, "`from_residual` returned no value"))?;
            let ret_val = if matches!(self.cx.analysis.tcx.kind(r), TyKind::Union(_) | TyKind::Dynamic) {
                Some(self.box_value(Some(converted), target))
            } else {
                Some(converted)
            };
            self.emit_return(ret_val)?;

            self.switch(next);
        }

        // Success path: narrow the box to the success type.
        if matches!(self.cx.analysis.tcx.kind(success_ty), TyKind::Union(_) | TyKind::Dynamic) {
            Ok(Some(ptr))
        } else {
            match clty_of(self.cx.analysis, success_ty) {
                Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
                None => Ok(None),
            }
        }
    }

    fn h_if(
        &mut self,
        cond: &hir::Expr,
        then_block: &hir::Block,
        else_branch: Option<&hir::Expr>,
        result_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let c = self
            .h_expr(cond)?
            .ok_or_else(|| CodegenError::new(cond.span, "condition has no value"))?;
        let then_bb = self.b.create_block();
        let else_bb = self.b.create_block();
        let merge = self.b.create_block();
        let result_ct = self.cx_clty(result_ty);
        if let Some(ct) = result_ct {
            self.b.append_block_param(merge, ct);
        }
        self.b.ins().brif(c, then_bb, &[], else_bb, &[]);
        self.term = true;

        self.switch(then_bb);
        let then_val = self.h_block(then_block)?;
        self.jump_to_merge(merge, then_val, result_ct)?;

        self.switch(else_bb);
        let else_val = match else_branch {
            None => None,
            Some(e) => self.h_expr(e)?,
        };
        self.jump_to_merge(merge, else_val, result_ct)?;

        self.switch(merge);
        Ok(result_ct.map(|_| self.b.block_params(merge)[0]))
    }

    fn h_while(&mut self, cond: &hir::Expr, body: &hir::Block) -> CgResult<Option<Value>> {
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(header);
        self.emit_safepoint();
        let c = self
            .h_expr(cond)?
            .ok_or_else(|| CodegenError::new(cond.span, "loop condition has no value"))?;
        self.b.ins().brif(c, body_bb, &[], exit, &[]);
        self.term = true;
        self.switch(body_bb);
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(exit);
        Ok(None)
    }

    fn h_loop(&mut self, body: &hir::Block, result_ty: Ty) -> CgResult<Option<Value>> {
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        let result_ct = self.cx_clty(result_ty);
        if let Some(ct) = result_ct {
            self.b.append_block_param(exit, ct);
        }
        self.b.ins().jump(body_bb, &[]);
        self.term = true;
        self.switch(body_bb);
        self.emit_safepoint();
        self.loops.push(LoopCg {
            continue_block: body_bb,
            break_block: exit,
            has_value: result_ct.is_some(),
        });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(body_bb, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(exit);
        Ok(result_ct.map(|_| self.b.block_params(exit)[0]))
    }

    fn h_continue(&mut self, span: Span) -> CgResult<Option<Value>> {
        let cont = match self.loops.last() {
            Some(f) => f.continue_block,
            None => return Err(CodegenError::new(span, "`continue` outside a loop")),
        };
        self.b.ins().jump(cont, &[]);
        self.term = true;
        Ok(None)
    }

    fn h_break(&mut self, value: Option<&hir::Expr>, span: Span) -> CgResult<Option<Value>> {
        let (break_block, has_value) = match self.loops.last() {
            Some(f) => (f.break_block, f.has_value),
            None => return Err(CodegenError::new(span, "`break` outside a loop")),
        };
        if has_value {
            let v = match value {
                Some(e) => self.h_expr(e)?,
                None => None,
            };
            match v {
                Some(v) => self.b.ins().jump(break_block, &[v.into()]),
                None => {
                    let zero = self.b.ins().iconst(types::I64, 0);
                    self.b.ins().jump(break_block, &[zero.into()])
                }
            };
        } else {
            if let Some(e) = value {
                self.h_expr(e)?;
            }
            self.b.ins().jump(break_block, &[]);
        }
        self.term = true;
        Ok(None)
    }

    fn h_call(&mut self, kind: &hir::CallKind, args: &[hir::Expr], ty: Ty, span: Span)
        -> CgResult<Option<Value>>
    {
        match kind {
            hir::CallKind::Direct { def, type_args } => {
                let targs: Vec<Ty> = type_args
                    .iter()
                    .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                    .collect();
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(
                        self.h_expr(a)?
                            .ok_or_else(|| CodegenError::new(a.span, "argument has no value"))?,
                    );
                }
                self.emit_call(*def, targs, &arg_vals, span)
            }
            hir::CallKind::Builtin(b) => self.h_builtin_call(*b, args),
            hir::CallKind::Extern { def } => self.h_extern_call(*def, args, span),
            hir::CallKind::TupleCtor { def, .. } => self.h_tuple_ctor(*def, args, ty, span),
            hir::CallKind::BuiltinMethod { name } => self.h_builtin_method(name, args, ty),
            hir::CallKind::Closure { callee } => {
                let env = self
                    .h_expr(callee)?
                    .ok_or_else(|| CodegenError::new(callee.span, "closure value has no value"))?;
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(
                        self.h_expr(a)?
                            .ok_or_else(|| CodegenError::new(a.span, "argument has no value"))?,
                    );
                }
                let cty = resolve_shallow(self.cx.analysis, callee.ty, &self.subst);
                let ret = match self.cx.analysis.tcx.kind(cty) {
                    TyKind::Func { ret, .. } => *ret,
                    _ => self.cx.analysis.tcx.error,
                };
                let ret_clty = self.cx_clty(ret);
                Ok(self.emit_closure_call(env, &arg_vals, ret_clty))
            }
            hir::CallKind::Method { def, type_args, recv_static, is_static } => {
                self.h_method_call(*def, type_args, *recv_static, *is_static, args, ty, span)
            }
        }
    }

    /// A builtin `str`/`Map` method `recv.m(..)` — `args[0]` is the receiver.
    /// Dispatches by the receiver's type to the shared `emit_*_method` helpers.
    /// (List and channel/`Shared` methods stay on the AST path for now; the
    /// coverage predicate keeps those bodies off the HIR walk.)
    fn h_builtin_method(&mut self, name: &str, args: &[hir::Expr], ty: Ty) -> CgResult<Option<Value>> {
        let receiver = &args[0];
        let recv_span = receiver.span;
        let rty = receiver.ty;
        // Evaluate receiver, then the method arguments, into values.
        if matches!(self.cx.analysis.tcx.kind(rty), TyKind::Str) {
            let s = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(recv_span, "str receiver has no value"))?;
            let mut argv = Vec::with_capacity(args.len().saturating_sub(1));
            for a in &args[1..] {
                argv.push(self.h_expr(a)?);
            }
            return self.emit_str_method(s, name, &argv, recv_span);
        }
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(recv_span, "map has no value"))?;
            let mut argv = Vec::with_capacity(args.len().saturating_sub(1));
            for a in &args[1..] {
                argv.push(self.h_expr(a)?);
            }
            return self.emit_map_method(map, kt, vt, name, &argv, recv_span);
        }
        if let Some(elem) = self.list_elem_of(rty) {
            let list = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(recv_span, "list has no value"))?;
            let mut argv = Vec::with_capacity(args.len().saturating_sub(1));
            let mut arg_tys = Vec::with_capacity(args.len().saturating_sub(1));
            for a in &args[1..] {
                argv.push(self.h_expr(a)?);
                arg_tys.push(a.ty);
            }
            return self.emit_list_method(list, elem, name, &argv, &arg_tys, recv_span);
        }
        // Channel ends: `Sender.send(v)` / `Receiver.recv()`/`.try_recv()`.
        if let TyKind::Named { def, args: targs } = self.cx.analysis.tcx.kind(rty).clone() {
            let p = &self.cx.analysis.program;
            let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
            if def == p.sender_def && p.sender_def != DefId(0) {
                let ptr = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(recv_span, "channel receiver has no value"))?;
                let chan = self.emit_channel_id(ptr, rty, recv_span)?;
                let v = self.h_expr(&args[1])?;
                return self.emit_channel_send(chan, elem, v, recv_span);
            }
            if def == p.receiver_def && p.receiver_def != DefId(0) {
                let ptr = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(recv_span, "channel receiver has no value"))?;
                let chan = self.emit_channel_id(ptr, rty, recv_span)?;
                return self.emit_channel_recv(chan, elem, name, recv_span);
            }
            if def == p.shared_def && p.shared_def != DefId(0) {
                // `Shared<T>.lock(body)` / `.try_lock(body)`: read the mutex id,
                // build the closure env, then run it under the lock. `ty` is the
                // call result (`R` for lock, `R | LockBusy` for try_lock).
                let ptr = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(recv_span, "`Shared` receiver has no value"))?;
                let id = self.emit_shared_id(ptr, rty, recv_span)?;
                let r_ty = self.func_ret(args[1].ty);
                let env = self
                    .h_expr(&args[1])?
                    .ok_or_else(|| CodegenError::new(args[1].span, "lock body has no value"))?;
                return self.emit_shared_lock(id, elem, name, env, r_ty, ty, recv_span);
            }
        }
        Err(CodegenError::new(recv_span, "HIR codegen: builtin method on this type pending"))
    }

    /// `list[i]` / `map[k]` index load (mirrors the AST `gen_index_load`, List
    /// and Map cases; fixed FFI arrays stay on the AST path for now).
    /// The byte address of an aggregate field that holds a fixed-size FFI array
    /// (`extern struct` field / tuple element). Mirrors the AST
    /// `aggregate_field_addr`: the receiver's value is a base pointer and the
    /// field's offset is added. Only `Field`/`TupleIndex` places are valid.
    fn h_aggregate_field_addr(&mut self, place: &hir::Expr) -> CgResult<Value> {
        use hir::ExprKind as K;
        let (recv, idx): (&hir::Expr, usize) = match &place.kind {
            K::Field { receiver, field } => (receiver, field.index as usize),
            K::TupleIndex { receiver, index } => (receiver, *index as usize),
            _ => {
                return Err(CodegenError::new(
                    place.span,
                    "fixed-array access is only supported on an extern struct field",
                ))
            }
        };
        let layout = self
            .layout_for_ty(recv.ty)
            .ok_or_else(|| CodegenError::new(recv.span, "array-field receiver is not a struct"))?;
        let base = self
            .h_expr(recv)?
            .ok_or_else(|| CodegenError::new(recv.span, "receiver has no value"))?;
        let off = *layout
            .offsets
            .get(idx)
            .ok_or_else(|| CodegenError::new(place.span, "unknown array field"))?;
        Ok(self.b.ins().iadd_imm(base, off as i64))
    }

    /// `&expr` — address-of (`docs/19`). `&arr[i]` computes the address of a
    /// fixed-array element; otherwise an `extern struct` place whose value is
    /// already its address.
    fn h_ref(&mut self, inner: &hir::Expr) -> CgResult<Option<Value>> {
        if let hir::ExprKind::Index { receiver, index } = &inner.kind {
            if matches!(self.cx.analysis.tcx.kind(receiver.ty), TyKind::Array { .. }) {
                let ct = self.array_elem_clty(receiver.ty).ok_or_else(|| {
                    CodegenError::new(receiver.span, "array element has no scalar type")
                })?;
                let base = self.h_aggregate_field_addr(receiver)?;
                let idx = self
                    .h_expr(index)?
                    .ok_or_else(|| CodegenError::new(index.span, "index has no value"))?;
                let scaled = self.b.ins().imul_imm(idx, ct.bytes() as i64);
                return Ok(Some(self.b.ins().iadd(base, scaled)));
            }
        }
        let v = self
            .h_expr(inner)?
            .ok_or_else(|| CodegenError::new(inner.span, "address-of operand has no value"))?;
        Ok(Some(v))
    }

    /// `*ptr` — pointer dereference (`docs/19` §2). Panics on null; for a pointer
    /// to an `extern struct` the value is the same address; otherwise loads the
    /// pointee scalar. `result_ty` is the pointee type (the node's `ty`).
    fn h_deref(&mut self, inner: &hir::Expr, result_ty: Ty) -> CgResult<Option<Value>> {
        let p = self
            .h_expr(inner)?
            .ok_or_else(|| CodegenError::new(inner.span, "dereference operand has no value"))?;
        let zero = self.b.ins().iconst(PTR, 0);
        let is_null = self.b.ins().icmp(IntCC::Equal, p, zero);
        self.guard_panic(is_null, "dereference of a null pointer");
        let rty = resolve_shallow(self.cx.analysis, result_ty, &self.subst);
        if let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(rty) {
            if self.is_extern_struct_def(*def) {
                return Ok(Some(p));
            }
        }
        match clty_of(self.cx.analysis, rty) {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), p, 0))),
            None => Ok(None),
        }
    }

    fn h_index_load(&mut self, receiver: &hir::Expr, index: &hir::Expr) -> CgResult<Option<Value>> {
        let rty = receiver.ty;
        // A fixed-size FFI array `[T; N]` field — `arr[i]` loads element `T`.
        if matches!(self.cx.analysis.tcx.kind(rty), TyKind::Array { .. }) {
            let ct = self
                .array_elem_clty(rty)
                .ok_or_else(|| CodegenError::new(receiver.span, "array element has no scalar type"))?;
            let base = self.h_aggregate_field_addr(receiver)?;
            let idx = self
                .h_expr(index)?
                .ok_or_else(|| CodegenError::new(index.span, "index has no value"))?;
            let scaled = self.b.ins().imul_imm(idx, ct.bytes() as i64);
            let addr = self.b.ins().iadd(base, scaled);
            return Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), addr, 0)));
        }
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(receiver.span, "map has no value"))?;
            let kv = self.h_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self
                .call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key])
                .expect("map_index returns a value");
            return self.i64_to_elem(raw, vt, receiver.span);
        }
        let elem = self
            .list_elem_of(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "HIR codegen: indexing only on List/Map"))?;
        let list = self
            .h_expr(receiver)?
            .ok_or_else(|| CodegenError::new(receiver.span, "list has no value"))?;
        let idx = self
            .h_expr(index)?
            .ok_or_else(|| CodegenError::new(index.span, "index has no value"))?;
        let raw = self
            .call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, idx])
            .expect("list_get returns a value");
        self.i64_to_elem(raw, elem, receiver.span)
    }

    /// `list[i] = v` / `map[k] = v` index store (mirrors the AST `gen_index_store`).
    fn h_index_store(&mut self, receiver: &hir::Expr, index: &hir::Expr, val: Option<Value>)
        -> CgResult<()>
    {
        let rty = receiver.ty;
        // A fixed-size FFI array `[T; N]` field — `arr[i] = v` stores element `T`.
        if matches!(self.cx.analysis.tcx.kind(rty), TyKind::Array { .. }) {
            let ct = self
                .array_elem_clty(rty)
                .ok_or_else(|| CodegenError::new(receiver.span, "array element has no scalar type"))?;
            let base = self.h_aggregate_field_addr(receiver)?;
            let idx = self
                .h_expr(index)?
                .ok_or_else(|| CodegenError::new(index.span, "index has no value"))?;
            let scaled = self.b.ins().imul_imm(idx, ct.bytes() as i64);
            let addr = self.b.ins().iadd(base, scaled);
            if let Some(v) = val {
                self.b.ins().store(MemFlags::trusted(), v, addr, 0);
            }
            return Ok(());
        }
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(receiver.span, "map has no value"))?;
            let kv = self.h_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self.elem_to_i64(val, vt, receiver.span)?;
            self.call_intrinsic(
                "lang_map_set",
                &[PTR, types::I64, types::I64],
                None,
                &[map, key, raw],
            );
            return Ok(());
        }
        let elem = self
            .list_elem_of(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "HIR codegen: indexed store only on List/Map"))?;
        let list = self
            .h_expr(receiver)?
            .ok_or_else(|| CodegenError::new(receiver.span, "list has no value"))?;
        let idx = self
            .h_expr(index)?
            .ok_or_else(|| CodegenError::new(index.span, "index has no value"))?;
        let raw = self.elem_to_i64(val, elem, receiver.span)?;
        self.call_intrinsic(
            "lang_list_set",
            &[PTR, types::I64, types::I64],
            None,
            &[list, idx, raw],
        );
        Ok(())
    }

    /// `[a, b, …]` list literal (mirrors the AST `ExprKind::List` arm).
    fn h_list(&mut self, elems: &[hir::Expr], ty: Ty) -> CgResult<Option<Value>> {
        let elem = self
            .list_elem_of(ty)
            .ok_or_else(|| CodegenError::new(Span::dummy(), "list literal has non-list type"))?;
        let list = self.gen_list_new(elem);
        self.mark_root(list);
        for el in elems {
            let v = self.h_expr(el)?;
            let raw = self.elem_to_i64(v, elem, el.span)?;
            self.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[list, raw]);
        }
        Ok(Some(list))
    }

    /// `{ k: v, …, ..base }` map literal (mirrors the AST `gen_map_lit`).
    fn h_map(&mut self, items: &[hir::MapEntry], ty: Ty, span: Span) -> CgResult<Option<Value>> {
        let (kt, vt) = self
            .map_kv_of(ty)
            .ok_or_else(|| CodegenError::new(span, "map literal has non-map type"))?;
        let map = self.gen_map_new(kt, vt);
        self.mark_root(map);
        for item in items {
            match item {
                hir::MapEntry::Kv { key, value } => {
                    let kv = self.h_expr(key)?;
                    let k = self.elem_to_i64(kv, kt, key.span)?;
                    let vv = self.h_expr(value)?;
                    let v = self.elem_to_i64(vv, vt, value.span)?;
                    self.call_intrinsic(
                        "lang_map_set",
                        &[PTR, types::I64, types::I64],
                        None,
                        &[map, k, v],
                    );
                }
                hir::MapEntry::Spread(base) => {
                    let src = self
                        .h_expr(base)?
                        .ok_or_else(|| CodegenError::new(base.span, "map spread source has no value"))?;
                    self.call_intrinsic("lang_map_extend", &[PTR, PTR], None, &[map, src]);
                }
            }
        }
        Ok(Some(map))
    }

    /// Construct a tuple struct from positional arguments (mirrors the AST
    /// `gen_tuple_ctor`): a `@Transparent` newtype is its single field's value;
    /// otherwise allocate the field block (laid out for the result type's
    /// inferred generics) and store each argument.
    fn h_tuple_ctor(&mut self, def: DefId, args: &[hir::Expr], ty: Ty, span: Span)
        -> CgResult<Option<Value>>
    {
        if transparent_inner(self.cx.analysis, ty).is_some() {
            return self.h_expr(&args[0]);
        }
        let layout = self.layout_for_ty(ty).unwrap_or_else(|| self.struct_layout(def, &[]));
        let ptr = if self.is_extern_struct_def(def) {
            self.alloc_extern(&layout)
        } else {
            self.alloc_struct(&layout)
        };
        for (i, a) in args.iter().enumerate() {
            let off = *layout.offsets.get(i).unwrap_or(&0) as i32;
            let v = self.h_expr(a)?;
            if let (Some(v), Some(Some(_))) = (v, layout.cltys.get(i)) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, off);
            }
        }
        let _ = span;
        Ok(Some(ptr))
    }

    /// A record/tuple struct literal (mirrors the AST `gen_struct_lit`).
    fn h_struct_lit(
        &mut self,
        def: DefId,
        type_args: &[Ty],
        fields: &[hir::FieldInit],
        spread: Option<&hir::Expr>,
        sty: Ty,
        span: Span,
    ) -> CgResult<Value> {
        let layout = self.struct_layout(def, type_args);
        let ptr = if self.is_extern_struct_def(def) {
            self.alloc_extern(&layout)
        } else {
            self.alloc_struct_typed(&layout, sty)
        };
        // A spread base fills every field first; explicit fields override.
        if let Some(base) = spread {
            let base_ptr = self
                .h_expr(base)?
                .ok_or_else(|| CodegenError::new(base.span, "spread base has no value"))?;
            for i in 0..layout.offsets.len() {
                if let Some(ct) = layout.cltys[i] {
                    let off = layout.offsets[i] as i32;
                    let v = self.b.ins().load(ct, MemFlags::trusted(), base_ptr, off);
                    self.b.ins().store(MemFlags::trusted(), v, ptr, off);
                }
            }
        }
        for fi in fields {
            let idx = fi.index as usize;
            if idx >= layout.offsets.len() {
                return Err(CodegenError::new(fi.span, "unknown field in struct literal"));
            }
            let off = layout.offsets[idx] as i32;
            let val = self.h_expr(&fi.value)?;
            if is_extern_struct_ty(self.cx.analysis, layout.tys[idx]) {
                if let Some(v) = val {
                    let n = self.sizeof_ty(layout.tys[idx]);
                    self.copy_bytes(ptr, off, v, n);
                }
            } else if let (Some(v), Some(_)) = (val, layout.cltys[idx]) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, off);
            }
        }
        let _ = span;
        Ok(ptr)
    }

    /// Load a field by position (record field index or tuple position), mirroring
    /// the AST `gen_field_load`.
    fn h_field_at(&mut self, receiver: &hir::Expr, idx: usize) -> CgResult<Option<Value>> {
        let rty = receiver.ty;
        // A `@Transparent` newtype's value *is* its single field.
        if transparent_inner(self.cx.analysis, resolve_shallow(self.cx.analysis, rty, &self.subst))
            .is_some()
        {
            return self.h_expr(receiver);
        }
        let layout = self
            .layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "field access on non-aggregate"))?;
        let ptr = self
            .h_expr(receiver)?
            .ok_or_else(|| CodegenError::new(receiver.span, "receiver has no value"))?;
        let off = *layout
            .offsets
            .get(idx)
            .ok_or_else(|| CodegenError::new(receiver.span, "field index out of range"))?
            as i32;
        // A nested extern struct is laid out inline; its value is the address.
        if is_extern_struct_ty(self.cx.analysis, layout.tys[idx]) {
            return Ok(Some(self.b.ins().iadd_imm(ptr, off as i64)));
        }
        match layout.cltys[idx] {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, off))),
            None => Ok(None),
        }
    }

    fn h_builtin_call(&mut self, b: Builtin, args: &[hir::Expr]) -> CgResult<Option<Value>> {
        match b {
            Builtin::Print | Builtin::Println => {
                let arg = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "builtin argument has no value"))?;
                let name = if matches!(b, Builtin::Print) { "lang_print" } else { "lang_println" };
                self.call_intrinsic(name, &[PTR], None, &[arg]);
                Ok(None)
            }
            Builtin::Panic => {
                let msg = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "panic message has no value"))?;
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::PanicWith => {
                let _ = self.h_expr(&args[0])?;
                let msg = self.const_str("explicit panic (panic_with)");
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::Exit => {
                let code = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "exit code has no value"))?;
                self.call_intrinsic("lang_exit", &[types::I32], None, &[code]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::Abort => {
                self.call_intrinsic("lang_abort", &[], None, &[]);
                self.emit_unreachable();
                Ok(None)
            }
        }
    }

    fn h_extern_call(&mut self, def: DefId, args: &[hir::Expr], span: Span)
        -> CgResult<Option<Value>>
    {
        let esig = self
            .cx
            .hir
            .extern_sigs
            .get(&def)
            .ok_or_else(|| CodegenError::new(span, "extern signature not recorded"))?;
        let (ptys, rty) = (esig.params.clone(), esig.ret);
        let mut sig = self.module.make_signature();
        for pt in &ptys {
            let ct = clty_of(self.cx.analysis, *pt)
                .ok_or_else(|| CodegenError::new(span, "extern parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        if let Some(rc) = clty_of(self.cx.analysis, rty) {
            sig.returns.push(AbiParam::new(rc));
        }
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            arg_vals.push(
                self.h_expr(a)?
                    .ok_or_else(|| CodegenError::new(a.span, "extern argument has no value"))?,
            );
        }
        let name = self.cx.analysis.program.def(def).name.clone();
        let id = self
            .module
            .declare_function(&name, Linkage::Import, &sig)
            .map_err(|e| CodegenError::new(span, format!("declare extern `{name}`: {e}")))?;
        let fref = self.module.declare_func_in_func(id, self.b.func);
        let inst = self.b.ins().call(fref, &arg_vals);
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// An anonymous tuple value (mirrors the AST `ExprKind::Tuple` arm).
    fn h_tuple(&mut self, elems: &[hir::Expr], ty: Ty) -> CgResult<Option<Value>> {
        let elem_tys = match self.cx.analysis.tcx.kind(ty).clone() {
            TyKind::Tuple(ts) => ts,
            _ => return Err(CodegenError::new(Span::dummy(), "tuple has non-tuple type")),
        };
        let layout = tuple_layout(self.cx.analysis, &elem_tys);
        let ptr = self.alloc_struct(&layout);
        for (i, e) in elems.iter().enumerate() {
            let v = self.h_expr(e)?;
            if let (Some(v), Some(Some(_))) = (v, layout.cltys.get(i)) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, layout.offsets[i] as i32);
            }
        }
        Ok(Some(ptr))
    }

    /// A (possibly interpolated) string literal (mirrors the AST
    /// `gen_str_literal`): each part becomes one `str`, concatenated left to
    /// right. Parts are GC-rooted across the remaining allocations.
    fn h_str(&mut self, parts: &[hir::StrPart]) -> CgResult<Value> {
        let mut vals: Vec<Value> = Vec::new();
        for part in parts {
            match part {
                hir::StrPart::Text(text) => {
                    let mut bytes = Vec::new();
                    unescape_into(text, &mut bytes);
                    vals.push(self.emit_str_bytes(bytes));
                }
                hir::StrPart::Interp { expr, stringify, stringify_targs } => {
                    let v = self.h_expr(expr)?;
                    vals.push(self.h_stringify(v, expr.ty, *stringify, stringify_targs, expr.span)?);
                }
            }
        }
        if vals.is_empty() {
            return Ok(self.const_str(""));
        }
        for &p in &vals {
            self.mark_root(p);
        }
        let mut acc = vals[0];
        for &p in &vals[1..] {
            acc = self
                .call_intrinsic("lang_str_concat", &[PTR, PTR], Some(PTR), &[acc, p])
                .expect("concat returns a value");
            self.mark_root(acc);
        }
        Ok(acc)
    }

    /// Convert an interpolation hole's value to a `str`. The HIR carries the
    /// resolved `to_str` method (if the hole is a user type) on the node; only
    /// builtin-typed holes are covered here so far (the coverage predicate keeps
    /// user-`to_str` holes on the AST path until their type-args ride the node).
    fn h_stringify(
        &mut self,
        v: Option<Value>,
        ty: Ty,
        stringify: Option<DefId>,
        stringify_targs: &[Ty],
        span: Span,
    ) -> CgResult<Value> {
        // A user type with a `to_str(self): str` method — call it with the value
        // as the receiver, monomorphized by the recorded type args.
        if let Some(mdef) = stringify {
            let recv = v.ok_or_else(|| CodegenError::new(span, "interpolated value has no payload"))?;
            let targs: Vec<Ty> = stringify_targs
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect();
            return self
                .emit_call(mdef, targs, &[recv], span)?
                .ok_or_else(|| CodegenError::new(span, "`to_str` returned no value"));
        }
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Str => v.ok_or_else(|| CodegenError::new(span, "str has no value")),
            TyKind::Null => Ok(self.const_str("null")),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char => {
                let v = v.ok_or_else(|| CodegenError::new(span, "value has no payload"))?;
                self.cast_to_str(v, ty, span)
            }
            _ => Err(CodegenError::new(span, "type is not stringifiable")),
        }
    }

    // -- match ---------------------------------------------------------------

    fn h_match(&mut self, scrutinee: &hir::Expr, arms: &[hir::MatchArm], result_ty: Ty)
        -> CgResult<Option<Value>>
    {
        let sty = scrutinee.ty;
        let scrut = self.h_expr(scrutinee)?;
        let is_union = matches!(self.cx.analysis.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic);
        let tag = if is_union {
            scrut.map(|p| self.b.ins().load(types::I64, MemFlags::trusted(), p, 0))
        } else {
            None
        };
        let result_ct = self.cx_clty(result_ty);
        let merge = self.b.create_block();
        if let Some(ct) = result_ct {
            self.b.append_block_param(merge, ct);
        }
        for arm in arms {
            let matched = self.h_pattern_matches(&arm.pattern, sty, scrut, tag)?;
            let cand = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(matched, cand, &[], next, &[]);
            self.term = true;
            self.switch(cand);
            self.h_bind_match_pattern(&arm.pattern, sty, scrut, tag)?;
            let proceed = match &arm.guard {
                Some(g) => self
                    .h_expr(g)?
                    .ok_or_else(|| CodegenError::new(g.span, "guard has no value"))?,
                None => self.b.ins().iconst(types::I8, 1),
            };
            let body_block = self.b.create_block();
            self.b.ins().brif(proceed, body_block, &[], next, &[]);
            self.term = true;
            self.switch(body_block);
            let body_val = self.h_expr(&arm.body)?;
            self.jump_to_merge(merge, body_val, result_ct)?;
            self.switch(next);
        }
        // Exhaustiveness is checked statically; reaching here is impossible.
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;
        self.switch(merge);
        Ok(result_ct.map(|_| self.b.block_params(merge)[0]))
    }

    fn h_pattern_matches(&mut self, pattern: &hir::Pattern, sty: Ty, scrut: Option<Value>, tag: Option<Value>)
        -> CgResult<Value>
    {
        use hir::PatternKind as P;
        let one = self.b.ins().iconst(types::I8, 1);
        match &pattern.kind {
            P::Wildcard | P::Bind(_) | P::Tuple { .. } => Ok(one),
            // A type-test (`T x`) / unit-variant pattern: in a union the tag is
            // checked at runtime; for a concrete scrutinee it is statically known.
            P::TypeBind { test_ty, .. } | P::UnitPath { test_ty, .. } => match tag {
                Some(tag) => Ok(self.tag_in_target(tag, *test_ty)),
                None => {
                    let yes = sty == *test_ty;
                    Ok(self.b.ins().iconst(types::I8, i64::from(yes)))
                }
            },
            P::Literal(e) => {
                if let hir::ExprKind::Null = &e.kind {
                    return match tag {
                        Some(tag) => {
                            let nid = self.b.ins().iconst(
                                types::I64,
                                type_id(self.cx.analysis, self.cx.analysis.tcx.null),
                            );
                            Ok(self.b.ins().icmp(IntCC::Equal, tag, nid))
                        }
                        None => Ok(one),
                    };
                }
                let lit = self
                    .h_expr(e)?
                    .ok_or_else(|| CodegenError::new(e.span, "literal pattern has no value"))?;
                let scrut =
                    scrut.ok_or_else(|| CodegenError::new(pattern.span, "scrutinee has no value"))?;
                match self.cx.analysis.tcx.kind(sty) {
                    TyKind::Float(_) => Ok(self.b.ins().fcmp(FloatCC::Equal, scrut, lit)),
                    _ => Ok(self.b.ins().icmp(IntCC::Equal, scrut, lit)),
                }
            }
            // `A | B | C` — match if any alternative matches.
            P::Or(alts) => {
                let mut acc = self.b.ins().iconst(types::I8, 0);
                for alt in alts {
                    let m = self.h_pattern_matches(alt, sty, scrut, tag)?;
                    acc = self.b.ins().bor(acc, m);
                }
                Ok(acc)
            }
            // `[a, b]` / `[head, ..tail]` — a length test on the list.
            P::List { elems, rest } => {
                let lv = scrut
                    .ok_or_else(|| CodegenError::new(pattern.span, "list scrutinee has no value"))?;
                let n = self
                    .call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[lv])
                    .expect("list size");
                let fixed = self.b.ins().iconst(types::I64, elems.len() as i64);
                let cc = if rest.is_some() {
                    IntCC::SignedGreaterThanOrEqual // `[a, ..t]` needs at least the fixed count
                } else {
                    IntCC::Equal
                };
                Ok(self.b.ins().icmp(cc, n, fixed))
            }
            // A struct variant pattern: in a union the box tag is checked
            // against the struct's type id; for a concrete scrutinee it is known.
            P::TupleStruct { def, .. } | P::RecordStruct { def, .. } => {
                let vt = self.struct_variant_ty(sty, *def);
                match tag {
                    Some(tag) => Ok(self.tag_in_target(tag, vt)),
                    None => {
                        let yes = sty == vt;
                        Ok(self.b.ins().iconst(types::I8, i64::from(yes)))
                    }
                }
            }
        }
    }

    /// The matched variant type of a struct pattern given the scrutinee `sty`:
    /// the union member with `def` (carrying concrete args), else the bare type.
    fn struct_variant_ty(&self, sty: Ty, def: DefId) -> Ty {
        for v in self.cx.analysis.tcx.variants(sty) {
            if let TyKind::Named { def: vd, .. } = self.cx.analysis.tcx.kind(v) {
                if *vd == def {
                    return v;
                }
            }
        }
        // The checker validated that `def` is `sty` or one of its variants, so
        // a concrete scrutinee is already the struct type itself.
        let _ = def;
        sty
    }

    fn h_bind_match_pattern(&mut self, pattern: &hir::Pattern, sty: Ty, scrut: Option<Value>, _tag: Option<Value>)
        -> CgResult<()>
    {
        use hir::PatternKind as P;
        match &pattern.kind {
            P::Wildcard | P::Literal(_) | P::UnitPath { .. } => Ok(()),
            P::Bind(local) => {
                if let (Some(v), Some(ct)) = (scrut, self.cx_clty(sty)) {
                    self.bind_local(*local, ct, v);
                }
                Ok(())
            }
            P::TypeBind { test_ty, bind: Some(local) } => {
                let t = *test_ty;
                // Extract the payload: unbox from a union, else use as-is.
                let val = match (scrut, self.cx_clty(t)) {
                    (Some(p), Some(ct))
                        if matches!(
                            self.cx.analysis.tcx.kind(sty),
                            TyKind::Union(_) | TyKind::Dynamic
                        ) =>
                    {
                        Some(self.b.ins().load(ct, MemFlags::trusted(), p, 8))
                    }
                    (s, Some(_)) => s,
                    _ => None,
                };
                if let (Some(v), Some(ct)) = (val, self.cx_clty(t)) {
                    self.bind_local(*local, ct, v);
                }
                Ok(())
            }
            P::TypeBind { bind: None, .. } => Ok(()),
            P::Tuple { elems, rest: None } => {
                let layout = self
                    .layout_for_ty(sty)
                    .ok_or_else(|| CodegenError::new(pattern.span, "tuple pattern on non-aggregate"))?;
                let elem_tys = match self.cx.analysis.tcx.kind(sty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                let ptr = scrut
                    .ok_or_else(|| CodegenError::new(pattern.span, "tuple scrutinee has no value"))?;
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => {
                            Some(self.b.ins().load(*ct, MemFlags::trusted(), ptr, layout.offsets[i] as i32))
                        }
                        _ => None,
                    };
                    self.h_bind_match_pattern(sub, elem_tys[i], elem_val, None)?;
                }
                Ok(())
            }
            // Or-patterns bind nothing (the checker rejects binding alternatives).
            P::Or(_) => Ok(()),
            // `[a, b]` / `[head, ..tail]`: bind the leading/trailing elements and
            // the `..rest` as a fresh sub-list.
            P::List { elems, rest } => {
                let lv = scrut
                    .ok_or_else(|| CodegenError::new(pattern.span, "list scrutinee has no value"))?;
                let elem = self
                    .list_elem_of(sty)
                    .ok_or_else(|| CodegenError::new(pattern.span, "list pattern on non-list"))?;
                let load_at = |this: &mut Self, idx: Value| -> CgResult<Option<Value>> {
                    let raw = this
                        .call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[lv, idx])
                        .expect("list get");
                    this.i64_to_elem(raw, elem, pattern.span)
                };
                match rest {
                    None => {
                        for (i, sub) in elems.iter().enumerate() {
                            let idx = self.b.ins().iconst(types::I64, i as i64);
                            let v = load_at(self, idx)?;
                            self.h_bind_pattern(sub, v, elem)?;
                        }
                    }
                    Some((rp, r)) => {
                        let rp = *rp;
                        let trailing = elems.len() - rp;
                        let n = self
                            .call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[lv])
                            .expect("list size");
                        // Leading fixed elements at 0..rp.
                        for (i, sub) in elems.iter().take(rp).enumerate() {
                            let idx = self.b.ins().iconst(types::I64, i as i64);
                            let v = load_at(self, idx)?;
                            self.h_bind_pattern(sub, v, elem)?;
                        }
                        // Trailing fixed elements at n - trailing + j.
                        let base = self.b.ins().iadd_imm(n, -(trailing as i64));
                        for (j, sub) in elems.iter().skip(rp).enumerate() {
                            let idx = self.b.ins().iadd_imm(base, j as i64);
                            let v = load_at(self, idx)?;
                            self.h_bind_pattern(sub, v, elem)?;
                        }
                        // `..rest` binds the middle slice [rp, n - trailing).
                        if let Some(local) = r.bind {
                            let start = self.b.ins().iconst(types::I64, rp as i64);
                            let end = self.b.ins().iadd_imm(n, -(trailing as i64));
                            let sub = self
                                .call_intrinsic("lang_list_slice", &[PTR, types::I64, types::I64], Some(PTR), &[lv, start, end])
                                .expect("list slice");
                            self.bind_local(local, PTR, sub);
                        }
                    }
                }
                Ok(())
            }
            // Struct variant binding: unbox the payload pointer (load `[8]` from
            // a union box, else use the scrutinee directly) and bind the fields
            // through the irrefutable struct-destructuring path.
            P::TupleStruct { def, .. } | P::RecordStruct { def, .. } => {
                let vt = self.struct_variant_ty(sty, *def);
                let payload = if matches!(
                    self.cx.analysis.tcx.kind(sty),
                    TyKind::Union(_) | TyKind::Dynamic
                ) {
                    scrut.map(|p| self.b.ins().load(PTR, MemFlags::trusted(), p, 8))
                } else {
                    scrut
                };
                self.h_bind_pattern(pattern, payload, vt)
            }
            _ => Err(CodegenError::new(pattern.span, "HIR codegen: pattern not yet supported in match")),
        }
    }

    // -- for -----------------------------------------------------------------

    fn h_for(&mut self, pattern: &hir::Pattern, iter: &hir::Expr, body: &hir::Block, driver: &hir::ForDriver)
        -> CgResult<Option<Value>>
    {
        match driver {
            hir::ForDriver::Iter(info) => self.h_for_iterator(pattern, iter, body, info),
            hir::ForDriver::Map { key, value, entry } => {
                self.h_for_map(pattern, iter, body, *key, *value, *entry)
            }
            hir::ForDriver::ListFast { elem } => self.h_for_list(pattern, iter, body, *elem),
            hir::ForDriver::AsyncIter(info) => self.h_for_async(pattern, iter, body, info),
            hir::ForDriver::StrChars => self.h_for_str_chars(pattern, iter, body),
        }
    }

    /// `for ch in s` over a `str` (`docs/18` §4): snapshot the Unicode scalars
    /// into a `List<char>` (`lang_str_to_chars`) and index-loop them — exactly
    /// the desugaring `for ch in s.chars()` with no intermediate iterator
    /// struct. The snapshot is rooted across the loop's safepoints.
    fn h_for_str_chars(&mut self, pattern: &hir::Pattern, iter: &hir::Expr, body: &hir::Block)
        -> CgResult<Option<Value>>
    {
        let s = self
            .h_expr(iter)?
            .ok_or_else(|| CodegenError::new(iter.span, "string has no value"))?;
        let list = self
            .call_intrinsic("lang_str_to_chars", &[PTR], Some(PTR), &[s])
            .expect("str chars snapshot returns a list");
        self.mark_root(list);
        let elem = self.cx.analysis.tcx.char;
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let latch = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self
            .call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;
        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let raw = self
            .call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, i2])
            .expect("get");
        let elem_val = self.i64_to_elem(raw, elem, iter.span)?;
        self.h_bind_pattern(pattern, elem_val, elem)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(latch, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(latch);
        let i3 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(exit);
        Ok(None)
    }

    /// `for await x in stream { body }` (`docs/21` §10): each iteration awaits
    /// `stream.next_async()` (a suspend site), breaks on `Done`, and binds the
    /// unwrapped `Item<T>.value`. Mirrors the AST `gen_for_await`; only valid
    /// inside an async `poll` body, and the stream must be a simple variable so
    /// re-loading it across suspends is correct.
    fn h_for_async(
        &mut self,
        pattern: &hir::Pattern,
        iter: &hir::Expr,
        body: &hir::Block,
        info: &hir::ForAsyncIter,
    ) -> CgResult<Option<Value>> {
        if !matches!(&iter.kind, hir::ExprKind::Name(hir::Res::Local(_))) {
            return Err(CodegenError::new(
                iter.span,
                "`for await` currently requires the stream to be a variable — \
                 bind it with `var s = …;` first",
            ));
        }

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;

        // header: fut = stream.next_async(); await it (suspends until ready).
        self.switch(header);
        let iter_val = self
            .h_expr(iter)?
            .ok_or_else(|| CodegenError::new(iter.span, "stream has no value"))?;
        // Resolve `next_async`: an interface-object stream dispatches through the
        // vtable; a concrete `extend … : AsyncIterator` resolves to the impl
        // (mirrors the synchronous `h_for_iterator`).
        let fut = if self.cx.analysis.program.def(info.next_async).kind == DefKind::InterfaceMethod {
            let recv = resolve_shallow(self.cx.analysis, info.iter_ty, &self.subst);
            if self.is_interface_ty(recv) {
                let slot = self
                    .vtable_slot(info.next_async)
                    .ok_or_else(|| CodegenError::new(iter.span, "`next_async` not in interface"))?;
                self.emit_vtable_call(slot, iter_val, &[], Some(PTR))?
            } else {
                let (target, targs) = self
                    .resolve_iface_method(info.next_async, recv)
                    .ok_or_else(|| CodegenError::new(iter.span, "cannot resolve `next_async`"))?;
                self.emit_call(target, targs, &[iter_val], iter.span)?
            }
        } else {
            let next_targs: Vec<Ty> = info
                .next_targs
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect();
            self.emit_call(info.next_async, next_targs, &[iter_val], iter.span)?
        }
        .ok_or_else(|| CodegenError::new(iter.span, "`next_async` returned no value"))?;
        let u = self
            .emit_await_suspend(fut, iter.span, info.union_ty)?
            .ok_or_else(|| CodegenError::new(iter.span, "awaited `next_async` has no value"))?;
        self.mark_root(u);
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), u, 0);
        let done_id = self.type_id_of(info.done_ty);
        let done_c = self.b.ins().iconst(types::I64, done_id);
        let is_done = self.b.ins().icmp(IntCC::Equal, tag, done_c);
        self.b.ins().brif(is_done, exit, &[], body_bb, &[]);
        self.term = true;

        // body: unwrap `Item<T>.value` and bind the loop variable.
        self.switch(body_bb);
        let item_ptr = self.b.ins().load(PTR, MemFlags::trusted(), u, 8);
        let layout = self
            .layout_for_ty(info.item_ty)
            .ok_or_else(|| CodegenError::new(iter.span, "`Item<T>` has no layout"))?;
        let idx = layout
            .index_of("value")
            .ok_or_else(|| CodegenError::new(iter.span, "`Item<T>` has no `value` field"))?;
        let off = layout.offsets[idx] as i32;
        let value = match layout.cltys[idx] {
            Some(ct) => {
                let v = self.b.ins().load(ct, MemFlags::trusted(), item_ptr, off);
                let resolved = resolve_shallow(self.cx.analysis, info.elem, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    self.mark_root(v);
                }
                Some(v)
            }
            None => None,
        };
        self.h_bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    fn h_for_list(&mut self, pattern: &hir::Pattern, iter: &hir::Expr, body: &hir::Block, elem: Ty)
        -> CgResult<Option<Value>>
    {
        let list = self
            .h_expr(iter)?
            .ok_or_else(|| CodegenError::new(iter.span, "iterable has no value"))?;
        self.mark_root(list);
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let latch = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self
            .call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;
        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let raw = self
            .call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, i2])
            .expect("get");
        let elem_val = self.i64_to_elem(raw, elem, iter.span)?;
        self.h_bind_pattern(pattern, elem_val, elem)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(latch, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(latch);
        let i3 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(exit);
        Ok(None)
    }

    fn h_for_iterator(&mut self, pattern: &hir::Pattern, iter: &hir::Expr, body: &hir::Block, info: &ForIter)
        -> CgResult<Option<Value>>
    {
        let iter_val = self
            .h_expr(iter)?
            .ok_or_else(|| CodegenError::new(iter.span, "iterator has no value"))?;
        self.mark_root(iter_val);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(header);
        self.emit_safepoint();
        let u = if self.cx.analysis.program.def(info.next).kind == DefKind::InterfaceMethod {
            let recv = resolve_shallow(self.cx.analysis, info.iter_ty, &self.subst);
            if self.is_interface_ty(recv) {
                let slot = self
                    .vtable_slot(info.next)
                    .ok_or_else(|| CodegenError::new(iter.span, "iterator method not in interface"))?;
                self.emit_vtable_call(slot, iter_val, &[], Some(PTR))?
            } else {
                let (target, targs) = self
                    .resolve_iface_method(info.next, recv)
                    .ok_or_else(|| CodegenError::new(iter.span, "cannot resolve iterator `next`"))?;
                self.emit_call(target, targs, &[iter_val], iter.span)?
            }
        } else {
            let next_targs: Vec<Ty> = info
                .next_targs
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect();
            self.emit_call(info.next, next_targs, &[iter_val], iter.span)?
        }
        .ok_or_else(|| CodegenError::new(iter.span, "`next` returned no value"))?;
        self.mark_root(u);
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), u, 0);
        let done_id = self.type_id_of(info.done_ty);
        let done_c = self.b.ins().iconst(types::I64, done_id);
        let is_done = self.b.ins().icmp(IntCC::Equal, tag, done_c);
        self.b.ins().brif(is_done, exit, &[], body_bb, &[]);
        self.term = true;
        self.switch(body_bb);
        let item_ptr = self.b.ins().load(PTR, MemFlags::trusted(), u, 8);
        let layout = self
            .layout_for_ty(info.item_ty)
            .ok_or_else(|| CodegenError::new(iter.span, "`Item<T>` has no layout"))?;
        let idx = layout
            .index_of("value")
            .ok_or_else(|| CodegenError::new(iter.span, "`Item<T>` has no `value` field"))?;
        let off = layout.offsets[idx] as i32;
        let value = match layout.cltys[idx] {
            Some(ct) => {
                let v = self.b.ins().load(ct, MemFlags::trusted(), item_ptr, off);
                let resolved = resolve_shallow(self.cx.analysis, info.elem, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    self.mark_root(v);
                }
                Some(v)
            }
            None => None,
        };
        self.h_bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(exit);
        Ok(None)
    }

    fn h_for_map(
        &mut self,
        pattern: &hir::Pattern,
        iter: &hir::Expr,
        body: &hir::Block,
        kt: Ty,
        vt: Ty,
        entry_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let map = self
            .h_expr(iter)?
            .ok_or_else(|| CodegenError::new(iter.span, "map has no value"))?;
        self.mark_root(map);
        let one = self.b.ins().iconst(types::I64, 1);
        let keys = self
            .call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, one])
            .expect("map_entries returns a list");
        self.mark_root(keys);
        let layout = self.struct_layout(
            match self
                .cx
                .analysis
                .tcx
                .kind(resolve_shallow(self.cx.analysis, entry_ty, &self.subst))
                .clone()
            {
                TyKind::Named { def, .. } => def,
                _ => return Err(CodegenError::new(iter.span, "Entry has no layout")),
            },
            &[kt, vt],
        );
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let latch = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self
            .call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[keys])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;
        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let key_raw = self
            .call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[keys, i2])
            .expect("get key");
        let val_raw = self
            .call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key_raw])
            .expect("get value");
        let entry = self.alloc_struct(&layout);
        if let Some(ko) = layout.index_of("key") {
            if let Some(kv) = self.i64_to_elem(key_raw, kt, iter.span)? {
                self.b.ins().store(MemFlags::trusted(), kv, entry, layout.offsets[ko] as i32);
            }
        }
        if let Some(vo) = layout.index_of("value") {
            if let Some(vv) = self.i64_to_elem(val_raw, vt, iter.span)? {
                self.b.ins().store(MemFlags::trusted(), vv, entry, layout.offsets[vo] as i32);
            }
        }
        self.h_bind_pattern(pattern, Some(entry), entry_ty)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.h_block(body)?;
        if !self.term {
            self.b.ins().jump(latch, &[]);
            self.term = true;
        }
        self.loops.pop();
        self.switch(latch);
        let i3 = self.b.use_var(iv);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;
        self.switch(exit);
        Ok(None)
    }

    /// A closure expression: build its heap environment (shared with the AST
    /// path) and queue the lifted body in HIR form.
    fn h_closure(
        &mut self,
        params: &[(LocalId, Ty)],
        captures: &[(LocalId, Ty)],
        ret: Ty,
        is_async: bool,
        body: &hir::Expr,
        span: Span,
    ) -> CgResult<Option<Value>> {
        if is_async {
            return Err(CodegenError::new(
                span,
                "`async` closure code generation is not yet implemented",
            ));
        }
        let info = compiler::sema::results::ClosureInfo {
            params: params.to_vec(),
            captures: captures.to_vec(),
            ret,
        };
        let (func_id, env) = self.emit_closure_value(&info, span)?;
        self.closures.push(crate::ClosureJob {
            func_id,
            info,
            body: body.clone(),
            subst: self.subst.clone(),
            span,
        });
        Ok(Some(env))
    }

    /// A method call `recv.m(..)` / static `Type.m(..)` — mirrors the AST
    /// `gen_method_call`/`gen_static_call`, covering dynamic dispatch through an
    /// interface object's vtable, the builtin-bound fallbacks
    /// (`Clone`/`Eq`/`Ord`/`ToStr`/`Hash` on a primitive/`str`/collection),
    /// interface→concrete resolution, and plain `extend` methods.
    fn h_method_call(
        &mut self,
        def: DefId,
        type_args: &[Ty],
        recv_static: Option<Ty>,
        is_static: bool,
        args: &[hir::Expr],
        ty: Ty,
        span: Span,
    ) -> CgResult<Option<Value>> {
        let dk = self.cx.analysis.program.def(def).kind;
        let is_iface = dk == DefKind::InterfaceMethod;
        let prog_targs = |this: &Self| -> Vec<Ty> {
            type_args.iter().map(|t| resolve_shallow(this.cx.analysis, *t, &this.subst)).collect()
        };

        // -- static call: no receiver; `args` are the call arguments ---------
        if is_static {
            let (target, targs) = if is_iface {
                let recv = resolve_shallow(
                    self.cx.analysis,
                    recv_static.unwrap_or(self.cx.analysis.tcx.error),
                    &self.subst,
                );
                self.resolve_iface_method(def, recv).ok_or_else(|| {
                    CodegenError::new(span, "cannot resolve static interface method to a concrete impl")
                })?
            } else {
                (def, prog_targs(self))
            };
            let mut arg_vals = Vec::with_capacity(args.len());
            for a in args {
                arg_vals.push(
                    self.h_expr(a)?.ok_or_else(|| CodegenError::new(a.span, "argument has no value"))?,
                );
            }
            return self.emit_call(target, targs, &arg_vals, span);
        }

        // -- instance call: `args[0]` is the receiver ------------------------
        let receiver = &args[0];
        let method_args = &args[1..];
        let recv_ty = resolve_shallow(self.cx.analysis, receiver.ty, &self.subst);
        let prog = &self.cx.analysis.program;

        // Dynamic dispatch through an interface object's vtable.
        if is_iface
            && matches!(self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Named { def: d, .. } if self.cx.analysis.program.def(*d).kind == DefKind::Interface)
        {
            let slot = self
                .vtable_slot(def)
                .ok_or_else(|| CodegenError::new(span, "method not found in interface"))?;
            let obj = self
                .h_expr(receiver)?
                .ok_or_else(|| CodegenError::new(receiver.span, "interface receiver has no value"))?;
            let mut arg_vals = Vec::with_capacity(method_args.len());
            for a in method_args {
                arg_vals.push(
                    self.h_expr(a)?.ok_or_else(|| CodegenError::new(a.span, "argument has no value"))?,
                );
            }
            let ret_clty = clty_of(self.cx.analysis, resolve_shallow(self.cx.analysis, ty, &self.subst));
            return self.emit_vtable_call(slot, obj, &arg_vals, ret_clty);
        }

        // `Clone.clone` through a `T: Clone` bound on a builtin-cloneable type.
        if is_iface && prog.def(def).parent == Some(prog.clone_def) {
            if let Some(kind) = self.builtin_clone_kind(recv_ty) {
                let v = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(receiver.span, "clone receiver has no value"))?;
                return self.emit_builtin_clone(v, recv_ty, kind, receiver.span);
            }
        }
        let parent = prog.def(def).parent;
        // `Eq.eq` / `Ord.{lt,le,gt,ge}` on a primitive/`str` receiver.
        if is_iface
            && (parent == Some(prog.eq_def) || parent == Some(prog.ord_def))
            && prog.eq_def != DefId(0)
        {
            if let Some(op) = crate::gen_call::compare_op(&prog.def(def).name) {
                if self.is_primitive_comparable(recv_ty) {
                    let l = self
                        .h_expr(receiver)?
                        .ok_or_else(|| CodegenError::new(receiver.span, "comparison receiver has no value"))?;
                    let r = self
                        .h_expr(&method_args[0])?
                        .ok_or_else(|| CodegenError::new(span, "comparison argument has no value"))?;
                    return self.emit_primitive_compare(op, recv_ty, l, r);
                }
            }
        }
        // `ToStr.to_str` on a directly-stringifiable receiver.
        if is_iface && parent == Some(prog.to_str_def) && prog.to_str_def != DefId(0) {
            if matches!(
                self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
            ) {
                let v = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(receiver.span, "to_str receiver has no value"))?;
                return Ok(Some(self.cast_to_str(v, recv_ty, span)?));
            }
        }
        // `Hash.hash` on a primitive/`str` receiver.
        if is_iface && parent == Some(prog.hash_def) && prog.hash_def != DefId(0) {
            if matches!(
                self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str
            ) {
                let v = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(receiver.span, "hash receiver has no value"))?;
                return Ok(Some(self.gen_primitive_hash(v, recv_ty)));
            }
        }
        // Interface method on a type parameter → concrete impl; or a plain
        // `extend` method (its type args recorded on the node).
        let (target, targs) = if is_iface {
            self.resolve_iface_method(def, recv_ty).ok_or_else(|| {
                CodegenError::new(span, "cannot resolve interface method to a concrete impl")
            })?
        } else {
            (def, prog_targs(self))
        };
        let self_val = self
            .h_expr(receiver)?
            .ok_or_else(|| CodegenError::new(receiver.span, "method receiver has no value"))?;
        let mut arg_vals = vec![self_val];
        for a in method_args {
            arg_vals.push(
                self.h_expr(a)?.ok_or_else(|| CodegenError::new(a.span, "argument has no value"))?,
            );
        }
        self.emit_call(target, targs, &arg_vals, span)
    }

    /// A bare `async { … }` block (mirrors the AST `gen_async_block`): allocate
    /// the state struct (room for body locals + the inner future when it
    /// suspends; just the captures otherwise), wrap it in a `Future<Output>`
    /// box, and queue the block as the `poll` function body (HIR form).
    fn h_async_block(
        &mut self,
        output: Ty,
        params: &[(LocalId, Ty)],
        captures: &[(LocalId, Ty)],
        block: &hir::Block,
        span: Span,
    ) -> CgResult<Option<Value>> {
        if !params.is_empty() {
            return Err(CodegenError::new(span, "async closure lowering is not yet implemented"));
        }
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        let name = format!("asyncblk.{}$poll", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let poll_fid = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .expect("declare async block poll");

        let (size, ptr_offsets, cap_offs): (u32, Vec<u32>, Vec<i32>) = if h_block_has_await(block) {
            let cap_ids: Vec<LocalId> = captures.iter().map(|(l, _)| *l).collect();
            let layout = async_state_layout(
                self.cx.analysis,
                &self.subst,
                &cap_ids,
                BodyView(block),
                self.cx.captured_locals,
            );
            let offs = cap_ids.iter().map(|l| layout.slot_off[l]).collect();
            (layout.state_size, layout.ptr_offsets, offs)
        } else {
            let n = captures.len();
            let cap_offs: Vec<i32> = (0..n).map(|k| (8 + k * 8) as i32).collect();
            let ptr_offsets: Vec<u32> = cap_offs.iter().map(|&o| o as u32).collect();
            ((8 + n * 8) as u32, ptr_offsets, cap_offs)
        };
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let state = self
            .call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, state, 0);
        for (k, (local, _)) in captures.iter().enumerate() {
            let var = *self
                .vars
                .get(local)
                .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, state, cap_offs[k]);
        }
        let fut = self.emit_future_box(poll_fid, state);
        let info = compiler::sema::results::AsyncInfo {
            output,
            params: params.to_vec(),
            captures: captures.to_vec(),
        };
        let body = hir::Expr {
            kind: hir::ExprKind::Block(block.clone()),
            ty: block.ty,
            span,
        };
        self.async_jobs.push(crate::AsyncJob {
            poll_fid,
            info,
            body,
            subst: self.subst.clone(),
            span,
            out: output,
        });
        Ok(Some(fut))
    }

    /// A compiler intrinsic. Only the value-producing, non-async ones are
    /// covered so far (empty collection constructors and builtin `.clone()`);
    /// the coverage predicate keeps the rest (numeric/foreign/concurrency/async
    /// intrinsics) on the AST path.
    fn h_intrinsic(&mut self, intrinsic: &hir::Intrinsic, args: &[hir::Expr], ty: Ty, span: Span)
        -> CgResult<Option<Value>>
    {
        match intrinsic {
            hir::Intrinsic::CollectionCtor => {
                if let Some((kt, vt)) = self.map_kv_of(ty) {
                    Ok(Some(self.gen_map_new(kt, vt)))
                } else if let Some(elem) = self.list_elem_of(ty) {
                    Ok(Some(self.gen_list_new(elem)))
                } else {
                    Err(CodegenError::new(span, "unknown builtin constructor"))
                }
            }
            hir::Intrinsic::Clone(kind) => {
                let receiver = &args[0];
                let rty = receiver.ty;
                let v = self
                    .h_expr(receiver)?
                    .ok_or_else(|| CodegenError::new(receiver.span, "clone receiver has no value"))?;
                self.emit_builtin_clone(v, rty, *kind, receiver.span)
            }
            hir::Intrinsic::Num(intr) => {
                let mut argv = Vec::with_capacity(args.len());
                for arg in args {
                    argv.push(self.h_expr(arg)?.ok_or_else(|| {
                        CodegenError::new(arg.span, "numeric intrinsic argument has no value")
                    })?);
                }
                self.emit_num_intrinsic(*intr, &argv)
            }
            // Foreign (FFI) memory intrinsics (`docs/19` §5–6).
            hir::Intrinsic::ForeignAlloc { ty: t, zeroed } => {
                let size = self.sizeof_ty(*t);
                let sz = self.b.ins().iconst(types::I64, size as i64);
                let f = if *zeroed { "lang_foreign_alloc_zeroed" } else { "lang_foreign_alloc" };
                Ok(self.call_intrinsic(f, &[types::I64], Some(PTR), &[sz]))
            }
            hir::Intrinsic::ForeignFlex { ty: t, elem } => {
                let base = self.sizeof_ty(*t) as i64;
                let esz = self.sizeof_ty(*elem) as i64;
                let n = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "alloc_flex count has no value"))?;
                let extra = self.b.ins().imul_imm(n, esz);
                let base_v = self.b.ins().iconst(types::I64, base);
                let total = self.b.ins().iadd(base_v, extra);
                Ok(self.call_intrinsic("lang_foreign_alloc", &[types::I64], Some(PTR), &[total]))
            }
            hir::Intrinsic::ForeignRealloc => {
                let p = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "realloc pointer has no value"))?;
                let sz = self
                    .h_expr(&args[1])?
                    .ok_or_else(|| CodegenError::new(args[1].span, "realloc size has no value"))?;
                Ok(self.call_intrinsic("lang_foreign_realloc", &[PTR, types::I64], Some(PTR), &[p, sz]))
            }
            hir::Intrinsic::ForeignFree => {
                let p = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "free argument has no value"))?;
                self.call_intrinsic("lang_foreign_free", &[PTR], None, &[p]);
                Ok(None)
            }
            hir::Intrinsic::CStringFromStr => {
                let s = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "from_str argument has no value"))?;
                Ok(self.call_intrinsic("lang_cstring_from_str", &[PTR], Some(PTR), &[s]))
            }
            hir::Intrinsic::CStrToStr => {
                let p = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "to_str argument has no value"))?;
                Ok(self.call_intrinsic("lang_cstr_to_str", &[PTR], Some(PTR), &[p]))
            }
            // -- concurrency (build/return runtime handle & future objects;
            //    no async state machine needed here) -------------------------
            hir::Intrinsic::ChannelNew => self.gen_channel_new(ty, span),
            hir::Intrinsic::SharedNew => {
                let elem = args[0].ty;
                let v = self.h_expr(&args[0])?;
                self.emit_shared_new(v, elem, ty, span)
            }
            hir::Intrinsic::ThreadSpawn { output } => {
                let env = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "spawn closure has no value"))?;
                self.emit_thread_spawn(env, *output, span)
            }
            hir::Intrinsic::ThreadJoin { output } => {
                let jh = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "join receiver has no value"))?;
                self.emit_thread_join(jh, *output, span)
            }
            hir::Intrinsic::YieldNow => {
                let ready_tid = 1000 + self.cx.analysis.program.ready_def.index() as i64;
                let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
                let rt = self.b.ins().iconst(types::I64, ready_tid);
                let pt = self.b.ins().iconst(types::I64, pending_tid);
                Ok(self.call_intrinsic("lang_async_yield", &[types::I64, types::I64], Some(PTR), &[rt, pt]))
            }
            hir::Intrinsic::AsyncSleep => {
                let ms = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "sleep argument has no value"))?;
                let ready_tid = 1000 + self.cx.analysis.program.ready_def.index() as i64;
                let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
                let rt = self.b.ins().iconst(types::I64, ready_tid);
                let pt = self.b.ins().iconst(types::I64, pending_tid);
                Ok(self.call_intrinsic(
                    "lang_async_sleep",
                    &[types::I64, types::I64, types::I64],
                    Some(PTR),
                    &[ms, rt, pt],
                ))
            }
            hir::Intrinsic::AsyncTimeout { output } => {
                let fut = self
                    .h_expr(&args[0])?
                    .ok_or_else(|| CodegenError::new(args[0].span, "timeout future has no value"))?;
                self.mark_root(fut);
                let ms = self
                    .h_expr(&args[1])?
                    .ok_or_else(|| CodegenError::new(args[1].span, "timeout duration has no value"))?;
                let out = resolve_shallow(self.cx.analysis, *output, &self.subst);
                let t_id = self.type_id_of(out);
                let t_is_ptr = i64::from(is_managed_ptr(self.cx.analysis, out));
                let timedout_tid = 1000 + self.cx.analysis.program.timed_out_def.index() as i64;
                let ready_tid = 1000 + self.cx.analysis.program.ready_def.index() as i64;
                let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
                let t_id = self.b.ins().iconst(types::I64, t_id);
                let tp = self.b.ins().iconst(types::I64, t_is_ptr);
                let to = self.b.ins().iconst(types::I64, timedout_tid);
                let rt = self.b.ins().iconst(types::I64, ready_tid);
                let pt = self.b.ins().iconst(types::I64, pending_tid);
                Ok(self.call_intrinsic(
                    "lang_async_timeout",
                    &[PTR, types::I64, types::I64, types::I64, types::I64, types::I64, types::I64],
                    Some(PTR),
                    &[fut, ms, t_id, tp, to, rt, pt],
                ))
            }
            hir::Intrinsic::FutureCancel => {
                // A compute-only future has nothing to release; evaluate the
                // receiver for effect and yield no value.
                self.h_expr(&args[0])?;
                Ok(None)
            }
        }
    }

    fn h_adjust(&mut self, adjust: &Adjust, inner: &hir::Expr) -> CgResult<Option<Value>> {
        let v = self.h_expr(inner)?;
        let from = inner.ty;
        match adjust {
            Adjust::Widen(target) => {
                let tgt = resolve_shallow(self.cx.analysis, *target, &self.subst);
                if npo_union(self.cx.analysis, tgt).is_some() {
                    let p = v.unwrap_or_else(|| self.b.ins().iconst(PTR, 0));
                    return Ok(Some(p));
                }
                Ok(Some(self.apply_widen(v, from)))
            }
            Adjust::Unbox(target) => {
                let src = resolve_shallow(self.cx.analysis, from, &self.subst);
                if npo_union(self.cx.analysis, src).is_some() {
                    return Ok(v);
                }
                let ptr = v.expect("unbox target is a boxed pointer");
                match clty_of(self.cx.analysis, *target) {
                    Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
                    None => Ok(None),
                }
            }
            Adjust::WidenDyn(iface) => Ok(Some(self.gen_widen_dyn(v, from, *iface, inner.span)?)),
        }
    }
}

/// Map the HIR's binary operator spelling to the AST's (the shared
/// [`FnGen::emit_binop`] still takes the AST `BinaryOp`).
fn hir_to_ast_binop(op: hir::BinaryOp) -> BinaryOp {
    use hir::BinaryOp as H;
    match op {
        H::Add => BinaryOp::Add,
        H::Sub => BinaryOp::Sub,
        H::Mul => BinaryOp::Mul,
        H::Div => BinaryOp::Div,
        H::Rem => BinaryOp::Rem,
        H::Eq => BinaryOp::Eq,
        H::Ne => BinaryOp::Ne,
        H::Lt => BinaryOp::Lt,
        H::Le => BinaryOp::Le,
        H::Gt => BinaryOp::Gt,
        H::Ge => BinaryOp::Ge,
        H::And => BinaryOp::And,
        H::Or => BinaryOp::Or,
        H::BitAnd => BinaryOp::BitAnd,
        H::BitOr => BinaryOp::BitOr,
        H::BitXor => BinaryOp::BitXor,
        H::Shl => BinaryOp::Shl,
        H::Shr => BinaryOp::Shr,
    }
}

// ===========================================================================
// Coverage predicate — conservatively, exactly the forms `h_expr` handles
// ===========================================================================

// ===========================================================================
// Async-body analysis over the HIR (mirrors `support.rs`'s AST versions; the
// HIR is cleaner — `await` sites are `ExprKind::Await` and bindings are
// `Pattern::Bind(LocalId)`, so no span side-table lookups are needed). These
// back the HIR async state-machine codegen.
// ===========================================================================

/// Whether `block` contains an `await` in its own async scope (NOT descending
/// into nested closures / `async {}` blocks, which have their own `poll`).
#[allow(dead_code)] // wired into HIR async codegen next
pub(crate) fn h_block_has_await(b: &hir::Block) -> bool {
    b.stmts.iter().any(h_stmt_has_await) || b.trailing.as_deref().is_some_and(h_expr_has_await)
}

#[allow(dead_code)] // wired into HIR async codegen next
fn h_stmt_has_await(s: &hir::Stmt) -> bool {
    match &s.kind {
        hir::StmtKind::Let { init, .. } => h_expr_has_await(init),
        hir::StmtKind::Assign { target, value } => {
            h_expr_has_await(target) || h_expr_has_await(value)
        }
        hir::StmtKind::Expr(e) => h_expr_has_await(e),
        hir::StmtKind::Item(_) => false,
    }
}

#[allow(dead_code)] // wired into HIR async codegen next
fn h_expr_has_await(e: &hir::Expr) -> bool {
    use hir::ExprKind as K;
    match &e.kind {
        K::Await { .. } => true,
        // Nested async scopes own their suspension.
        K::Closure { .. } | K::AsyncBlock { .. } => false,
        K::Unary { operand: x, .. }
        | K::Cast { expr: x, .. }
        | K::Field { receiver: x, .. }
        | K::TupleIndex { receiver: x, .. }
        | K::Try { expr: x, .. }
        | K::Ref(x)
        | K::Deref(x)
        | K::Adjust { expr: x, .. }
        | K::Spawn { expr: x, .. } => h_expr_has_await(x),
        K::Binary { left, right, .. } => h_expr_has_await(left) || h_expr_has_await(right),
        K::Tuple(xs) | K::List(xs) => xs.iter().any(h_expr_has_await),
        K::Call { args, kind, .. } => {
            (matches!(kind, hir::CallKind::Closure { callee } if h_expr_has_await(callee)))
                || args.iter().any(h_expr_has_await)
        }
        K::Intrinsic { args, .. } => args.iter().any(h_expr_has_await),
        K::Index { receiver, index } => h_expr_has_await(receiver) || h_expr_has_await(index),
        K::Struct { fields, spread, .. } => {
            fields.iter().any(|f| h_expr_has_await(&f.value))
                || spread.as_deref().is_some_and(h_expr_has_await)
        }
        K::Map(items) => items.iter().any(|it| match it {
            hir::MapEntry::Kv { key, value } => h_expr_has_await(key) || h_expr_has_await(value),
            hir::MapEntry::Spread(e) => h_expr_has_await(e),
        }),
        K::Str(parts) => parts.iter().any(|p| match p {
            hir::StrPart::Interp { expr, .. } => h_expr_has_await(expr),
            hir::StrPart::Text(_) => false,
        }),
        K::If { cond, then_block, else_branch } => {
            h_expr_has_await(cond)
                || h_block_has_await(then_block)
                || else_branch.as_deref().is_some_and(h_expr_has_await)
        }
        K::Match { scrutinee, arms } => {
            h_expr_has_await(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(h_expr_has_await) || h_expr_has_await(&a.body)
                })
        }
        K::Block(b) | K::Loop(b) => h_block_has_await(b),
        K::While { cond, body } => h_expr_has_await(cond) || h_block_has_await(body),
        K::For { in_async, iter, body, .. } => {
            *in_async || h_expr_has_await(iter) || h_block_has_await(body)
        }
        K::Return(v) | K::Break(v) => v.as_deref().is_some_and(h_expr_has_await),
        _ => false,
    }
}

/// Collect the spans that key each statement-level `await` suspend site (the
/// whole RHS of a `var`/assignment, a bare expression statement, a block's
/// trailing expression, or `return`), plus the `for await` iterable span.
/// Recurses through control-flow bodies. Mirrors `scan_stmt_awaits`; for HIR the
/// key is the `Await` node's own span (used consistently by the codegen).
#[allow(dead_code)] // wired into HIR async codegen next
pub(crate) fn h_scan_stmt_awaits(block: &hir::Block, out: &mut Vec<Span>) {
    for s in &block.stmts {
        match &s.kind {
            hir::StmtKind::Let { init, .. } => h_scan_value_await(init, out),
            hir::StmtKind::Assign { value, .. } => h_scan_value_await(value, out),
            hir::StmtKind::Expr(e) => h_scan_value_await(e, out),
            hir::StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &block.trailing {
        h_scan_value_await(t, out);
    }
}

#[allow(dead_code)] // wired into HIR async codegen next
fn h_scan_value_await(e: &hir::Expr, out: &mut Vec<Span>) {
    use hir::ExprKind as K;
    match &e.kind {
        K::Await { .. } => out.push(e.span),
        K::Adjust { expr, .. } | K::Return(Some(expr)) | K::Break(Some(expr)) => {
            h_scan_value_await(expr, out)
        }
        K::Block(b) | K::Loop(b) => h_scan_stmt_awaits(b, out),
        K::While { body, .. } => h_scan_stmt_awaits(body, out),
        K::For { in_async, iter, body, .. } => {
            if *in_async {
                out.push(iter.span);
            }
            h_scan_stmt_awaits(body, out);
        }
        K::If { then_block, else_branch, .. } => {
            h_scan_stmt_awaits(then_block, out);
            if let Some(e) = else_branch {
                h_scan_value_await(e, out);
            }
        }
        K::Match { arms, .. } => {
            for arm in arms {
                h_scan_value_await(&arm.body, out);
            }
        }
        _ => {}
    }
}

/// Enumerate every local binding introduced in `block` (so an async state struct
/// can reserve a slot for each), NOT descending into nested closures /
/// `async {}` blocks. Mirrors `collect_block_locals`; HIR bindings are
/// `LocalId`s on the pattern, so no resolution lookup.
#[allow(dead_code)] // wired into HIR async codegen next
pub(crate) fn h_collect_block_locals(block: &hir::Block, out: &mut Vec<LocalId>, seen: &mut std::collections::HashSet<LocalId>) {
    for s in &block.stmts {
        match &s.kind {
            hir::StmtKind::Let { pattern, init } => {
                h_collect_pat_locals(pattern, out, seen);
                h_collect_expr_locals(init, out, seen);
            }
            hir::StmtKind::Assign { target, value } => {
                h_collect_expr_locals(target, out, seen);
                h_collect_expr_locals(value, out, seen);
            }
            hir::StmtKind::Expr(e) => h_collect_expr_locals(e, out, seen),
            hir::StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &block.trailing {
        h_collect_expr_locals(t, out, seen);
    }
}

#[allow(dead_code)] // wired into HIR async codegen next
fn push_local_id(id: LocalId, out: &mut Vec<LocalId>, seen: &mut std::collections::HashSet<LocalId>) {
    if seen.insert(id) {
        out.push(id);
    }
}

#[allow(dead_code)] // wired into HIR async codegen next
fn h_collect_pat_locals(p: &hir::Pattern, out: &mut Vec<LocalId>, seen: &mut std::collections::HashSet<LocalId>) {
    use hir::PatternKind as P;
    match &p.kind {
        P::Bind(l) => push_local_id(*l, out, seen),
        P::TypeBind { bind: Some(l), .. } => push_local_id(*l, out, seen),
        P::TupleStruct { fields, rest, .. } => {
            for f in fields {
                h_collect_pat_locals(f, out, seen);
            }
            if let Some(r) = rest {
                if let Some(l) = r.bind {
                    push_local_id(l, out, seen);
                }
            }
        }
        P::RecordStruct { fields, .. } => {
            for f in fields {
                h_collect_pat_locals(&f.pattern, out, seen);
            }
        }
        P::Tuple { elems, rest } | P::List { elems, rest } => {
            for e in elems {
                h_collect_pat_locals(e, out, seen);
            }
            if let Some((_, r)) = rest {
                if let Some(l) = r.bind {
                    push_local_id(l, out, seen);
                }
            }
        }
        P::Or(ps) => {
            for sub in ps {
                h_collect_pat_locals(sub, out, seen);
            }
        }
        _ => {}
    }
}

#[allow(dead_code)] // wired into HIR async codegen next
fn h_collect_expr_locals(e: &hir::Expr, out: &mut Vec<LocalId>, seen: &mut std::collections::HashSet<LocalId>) {
    use hir::ExprKind as K;
    match &e.kind {
        K::Closure { .. } | K::AsyncBlock { .. } => {}
        K::Unary { operand: x, .. }
        | K::Cast { expr: x, .. }
        | K::Field { receiver: x, .. }
        | K::TupleIndex { receiver: x, .. }
        | K::Try { expr: x, .. }
        | K::Ref(x)
        | K::Deref(x)
        | K::Adjust { expr: x, .. }
        | K::Await { expr: x, .. }
        | K::Spawn { expr: x, .. } => h_collect_expr_locals(x, out, seen),
        K::Binary { left, right, .. } => {
            h_collect_expr_locals(left, out, seen);
            h_collect_expr_locals(right, out, seen);
        }
        K::Tuple(xs) | K::List(xs) => {
            for x in xs {
                h_collect_expr_locals(x, out, seen);
            }
        }
        K::Call { args, kind, .. } => {
            if let hir::CallKind::Closure { callee } = kind {
                h_collect_expr_locals(callee, out, seen);
            }
            for x in args {
                h_collect_expr_locals(x, out, seen);
            }
        }
        K::Intrinsic { args, .. } => {
            for x in args {
                h_collect_expr_locals(x, out, seen);
            }
        }
        K::Index { receiver, index } => {
            h_collect_expr_locals(receiver, out, seen);
            h_collect_expr_locals(index, out, seen);
        }
        K::Struct { fields, spread, .. } => {
            for f in fields {
                h_collect_expr_locals(&f.value, out, seen);
            }
            if let Some(s) = spread {
                h_collect_expr_locals(s, out, seen);
            }
        }
        K::Map(items) => {
            for it in items {
                match it {
                    hir::MapEntry::Kv { key, value } => {
                        h_collect_expr_locals(key, out, seen);
                        h_collect_expr_locals(value, out, seen);
                    }
                    hir::MapEntry::Spread(x) => h_collect_expr_locals(x, out, seen),
                }
            }
        }
        K::Str(parts) => {
            for p in parts {
                if let hir::StrPart::Interp { expr, .. } = p {
                    h_collect_expr_locals(expr, out, seen);
                }
            }
        }
        K::If { cond, then_block, else_branch } => {
            h_collect_expr_locals(cond, out, seen);
            h_collect_block_locals(then_block, out, seen);
            if let Some(e) = else_branch {
                h_collect_expr_locals(e, out, seen);
            }
        }
        K::Match { scrutinee, arms } => {
            h_collect_expr_locals(scrutinee, out, seen);
            for arm in arms {
                h_collect_pat_locals(&arm.pattern, out, seen);
                if let Some(g) = &arm.guard {
                    h_collect_expr_locals(g, out, seen);
                }
                h_collect_expr_locals(&arm.body, out, seen);
            }
        }
        K::Block(b) | K::Loop(b) => h_collect_block_locals(b, out, seen),
        K::While { cond, body } => {
            h_collect_expr_locals(cond, out, seen);
            h_collect_block_locals(body, out, seen);
        }
        K::For { pattern, iter, body, .. } => {
            h_collect_pat_locals(pattern, out, seen);
            h_collect_expr_locals(iter, out, seen);
            h_collect_block_locals(body, out, seen);
        }
        K::Return(v) | K::Break(v) => {
            if let Some(x) = v {
                h_collect_expr_locals(x, out, seen);
            }
        }
        _ => {}
    }
}
