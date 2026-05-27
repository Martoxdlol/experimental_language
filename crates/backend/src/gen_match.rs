//! Per-function codegen: `?` propagation, `match`, and loops (`for`/`while`/`loop`) (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- `?` propagation -----------------------------------------------------

    /// `expr?`: if the union box holds a failure variant (one also in the
    /// function's return type), return it; otherwise continue with the success
    /// value (`success_ty` is the checker-computed result type).
    pub(crate) fn gen_try(&mut self, inner: &Expr, q_span: Span, success_ty: Ty) -> CgResult<Option<Value>> {
        let et = self.cx.analysis.results.expr_ty(inner.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let ptr = self.gen_expr(inner)?.ok_or_else(|| {
            CodegenError::new(inner.span, "`?` operand has no value")
        })?;
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);

        // Failure variants: those of `et` that are also in the return type.
        let r = self.ret_ty;
        let r_variants = self.cx.analysis.tcx.variants(r);
        let failures: Vec<Ty> = self
            .cx
            .analysis
            .tcx
            .variants(et)
            .into_iter()
            .filter(|v| r_variants.contains(v))
            .collect();

        for fv in failures {
            let fid = { let id = self.type_id_of(fv); self.b.ins().iconst(types::I64, id) };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, fid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            // Return the box as the function's return type. When R is a union
            // the box passes through; otherwise unbox to R's single variant.
            let ret_val = if matches!(self.cx.analysis.tcx.kind(r), TyKind::Union(_) | TyKind::Dynamic) {
                Some(ptr)
            } else {
                clty_of(self.cx.analysis, r)
                    .map(|ct| self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))
            };
            self.emit_return(ret_val)?;

            self.switch(next);
        }

        // Residual conversions (`docs/13` §4): a failure variant `E` not in R
        // is propagated by converting it via `Target.from_residual(e)` and
        // returning that (boxed through R).
        let conversions = self
            .cx
            .analysis
            .results
            .residual_conversions
            .get(&q_span)
            .cloned()
            .unwrap_or_default();
        for (residual, method, target) in conversions {
            let rid = { let id = self.type_id_of(residual); self.b.ins().iconst(types::I64, id) };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, rid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            // Unbox the residual payload, convert it, then box the result.
            let payload = match clty_of(self.cx.analysis, residual) {
                Some(ct) => self.b.ins().load(ct, MemFlags::trusted(), ptr, 8),
                None => self.b.ins().iconst(PTR, 0),
            };
            let converted = self
                .emit_call(method, Vec::new(), &[payload], inner.span)?
                .ok_or_else(|| CodegenError::new(inner.span, "`from_residual` returned no value"))?;
            // The converted value has type `target`; box it through R (a union)
            // or return it directly when R is that single type.
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

    // -- match ---------------------------------------------------------------

    pub(crate) fn gen_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        result_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let sty = self.cx.analysis.results.expr_ty(scrutinee.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let scrut = self.gen_expr(scrutinee)?;
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
            let matched = self.pattern_matches(&arm.pattern, sty, scrut, tag)?;
            let cand = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(matched, cand, &[], next, &[]);
            self.term = true;

            self.switch(cand);
            self.bind_match_pattern(&arm.pattern, sty, scrut, tag)?;
            // A guard, if present, must pass for the arm to fire.
            let proceed = match &arm.guard {
                Some(g) => self.gen_expr(g)?.ok_or_else(|| {
                    CodegenError::new(g.span, "guard has no value")
                })?,
                None => self.b.ins().iconst(types::I8, 1),
            };
            let body_block = self.b.create_block();
            self.b.ins().brif(proceed, body_block, &[], next, &[]);
            self.term = true;

            self.switch(body_block);
            let body_val = self.gen_expr(&arm.body)?;
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

    /// Whether `pattern` structurally matches the scrutinee, as an i8 boolean
    /// (without binding). Tuple patterns are irrefutable here; their bindings
    /// are extracted in [`Self::bind_match_pattern`].
    pub(crate) fn pattern_matches(
        &mut self,
        pattern: &Pattern,
        sty: Ty,
        scrut: Option<Value>,
        tag: Option<Value>,
    ) -> CgResult<Value> {
        let one = self.b.ins().iconst(types::I8, 1);
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) | PatternKind::Tuple { .. } => {
                Ok(one)
            }
            PatternKind::TypeBinding { .. } | PatternKind::UnitPath(_) => {
                let t = self.cx.analysis.results.pattern_types.get(&pattern.span).copied()
                    .unwrap_or(self.cx.analysis.tcx.error);
                match tag {
                    Some(tag) => Ok(self.tag_in_target(tag, t)),
                    // Concrete scrutinee: statically known.
                    None => {
                        let yes = sty == t;
                        Ok(self.b.ins().iconst(types::I8, i64::from(yes)))
                    }
                }
            }
            PatternKind::Literal(e) => {
                // `null` literal against a union: compare the tag.
                if let ExprKind::Null = &e.kind {
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
                let lit = self.gen_expr(e)?.ok_or_else(|| {
                    CodegenError::new(e.span, "literal pattern has no value")
                })?;
                let scrut = scrut.ok_or_else(|| {
                    CodegenError::new(pattern.span, "scrutinee has no value")
                })?;
                // Compare against the (concrete-typed) scrutinee value.
                match self.cx.analysis.tcx.kind(sty) {
                    TyKind::Float(_) => Ok(self.b.ins().fcmp(FloatCC::Equal, scrut, lit)),
                    _ => Ok(self.b.ins().icmp(IntCC::Equal, scrut, lit)),
                }
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable in match")),
        }
    }

    /// Bind the names introduced by a matched pattern, extracting payloads.
    pub(crate) fn bind_match_pattern(
        &mut self,
        pattern: &Pattern,
        sty: Ty,
        scrut: Option<Value>,
        _tag: Option<Value>,
    ) -> CgResult<()> {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::UnitPath(_) => Ok(()),
            PatternKind::Binding(name) => {
                if let (Some(v), Some(ct)) = (scrut, self.cx_clty(sty)) {
                    let local = self.resolve_local(name.span)?;
                    let var = self.fresh_var(local, ct);
                    self.b.def_var(var, v);
                    let _ = ct;
                }
                Ok(())
            }
            PatternKind::TypeBinding { binding: Some(b), .. } => {
                let t = self.cx.analysis.results.pattern_types.get(&pattern.span).copied()
                    .unwrap_or(self.cx.analysis.tcx.error);
                // Extract the payload: unbox from a union, else use as-is.
                let val = match (scrut, self.cx_clty(t)) {
                    (Some(p), Some(ct)) if matches!(
                        self.cx.analysis.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic
                    ) => Some(self.b.ins().load(ct, MemFlags::trusted(), p, 8)),
                    (s, Some(_)) => s,
                    _ => None,
                };
                if let (Some(v), Some(ct)) = (val, self.cx_clty(t)) {
                    let local = self.resolve_local(b.span)?;
                    let var = self.fresh_var(local, ct);
                    self.b.def_var(var, v);
                }
                Ok(())
            }
            PatternKind::TypeBinding { binding: None, .. } => Ok(()),
            PatternKind::Tuple { elems, rest: None } => {
                let layout = self.layout_for_ty(sty).ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple pattern on non-aggregate")
                })?;
                let elem_tys = match self.cx.analysis.tcx.kind(sty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                let ptr = scrut.ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple scrutinee has no value")
                })?;
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => Some(self.b.ins().load(
                            *ct, MemFlags::trusted(), ptr, layout.offsets[i] as i32,
                        )),
                        _ => None,
                    };
                    self.bind_match_pattern(sub, elem_tys[i], elem_val, None)?;
                }
                Ok(())
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable in match")),
        }
    }

    /// Lower `for pat in iter { body }`. A `List` iterates by index (fast path);
    /// any other type recorded by the checker drives the `Iterator` protocol.
    pub(crate) fn gen_for(&mut self, pattern: &Pattern, iter: &Expr, body: &Block) -> CgResult<Option<Value>> {
        if let Some(info) = self.cx.analysis.results.for_iters.get(&iter.span).cloned() {
            return self.gen_for_iterator(pattern, iter, body, info);
        }
        if let Some((kt, vt, entry_ty)) = self.cx.analysis.results.for_maps.get(&iter.span).copied() {
            return self.gen_for_map(pattern, iter, body, kt, vt, entry_ty);
        }
        let ity = self.cx.analysis.results.expr_ty(iter.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let elem = self.list_elem_of(ity).ok_or_else(|| {
            CodegenError::new(iter.span, "`for` currently iterates `List<T>` only")
        })?;
        let list = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "iterable has no value")
        })?;

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
        let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, i2])
            .expect("get");
        let elem_val = self.i64_to_elem(raw, elem, iter.span)?;
        self.bind_pattern(pattern, elem_val, elem)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.gen_block(body)?;
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

    /// Lower `for pat in iter { body }` via the `Iterator` protocol: evaluate the
    /// iterator once, then loop calling `next()`, breaking on `Done` and binding
    /// the unwrapped `Item<U>.value` each step.
    pub(crate) fn gen_for_iterator(
        &mut self,
        pattern: &Pattern,
        iter: &Expr,
        body: &Block,
        info: ForIter,
    ) -> CgResult<Option<Value>> {
        let iter_val = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "iterator has no value")
        })?;
        // The iterator object is mutated by `next` and lives across the loop.
        self.mark_root(iter_val);

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        self.b.ins().jump(header, &[]);
        self.term = true;

        // header: u = iter.next(); branch on the `Done` tag.
        self.switch(header);
        self.emit_safepoint();
        // Dispatch `next`: a concrete/generic `extend` method is a direct call;
        // an interface method (bounded type param or interface object) resolves
        // to the concrete impl, or goes through the vtable for an object.
        let u = if self.cx.analysis.program.def(info.next).kind == DefKind::InterfaceMethod {
            let recv = resolve_shallow(self.cx.analysis, info.iter_ty, &self.subst);
            if self.is_interface_ty(recv) {
                let slot = self.vtable_slot(info.next)
                    .ok_or_else(|| CodegenError::new(iter.span, "iterator method not in interface"))?;
                self.emit_vtable_call(slot, iter_val, &[], Some(PTR))?
            } else {
                let (target, targs) = self.resolve_iface_method(info.next, recv)
                    .ok_or_else(|| CodegenError::new(iter.span, "cannot resolve iterator `next`"))?;
                self.emit_call(target, targs, &[iter_val], iter.span)?
            }
        } else {
            // Resolve the method's type args through this instance's subst.
            let next_targs: Vec<Ty> = info.next_targs.iter()
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

        // body: unwrap the `Item<U>` payload and bind the loop variable.
        self.switch(body_bb);
        let item_ptr = self.b.ins().load(PTR, MemFlags::trusted(), u, 8);
        let layout = self.layout_for_ty(info.item_ty).ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no layout")
        })?;
        let idx = layout.index_of("value").ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no `value` field")
        })?;
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
        self.bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    /// Lower `for await x in stream { body }` (`docs/21` §10): each iteration
    /// `await`s `stream.next_async()` (a suspend site), breaks on `Done`, and
    /// binds the unwrapped `Item<T>.value`. Only valid inside an async `poll`
    /// body. The stream must be a simple variable so re-loading it each
    /// iteration (across suspends) is correct.
    pub(crate) fn gen_for_await(&mut self, pattern: &Pattern, iter: &Expr, body: &Block)
        -> CgResult<Option<Value>>
    {
        let info = self.cx.analysis.results.for_async_iters.get(&iter.span).cloned()
            .ok_or_else(|| CodegenError::new(iter.span, "for-await stream was not analysed"))?;
        if !matches!(&iter.kind, ExprKind::Ident(_) | ExprKind::SelfExpr) {
            return Err(CodegenError::new(iter.span,
                "`for await` currently requires the stream to be a variable — \
                 bind it with `var s = …;` first"));
        }

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;

        // header: fut = stream.next_async(); await it (suspends until ready).
        self.switch(header);
        let iter_val = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "stream has no value")
        })?;
        let next_targs: Vec<Ty> = info.next_targs.iter()
            .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
            .collect();
        let fut = self.emit_call(info.next_async, next_targs, &[iter_val], iter.span)?
            .ok_or_else(|| CodegenError::new(iter.span, "`next_async` returned no value"))?;
        let u = self.emit_await_suspend(fut, iter.span, info.union_ty)?
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
        let layout = self.layout_for_ty(info.item_ty).ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no layout")
        })?;
        let idx = layout.index_of("value").ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no `value` field")
        })?;
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
        self.bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    /// Lower `for entry in map { body }`: snapshot the keys, then for each key
    /// build an `Entry<K, V>` (key + looked-up value) and bind the loop variable.
    pub(crate) fn gen_for_map(
        &mut self,
        pattern: &Pattern,
        iter: &Expr,
        body: &Block,
        kt: Ty,
        vt: Ty,
        entry_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let map = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "map has no value")
        })?;
        self.mark_root(map);
        // A snapshot list of the keys (rooted across the loop).
        let one = self.b.ins().iconst(types::I64, 1);
        let keys = self.call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, one])
            .expect("map_entries returns a list");
        self.mark_root(keys);
        let layout = self.struct_layout(
            match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, entry_ty, &self.subst)).clone() {
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
        let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[keys])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let key_raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[keys, i2])
            .expect("get key");
        let val_raw = self.call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key_raw])
            .expect("get value");
        // Build the Entry { key, value } struct.
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
        self.bind_pattern(pattern, Some(entry), entry_ty)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.gen_block(body)?;
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

    pub(crate) fn gen_while(&mut self, cond: &Expr, body: &Block) -> CgResult<Option<Value>> {
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(header);
        self.emit_safepoint();
        let c = self.gen_expr(cond)?.ok_or_else(|| {
            CodegenError::new(cond.span, "loop condition has no value")
        })?;
        self.b.ins().brif(c, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    pub(crate) fn gen_loop(&mut self, body: &Block, result_ty: Ty) -> CgResult<Option<Value>> {
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
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(body_bb, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(result_ct.map(|_| self.b.block_params(exit)[0]))
    }

    pub(crate) fn gen_break(&mut self, value: Option<&Expr>, span: Span) -> CgResult<Option<Value>> {
        let (break_block, has_value) = match self.loops.last() {
            Some(f) => (f.break_block, f.has_value),
            None => return Err(CodegenError::new(span, "`break` outside a loop")),
        };
        if has_value {
            let v = match value {
                Some(e) => self.gen_expr(e)?,
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
                self.gen_expr(e)?; // evaluate for effect, discard
            }
            self.b.ins().jump(break_block, &[]);
        }
        self.term = true;
        Ok(None)
    }

    pub(crate) fn gen_continue(&mut self, span: Span) -> CgResult<Option<Value>> {
        let cont = match self.loops.last() {
            Some(f) => f.continue_block,
            None => return Err(CodegenError::new(span, "`continue` outside a loop")),
        };
        self.b.ins().jump(cont, &[]);
        self.term = true;
        Ok(None)
    }

}
