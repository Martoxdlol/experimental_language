//! Per-function codegen: calls: functions, methods, closures, builtins, concurrency, FFI (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    /// Numeric-namespace intrinsic over already-evaluated argument values.
    /// Shared by the AST and HIR walks.
    pub(crate) fn emit_num_intrinsic(&mut self, intr: NumIntrinsic, args: &[Value]) -> CgResult<Option<Value>> {
        match intr {
            NumIntrinsic::IntBound { ty, max } => {
                let it = self.int_ty_of(ty);
                let (lo, hi) = int_min_max(it);
                let ct = int_clty(it);
                Ok(Some(self.b.ins().iconst(ct, if max { hi } else { lo })))
            }
            NumIntrinsic::FloatConst { ty, kind } => {
                let f = match kind {
                    0 => f64::INFINITY,
                    1 => f64::NEG_INFINITY,
                    _ => f64::NAN,
                };
                Ok(Some(match self.cx.analysis.tcx.kind(ty) {
                    TyKind::Float(FloatTy::F32) => self.b.ins().f32const(f as f32),
                    _ => self.b.ins().f64const(f),
                }))
            }
            NumIntrinsic::FloatPred { ty: _, kind } => {
                let v = args[0];
                let r = match kind {
                    // is_nan: v != v
                    0 => self.b.ins().fcmp(FloatCC::NotEqual, v, v),
                    // is_infinite: v == +inf || v == -inf
                    1 => {
                        let pinf = self.fconst_like(v, f64::INFINITY);
                        let ninf = self.fconst_like(v, f64::NEG_INFINITY);
                        let a = self.b.ins().fcmp(FloatCC::Equal, v, pinf);
                        let b = self.b.ins().fcmp(FloatCC::Equal, v, ninf);
                        self.b.ins().bor(a, b)
                    }
                    // is_finite: a finite value satisfies v - v == 0 (NaN/±inf give NaN).
                    _ => {
                        let diff = self.b.ins().fsub(v, v);
                        let zero = self.fconst_like(v, 0.0);
                        self.b.ins().fcmp(FloatCC::Equal, diff, zero)
                    }
                };
                Ok(Some(r))
            }
            NumIntrinsic::IntArith { ty, family, op } => {
                self.emit_int_arith(ty, family, op, args)
            }
        }
    }

    /// A float constant of the same Cranelift type as `like`.
    pub(crate) fn fconst_like(&mut self, like: Value, v: f64) -> Value {
        match self.b.func.dfg.value_type(like) {
            types::F32 => self.b.ins().f32const(v as f32),
            _ => self.b.ins().f64const(v),
        }
    }

    /// The `IntTy` behind a primitive integer `Ty` (after substitution).
    pub(crate) fn int_ty_of(&self, ty: Ty) -> IntTy {
        match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)) {
            TyKind::Int(it) => *it,
            _ => IntTy::I64,
        }
    }

    /// Lower a `{wrapping,saturating,checked,overflowing}_{add,sub,mul,div,
    /// rem,neg,shl,shr}` call. Op codes follow `NumIntrinsic::IntArith`.
    pub(crate) fn emit_int_arith(&mut self, ty: Ty, family: u8, op: u8, args: &[Value]) -> CgResult<Option<Value>> {
        let it = self.int_ty_of(ty);
        let signed = it.is_signed();
        // Arg layout: neg is unary; shl/shr take a `u32` shift; the rest `(T, T)`.
        let a = args[0];
        let b = if op == 5 { a } else { args[1] };
        let r = match op {
            0 | 1 | 2 => self.gen_int_arith_addsubmul(ty, it, signed, family, op, a, b)?,
            3 | 4 => self.gen_int_arith_divrem(ty, it, signed, family, op, a, b)?,
            5 => self.gen_int_arith_neg(ty, it, signed, family, a)?,
            6 | 7 => self.gen_int_arith_shift(ty, it, signed, family, op, a, b)?,
            _ => return Err(CodegenError::new(Span::dummy(), "unknown int arith op")),
        };
        Ok(Some(r))
    }

    /// Wrap a binary-overflow style result `(res, ovf)` according to `family`.
    /// `saturating_clamp` is the value to substitute when overflowing in
    /// saturating mode (callers pre-compute it based on the op's overflow
    /// shape; see add/sub/mul below for the per-op rules).
    fn package_int_arith(
        &mut self,
        family: u8,
        ty: Ty,
        res: Value,
        ovf: Value,
        saturating_clamp: Option<Value>,
    ) -> Value {
        match family {
            0 => res, // wrapping
            1 => {
                let clamp = saturating_clamp.expect("saturating needs a clamp value");
                self.b.ins().select(ovf, clamp, res)
            }
            2 => self.build_checked_union(res, ovf, ty),
            _ => self.build_overflowing_tuple(res, ovf, ty),
        }
    }

    /// Build a `T | null` box: the value boxed as `T` when `ovf` is false,
    /// otherwise a `null`-tagged box (`docs/14` §5).
    fn build_checked_union(&mut self, res: Value, ovf: Value, ty: Ty) -> Value {
        let one = self.b.ins().iconst(types::I8, 1);
        let no_ovf = self.b.ins().bxor(ovf, one);
        let some_bb = self.b.create_block();
        let none_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(no_ovf, some_bb, &[], none_bb, &[]);
        self.term = true;
        self.switch(some_bb);
        let boxed = self.box_value(Some(res), ty);
        self.b.ins().jump(merge, &[boxed.into()]);
        self.term = true;
        self.switch(none_bb);
        let null_ty = self.cx.analysis.tcx.null;
        let nb = self.box_value(None, null_ty);
        self.b.ins().jump(merge, &[nb.into()]);
        self.term = true;
        self.switch(merge);
        self.b.block_params(merge)[0]
    }

    /// Build a `(T, bool)` tuple holding the result and the overflow flag.
    fn build_overflowing_tuple(&mut self, res: Value, ovf: Value, ty: Ty) -> Value {
        let elems = vec![ty, self.cx.analysis.tcx.bool];
        let layout = tuple_layout(self.cx.analysis, &elems);
        let ptr = self.alloc_struct(&layout);
        self.b.ins().store(MemFlags::trusted(), res, ptr, layout.offsets[0] as i32);
        self.b.ins().store(MemFlags::trusted(), ovf, ptr, layout.offsets[1] as i32);
        ptr
    }

    /// `{wrapping,saturating,checked,overflowing}_{add,sub,mul}`. The original
    /// implementation, factored out so the new ops live alongside it.
    fn gen_int_arith_addsubmul(
        &mut self,
        ty: Ty,
        it: IntTy,
        signed: bool,
        family: u8,
        op: u8,
        a: Value,
        b: Value,
    ) -> CgResult<Value> {
        let ct = int_clty(it);
        let (res, ovf) = match (op, signed) {
            (0, true) => self.b.ins().sadd_overflow(a, b),
            (0, false) => self.b.ins().uadd_overflow(a, b),
            (1, true) => self.b.ins().ssub_overflow(a, b),
            (1, false) => self.b.ins().usub_overflow(a, b),
            (2, true) => self.b.ins().smul_overflow(a, b),
            _ => self.b.ins().umul_overflow(a, b),
        };
        let sat_clamp = if family == 1 {
            let (lo, hi) = int_min_max(it);
            let min = self.b.ins().iconst(ct, lo);
            let max = self.b.ins().iconst(ct, hi);
            let zero = self.b.ins().iconst(ct, 0);
            Some(if !signed {
                if op == 1 { min } else { max }
            } else {
                let to_max = match op {
                    0 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero),
                    1 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero),
                    _ => {
                        let an = self.b.ins().icmp(IntCC::SignedLessThan, a, zero);
                        let bn = self.b.ins().icmp(IntCC::SignedLessThan, b, zero);
                        let diff = self.b.ins().bxor(an, bn);
                        let one = self.b.ins().iconst(types::I8, 1);
                        self.b.ins().bxor(diff, one)
                    }
                };
                self.b.ins().select(to_max, max, min)
            })
        } else {
            None
        };
        Ok(self.package_int_arith(family, ty, res, ovf, sat_clamp))
    }

    /// `{wrapping,saturating,checked,overflowing}_{div,rem}`. Divide-by-zero
    /// is a panic for every family except `checked_*`, which folds it into
    /// the null branch (`docs/14` §5). The only real overflow on `div`/`rem`
    /// is signed `INT_MIN / -1` (resp. `INT_MIN % -1` → 0).
    fn gen_int_arith_divrem(
        &mut self,
        ty: Ty,
        it: IntTy,
        signed: bool,
        family: u8,
        op: u8,
        a: Value,
        b: Value,
    ) -> CgResult<Value> {
        let ct = int_clty(it);
        let zero = self.b.ins().iconst(ct, 0);
        let b_zero = self.b.ins().icmp(IntCC::Equal, b, zero);
        // For `checked_div`/`checked_rem`, treat `b == 0` as overflow (null);
        // every other family panics on it (uniform with the operator forms).
        if family != 2 {
            self.guard_panic(b_zero, "divide by zero");
        }
        // Signed `INT_MIN / -1` (or `% -1`) is the only true overflow.
        let (ovf_signed, safe_b) = if signed {
            let (o, sb) = self.div_overflow_select(a, b);
            (o, sb)
        } else {
            let i8z = self.b.ins().iconst(types::I8, 0);
            (i8z, b)
        };
        // For `checked_*`, the divisor must also be sanitised so the hardware
        // op doesn't trap on `0` before we surface it as null. The `ovf` flag
        // combines "true overflow" with "divide by zero" for this family.
        let one = self.b.ins().iconst(ct, 1);
        let safe_b = self.b.ins().select(b_zero, one, safe_b);
        let ovf = if family == 2 {
            self.b.ins().bor(ovf_signed, b_zero)
        } else {
            ovf_signed
        };
        let raw = if op == 3 {
            if signed { self.b.ins().sdiv(a, safe_b) } else { self.b.ins().udiv(a, safe_b) }
        } else if signed {
            self.b.ins().srem(a, safe_b)
        } else {
            self.b.ins().urem(a, safe_b)
        };
        // Real `INT_MIN / -1` wraps to `INT_MIN`; `INT_MIN % -1` to `0`.
        let res = if signed {
            if op == 3 { self.b.ins().select(ovf_signed, a, raw) }
            else { self.b.ins().select(ovf_signed, zero, raw) }
        } else {
            raw
        };
        let sat_clamp = if family == 1 {
            // Saturating signed `INT_MIN / -1` saturates to `INT_MAX`;
            // `INT_MIN % -1` already wraps to `0` (no saturation needed but
            // the package selects between `res` and the clamp under `ovf`,
            // so for rem we still pass `zero` which keeps the result `0`).
            let (_, hi) = int_min_max(it);
            let max = self.b.ins().iconst(ct, hi);
            Some(if op == 3 { max } else { zero })
        } else {
            None
        };
        Ok(self.package_int_arith(family, ty, res, ovf, sat_clamp))
    }

    /// `{wrapping,saturating,checked,overflowing}_neg`. Signed `INT_MIN`
    /// negated overflows back to `INT_MIN`; for unsigned `T`, `neg(a)` is
    /// `0 - a` (wrapping) and overflows whenever `a != 0`.
    fn gen_int_arith_neg(
        &mut self,
        ty: Ty,
        it: IntTy,
        signed: bool,
        family: u8,
        a: Value,
    ) -> CgResult<Value> {
        let ct = int_clty(it);
        let res = self.b.ins().ineg(a);
        let ovf = if signed {
            let (lo, _) = int_min_max(it);
            let min_v = self.b.ins().iconst(ct, lo);
            self.b.ins().icmp(IntCC::Equal, a, min_v)
        } else {
            let zero = self.b.ins().iconst(ct, 0);
            self.b.ins().icmp(IntCC::NotEqual, a, zero)
        };
        let sat_clamp = if family == 1 {
            // Signed saturating neg of `INT_MIN` is `INT_MAX`; unsigned
            // saturates to `0` (the only representable negation).
            Some(if signed {
                let (_, hi) = int_min_max(it);
                self.b.ins().iconst(ct, hi)
            } else {
                self.b.ins().iconst(ct, 0)
            })
        } else {
            None
        };
        Ok(self.package_int_arith(family, ty, res, ovf, sat_clamp))
    }

    /// `{wrapping,saturating,checked,overflowing}_{shl,shr}`. The shift count
    /// is a `u32` (Rust convention); overflow means `count >= BITS`.
    fn gen_int_arith_shift(
        &mut self,
        ty: Ty,
        it: IntTy,
        signed: bool,
        family: u8,
        op: u8,
        a: Value,
        count_u32: Value,
    ) -> CgResult<Value> {
        let ct = int_clty(it);
        let bits = ct.bits() as i64;
        // The shift count is a `u32`; widen/narrow to the value's type for the
        // shift, and keep the original to compare against `BITS`.
        let count_u32_w = match self.b.func.dfg.value_type(count_u32).bits() {
            32 => count_u32,
            _ => self.b.ins().uextend(types::I32, count_u32),
        };
        let bits_i32 = self.b.ins().iconst(types::I32, bits);
        let ovf = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, count_u32_w, bits_i32);
        // Bring the count into the value's type for the hardware shift. The
        // hardware ignores the upper bits past `BITS`, so wrapping behaviour
        // emerges naturally; the explicit `ovf` flag drives the family logic.
        let count_t = if ct == types::I32 {
            count_u32_w
        } else if ct.bits() < 32 {
            self.b.ins().ireduce(ct, count_u32_w)
        } else {
            self.b.ins().uextend(ct, count_u32_w)
        };
        let res = match (op, signed) {
            (6, _) => self.b.ins().ishl(a, count_t),
            (7, true) => self.b.ins().sshr(a, count_t),
            _ => self.b.ins().ushr(a, count_t),
        };
        let sat_clamp = if family == 1 {
            // For an oversized shift count we saturate the count itself to
            // `BITS - 1` and shift by that — a defined fallback when the
            // result would otherwise be undefined / all-zero / all-sign.
            let cap = self.b.ins().iconst(ct, bits - 1);
            let satted = match (op, signed) {
                (6, _) => self.b.ins().ishl(a, cap),
                (7, true) => self.b.ins().sshr(a, cap),
                _ => self.b.ins().ushr(a, cap),
            };
            Some(satted)
        } else {
            None
        };
        Ok(self.package_int_arith(family, ty, res, ovf, sat_clamp))
    }

    /// Builtin `.clone()` over an already-evaluated receiver value `v` of type
    /// `rty`. Shared by the AST and HIR walks.
    pub(crate) fn emit_builtin_clone(&mut self, v: Value, rty: Ty, kind: CloneKind, span: Span)
        -> CgResult<Option<Value>>
    {
        Ok(match kind {
            CloneKind::Identity => {
                // Cloning a channel endpoint registers another live handle so
                // the deterministic close stays balanced (`docs/20` §2): a
                // `Sender.clone()` is a new producer. The clone shares the same
                // handle object (same `chan` id); only the count changes.
                if let Some(is_sender) = self.channel_endpoint_kind(rty) {
                    let chan = self.emit_channel_id(v, rty, span)?;
                    let name = if is_sender {
                        "lang_chan_sender_acquire"
                    } else {
                        "lang_chan_receiver_acquire"
                    };
                    self.call_intrinsic(name, &[types::I64], None, &[chan]);
                }
                Some(v)
            }
            CloneKind::List => self.call_intrinsic("lang_list_clone", &[PTR], Some(PTR), &[v]),
            CloneKind::Map => self.call_intrinsic("lang_map_clone", &[PTR], Some(PTR), &[v]),
            CloneKind::ListDeep => {
                let elem = self.list_elem_of(rty).ok_or_else(|| {
                    CodegenError::new(span, "deep-clone target is not a List")
                })?;
                Some(self.gen_list_clone_deep(v, elem, span)?)
            }
            CloneKind::MapDeep => {
                let (kt, vt) = self.map_kv_of(rty).ok_or_else(|| {
                    CodegenError::new(span, "deep-clone target is not a Map")
                })?;
                Some(self.gen_map_clone_deep(v, kt, vt, span)?)
            }
        })
    }

    /// Recursively clone a value of type `ty`. Drives `CloneKind::*Deep` over
    /// arbitrarily-nested collections and user types (`docs/10`/`docs/15` §8).
    pub(crate) fn gen_clone_value(&mut self, v: Value, ty: Ty, span: Span) -> CgResult<Value> {
        let ty = resolve_shallow(self.cx.analysis, ty, &self.subst);
        // Intrinsic identity — immutable scalars and `str`, plus the
        // thread-shareable `Shared`/`Sender`/`Receiver` handles.
        if matches!(
            self.cx.analysis.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        ) {
            return Ok(v);
        }
        let prog = &self.cx.analysis.program;
        if let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(ty).clone() {
            if def == prog.shared_def {
                return Ok(v);
            }
            // Channel endpoints clone to another live handle for the same
            // channel; bump the matching endpoint count (`docs/20` §2).
            if let Some(is_sender) = self.channel_endpoint_kind(ty) {
                let chan = self.emit_channel_id(v, ty, span)?;
                let name = if is_sender {
                    "lang_chan_sender_acquire"
                } else {
                    "lang_chan_receiver_acquire"
                };
                self.call_intrinsic(name, &[types::I64], None, &[chan]);
                return Ok(v);
            }
        }
        if let Some(elem) = self.list_elem_of(ty) {
            return if is_immutable_value_codegen(self.cx.analysis, elem) {
                Ok(self
                    .call_intrinsic("lang_list_clone", &[PTR], Some(PTR), &[v])
                    .expect("list_clone returns"))
            } else {
                self.gen_list_clone_deep(v, elem, span)
            };
        }
        if let Some((kt, vt)) = self.map_kv_of(ty) {
            return if is_immutable_value_codegen(self.cx.analysis, kt)
                && is_immutable_value_codegen(self.cx.analysis, vt)
            {
                Ok(self
                    .call_intrinsic("lang_map_clone", &[PTR], Some(PTR), &[v])
                    .expect("map_clone returns"))
            } else {
                self.gen_map_clone_deep(v, kt, vt, span)
            };
        }
        // User type: dispatch through its `Clone` impl.
        if let TyKind::Named { def: tdef, args } = self.cx.analysis.tcx.kind(ty).clone() {
            let clone_def = self.cx.analysis.program.clone_def;
            if let Some(&ext) = self.cx.hir.iface_impls.get(&(tdef, clone_def)) {
                let method = (0..self.cx.analysis.program.defs.len() as u32)
                    .map(DefId)
                    .find(|&d| {
                        let de = self.cx.analysis.program.def(d);
                        de.kind == DefKind::ExtendMethod && de.parent == Some(ext) && de.name == "clone"
                    })
                    .ok_or_else(|| CodegenError::new(span, "Clone impl has no `clone` method"))?;
                let targs = if self.cx.analysis.program.def(ext).generics.is_empty() {
                    Vec::new()
                } else {
                    args
                };
                return self
                    .emit_call(method, targs, &[v], span)?
                    .ok_or_else(|| CodegenError::new(span, "`clone` returned no value"));
            }
        }
        Err(CodegenError::new(span, "no Clone impl for this type"))
    }

    /// Deep-clone a `List<T>`: allocate a fresh list and push each cloned
    /// element. Used when `T` is mutable but implements `Clone`.
    fn gen_list_clone_deep(&mut self, src: Value, elem: Ty, span: Span) -> CgResult<Value> {
        let elem_is_ptr = if is_managed_ptr(self.cx.analysis, elem) { 1 } else { 0 };
        let flag = self.b.ins().iconst(types::I64, elem_is_ptr);
        let new_list = self
            .call_intrinsic("lang_list_new", &[types::I64], Some(PTR), &[flag])
            .expect("list_new returns");
        // Pin the source AND the destination across the per-element clone
        // (each may itself allocate).
        self.mark_root(src);
        self.mark_root(new_list);
        self.list_for_each(src, elem, span, |this, ev| {
            let cloned = this.gen_clone_value(ev, elem, span)?;
            let raw = this.elem_to_i64(Some(cloned), elem, span)?;
            this.call_intrinsic(
                "lang_list_push",
                &[PTR, types::I64],
                None,
                &[new_list, raw],
            );
            Ok(())
        })?;
        Ok(new_list)
    }

    /// Deep-clone a `Map<K, V>`: build a fresh map (same key-ops) and write
    /// each `(k, clone(v))` pair. Keys are immutable so they share by value;
    /// values use `gen_clone_value` for per-element deep clone.
    fn gen_map_clone_deep(&mut self, src: Value, kt: Ty, vt: Ty, span: Span) -> CgResult<Value> {
        let new_map = self.gen_map_new(kt, vt);
        self.mark_root(src);
        self.mark_root(new_map);
        // Snapshot the key list.
        let one = self.b.ins().iconst(types::I64, 1);
        let keys = self
            .call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[src, one])
            .expect("map_entries returns");
        self.mark_root(keys);
        self.list_for_each(keys, kt, span, |this, kv| {
            let kraw = this.elem_to_i64(Some(kv), kt, span)?;
            // Look up the value in the source map for this key.
            let vraw = this
                .call_intrinsic(
                    "lang_map_index",
                    &[PTR, types::I64],
                    Some(types::I64),
                    &[src, kraw],
                )
                .expect("map_index returns");
            let vval = this
                .i64_to_elem(vraw, vt, span)?
                .ok_or_else(|| CodegenError::new(span, "map value is zero-sized"))?;
            let cloned = this.gen_clone_value(vval, vt, span)?;
            let craw = this.elem_to_i64(Some(cloned), vt, span)?;
            this.call_intrinsic(
                "lang_map_set",
                &[PTR, types::I64, types::I64],
                None,
                &[new_map, kraw, craw],
            );
            Ok(())
        })?;
        Ok(new_map)
    }

    /// The intrinsic [`CloneKind`] for a builtin receiver type, or `None` for a
    /// user type (which clones through its own `Clone` impl). Mirrors the
    /// checker's `check_builtin_clone` so monomorphized `T: Clone` dispatch
    /// agrees with direct `.clone()` calls.
    pub(crate) fn builtin_clone_kind(&self, ty: Ty) -> Option<CloneKind> {
        if matches!(
            self.cx.analysis.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        ) {
            return Some(CloneKind::Identity);
        }
        if self.list_elem_of(ty).is_some() {
            return Some(CloneKind::List);
        }
        if self.map_kv_of(ty).is_some() {
            return Some(CloneKind::Map);
        }
        None
    }

    /// Whether `ty` has an intrinsic `Eq`/`Ord` comparison (a primitive scalar
    /// or `str`); user types compare through their own `extend … : Eq`/`: Ord`.
    pub(crate) fn is_primitive_comparable(&self, ty: Ty) -> bool {
        matches!(
            self.cx.analysis.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str
        )
    }

    /// Emit a `Hash.hash` intrinsic call for a primitive or `str` receiver
    /// (`docs/15` §7). Narrow integer-shaped values widen to `i64` (we hash by
    /// bit pattern, so unsigned widening is correct for both signs); floats
    /// dispatch to `lang_hash_f64`, strings to `lang_hash_str`.
    pub(crate) fn gen_primitive_hash(&mut self, v: Value, ty: Ty) -> Value {
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Str => self
                .call_intrinsic("lang_hash_str", &[PTR], Some(types::I64), &[v])
                .expect("lang_hash_str returns a value"),
            TyKind::Float(f) => {
                let v64 = if matches!(f, FloatTy::F32) {
                    self.b.ins().fpromote(types::F64, v)
                } else {
                    v
                };
                self.call_intrinsic("lang_hash_f64", &[types::F64], Some(types::I64), &[v64])
                    .expect("lang_hash_f64 returns a value")
            }
            _ => {
                let val_ty = self.b.func.dfg.value_type(v);
                let v64 = if val_ty == types::I64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                };
                self.call_intrinsic("lang_hash_i64", &[types::I64], Some(types::I64), &[v64])
                    .expect("lang_hash_i64 returns a value")
            }
        }
    }

    /// Emit a primitive/`str` comparison over already-evaluated operand values.
    /// Shared by the AST and HIR walks.
    pub(crate) fn emit_primitive_compare(&mut self, op: BinaryOp, recv_ty: Ty, l: Value, r: Value)
        -> CgResult<Option<Value>>
    {
        if matches!(self.cx.analysis.tcx.kind(recv_ty), TyKind::Str) {
            return Ok(Some(self.gen_str_compare(op, l, r)));
        }
        let (is_float, signed) = match self.cx.analysis.tcx.kind(recv_ty) {
            TyKind::Float(_) => (true, true),
            TyKind::Int(it) => (false, it.is_signed()),
            // `char` and `bool` compare as unsigned integers.
            _ => (false, false),
        };
        Ok(Some(self.gen_compare(op, is_float, signed, l, r)))
    }

    /// `Thread.spawn` over an already-evaluated closure env value. Shared by the
    /// AST and HIR walks. When `is_async` the closure is `() => Future<R>`: the
    /// worker drives the future to completion (`docs/20` §1), so we call
    /// `lang_thread_spawn_async` (passing the `Pending` type id its `block_on`
    /// needs) instead of `lang_thread_spawn`.
    pub(crate) fn emit_thread_spawn(
        &mut self,
        env: Value,
        r: Ty,
        is_async: bool,
        _span: Span,
    ) -> CgResult<Option<Value>> {
        let id = if is_async {
            // `block_on` carries the awaited `R` as its raw bits (a float is its
            // own bit pattern), so no `float_kind` is needed here (`docs/20` §1).
            let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            self.call_intrinsic(
                "lang_thread_spawn_async",
                &[PTR, types::I64],
                Some(types::I64),
                &[env, pt],
            )
            .expect("lang_thread_spawn_async returns an id")
        } else {
            // A float result is returned in a floating-point register; tell the
            // runtime which result ABI the lifted closure uses so it reads the
            // value from the right register and carries its raw bits (`docs/20`).
            let r_res = resolve_shallow(self.cx.analysis, r, &self.subst);
            let float_kind = match self.cx.analysis.tcx.kind(r_res) {
                TyKind::Float(FloatTy::F64) => 8,
                TyKind::Float(FloatTy::F32) => 4,
                _ => 0,
            };
            let fk = self.b.ins().iconst(types::I64, float_kind);
            self.call_intrinsic("lang_thread_spawn", &[PTR, types::I64], Some(types::I64), &[env, fk])
                .expect("lang_thread_spawn returns an id")
        };
        let jh_def = self.cx.analysis.program.join_handle_def;
        let layout = self.struct_layout(jh_def, &[r]);
        let ptr = self.alloc_struct(&layout);
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), id, ptr, off);
        // Pin the handle as a global root for its lifetime: it may be held on a
        // thread whose stack a collector cannot perfectly reconstruct, and it is
        // tiny, so pinning until `join` is both simple and robust (`docs/20`).
        self.call_intrinsic("lang_gc_pin", &[PTR], None, &[ptr]);
        Ok(Some(ptr))
    }

    /// `JoinHandle<R>.join()` over an already-evaluated handle value. Shared by
    /// the AST and HIR walks.
    pub(crate) fn emit_thread_join(&mut self, jh: Value, r: Ty, _span: Span) -> CgResult<Option<Value>> {
        let jh_def = self.cx.analysis.program.join_handle_def;
        let jh_layout = self.struct_layout(jh_def, &[r]);
        let id_off = jh_layout.offsets[jh_layout.index_of("id").unwrap_or(0)] as i32;
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), jh, id_off);
        // The handle is consumed by `join`; unpin it from the global roots.
        self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[jh]);

        // Tids the runtime's `thread_join_poll` needs to build the result box:
        // the outer `Ready<Out> | Pending` union tags + the inner
        // `Joined<R> | Panicked` variant tags. Tids follow the language-wide
        // `1000 + def.index()` convention.
        let prog = &self.cx.analysis.program;
        let ready_tid = 1000 + prog.ready_def.index() as i64;
        let pending_tid = 1000 + prog.pending_def.index() as i64;
        let joined_tid = 1000 + prog.joined_def.index() as i64;
        let panicked_tid = 1000 + prog.panicked_def.index() as i64;
        let rt = self.b.ins().iconst(types::I64, ready_tid);
        let pt = self.b.ins().iconst(types::I64, pending_tid);
        let jt = self.b.ins().iconst(types::I64, joined_tid);
        let pkt = self.b.ins().iconst(types::I64, panicked_tid);
        // GC needs to know whether the `Joined<R>.value` slot should be traced.
        let r_res = resolve_shallow(self.cx.analysis, r, &self.subst);
        let is_ptr = is_managed_ptr(self.cx.analysis, r_res) as i64;
        let ip = self.b.ins().iconst(types::I64, is_ptr);
        let fut = self
            .call_intrinsic(
                "lang_thread_join_future",
                &[types::I64, types::I64, types::I64, types::I64, types::I64, types::I64],
                Some(PTR),
                &[id, rt, pt, jt, pkt, ip],
            )
            .expect("join future");
        Ok(Some(fut))
    }

    /// `JoinHandle<R>.detach()` over an already-evaluated handle value
    /// (`docs/20` §1): unpin the handle from the global roots and relinquish the
    /// worker (fire-and-forget). Yields no value (`null`).
    pub(crate) fn emit_thread_detach(&mut self, jh: Value, r: Ty, _span: Span) -> CgResult<Option<Value>> {
        let jh_def = self.cx.analysis.program.join_handle_def;
        let jh_layout = self.struct_layout(jh_def, &[r]);
        let id_off = jh_layout.offsets[jh_layout.index_of("id").unwrap_or(0)] as i32;
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), jh, id_off);
        // The handle is consumed by `detach`; unpin it from the global roots.
        self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[jh]);
        self.call_intrinsic("lang_thread_detach", &[types::I64], None, &[id]);
        Ok(None)
    }

    /// Lower `channel<T>()` (`docs/20` §2): allocate a runtime channel and build
    /// the `(Sender<T>, Receiver<T>)` tuple, both carrying the channel id.
    pub(crate) fn gen_channel_new(&mut self, result_ty: Ty, span: Span) -> CgResult<Option<Value>> {
        let id = self.call_intrinsic("lang_channel_new", &[], Some(types::I64), &[])
            .expect("channel id");
        // The `(Sender<T>, Receiver<T>)` tuple type rides on the intrinsic node.
        let elem_tys = match self.cx.analysis.tcx.kind(result_ty).clone() {
            TyKind::Tuple(ts) => ts,
            _ => return Err(CodegenError::new(span, "`channel` result is not a tuple")),
        };
        let sender = self.build_channel_end(elem_tys[0], id, span)?;
        self.mark_root(sender);
        let receiver = self.build_channel_end(elem_tys[1], id, span)?;
        self.mark_root(receiver);
        let layout = tuple_layout(self.cx.analysis, &elem_tys);
        let tup = self.alloc_struct(&layout);
        self.b.ins().store(MemFlags::trusted(), sender, tup, layout.offsets[0] as i32);
        self.b.ins().store(MemFlags::trusted(), receiver, tup, layout.offsets[1] as i32);
        Ok(Some(tup))
    }

    /// Allocate a `Sender<T>`/`Receiver<T>` struct holding the channel `id`.
    pub(crate) fn build_channel_end(&mut self, end_ty: Ty, id: Value, span: Span) -> CgResult<Value> {
        let resolved = resolve_shallow(self.cx.analysis, end_ty, &self.subst);
        let (def, args) = match self.cx.analysis.tcx.kind(resolved).clone() {
            TyKind::Named { def, args } => (def, args),
            _ => return Err(CodegenError::new(span, "channel end is not a struct")),
        };
        let layout = self.struct_layout(def, &args);
        let p = self.alloc_struct(&layout);
        let off = layout.offsets[layout.index_of("chan").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), id, p, off);
        Ok(p)
    }

    /// `Sender<T>.send(v)` over an already-read channel id and value. Returns
    /// `null | ChannelClosed` (`docs/20` §2): the runtime reports `1` when every
    /// receiver has been dropped (the message is not enqueued), `0` otherwise.
    /// `result_ty` is the `null | ChannelClosed` union the call evaluates to.
    pub(crate) fn emit_channel_send(
        &mut self,
        chan: Value,
        elem: Ty,
        v: Option<Value>,
        result_ty: Ty,
        span: Span,
    ) -> CgResult<Option<Value>> {
        let raw = self.elem_to_i64(v, elem, span)?;
        let status = self
            .call_intrinsic("lang_chan_send", &[types::I64, types::I64], Some(types::I64), &[chan, raw])
            .expect("send status");
        // Identify the `ChannelClosed` variant of the result union to box on a
        // closed send; the success case is `null`.
        let closed_ty = self.union_variant_ty(result_ty, |a, ty| {
            matches!(a.tcx.kind(ty), TyKind::Named { def, .. }
                if *def == a.program.channel_closed_def)
        });
        let zero = self.b.ins().iconst(types::I64, 0);
        let is_closed = self.b.ins().icmp(IntCC::NotEqual, status, zero);
        let ok_bb = self.b.create_block();
        let closed_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(is_closed, closed_bb, &[], ok_bb, &[]);
        self.term = true;
        // Success: `null` boxed into the union (a tagged box, not a raw null —
        // so `match`/`is` dispatch works), mirroring `try_recv`'s null branch.
        self.switch(ok_bb);
        let null_ty = self.cx.analysis.tcx.null;
        let ok_box = self.box_value(None, null_ty);
        self.b.ins().jump(merge, &[ok_box.into()]);
        self.term = true;
        // Closed: `ChannelClosed` (a unit struct) boxed into the union.
        self.switch(closed_bb);
        let closed_box = self.box_value(None, closed_ty);
        self.b.ins().jump(merge, &[closed_box.into()]);
        self.term = true;
        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Find the union variant of `ty` matching `pred`, or `ty` itself if it is
    /// not a union (single-variant). Used to recover a specific variant type for
    /// boxing a runtime-produced value into a union.
    fn union_variant_ty(
        &self,
        ty: Ty,
        pred: impl Fn(&Analysis, Ty) -> bool,
    ) -> Ty {
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        if let TyKind::Union(vs) = self.cx.analysis.tcx.kind(resolved).clone() {
            for v in vs {
                if pred(self.cx.analysis, v) {
                    return v;
                }
            }
        }
        ty
    }

    /// `Receiver<T>.recv()`/`.try_recv()` over an already-read channel id. Shared.
    pub(crate) fn emit_channel_recv(&mut self, chan: Value, elem: Ty, method: &str, span: Span)
        -> CgResult<Option<Value>>
    {
        if method == "recv" {
            // Async recv: build a `Future<T | ChannelClosed>` interface-object
            // box. The runtime future's `poll` pops a message (→ `Ready<T>`
            // variant), reports `ChannelClosed` once drained + all senders gone,
            // or registers the executor waker and reports `Pending` (`docs/20`
            // §2 / `docs/21`). It needs the `Ready`/`Pending` tids plus the tids
            // tagging the resolved union's `T` and `ChannelClosed` variants.
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let closed_tid = 1000 + prog.channel_closed_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            let resolved = resolve_shallow(self.cx.analysis, elem, &self.subst);
            let is_ptr = is_managed_ptr(self.cx.analysis, resolved) as i64;
            let ip = self.b.ins().iconst(types::I64, is_ptr);
            let value_tid = self.type_id_of(elem);
            let vt = self.b.ins().iconst(types::I64, value_tid);
            let ct = self.b.ins().iconst(types::I64, closed_tid);
            let fut = self.call_intrinsic(
                "lang_chan_recv_future",
                &[types::I64, types::I64, types::I64, types::I64, types::I64, types::I64],
                Some(PTR),
                &[chan, rt, pt, ip, vt, ct],
            ).expect("recv future");
            return Ok(Some(fut));
        }
        // try_recv: returns `T | null` — null when the queue is empty.
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot, 8, 3,
        ));
        let has_ptr = self.b.ins().stack_addr(PTR, slot, 0);
        let raw = self.call_intrinsic(
            "lang_chan_try_recv", &[types::I64, PTR], Some(types::I64), &[chan, has_ptr],
        ).expect("try_recv value");
        let has = self.b.ins().load(types::I64, MemFlags::trusted(), has_ptr, 0);
        let zero = self.b.ins().iconst(types::I64, 0);
        let got = self.b.ins().icmp(IntCC::NotEqual, has, zero);
        // Build the `T | null` union: a value box when present, else a null ptr.
        let some_bb = self.b.create_block();
        let none_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(got, some_bb, &[], none_bb, &[]);
        self.term = true;
        self.switch(some_bb);
        let val = self.i64_to_elem(raw, elem, span)?;
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, elem, &self.subst)) {
            if let Some(v) = val { self.mark_root(v); }
        }
        let boxed = self.box_value(val, elem);
        self.b.ins().jump(merge, &[boxed.into()]);
        self.term = true;
        self.switch(none_bb);
        // The empty case is `null` *boxed into the union* (a box tagged with the
        // null type id), not a raw null pointer — so `match`/`is` dispatch works.
        let null_ty = self.cx.analysis.tcx.null;
        let null_box = self.box_value(None, null_ty);
        self.b.ins().jump(merge, &[null_box.into()]);
        self.term = true;
        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Classify a type as a channel endpoint handle: `Some(true)` for a
    /// `Sender<T>`, `Some(false)` for a `Receiver<T>`, `None` otherwise. Used to
    /// emit deterministic acquire/release of the channel's endpoint reference
    /// counts (`docs/16` §8 / `docs/20` §2).
    pub(crate) fn channel_endpoint_kind(&self, ty: Ty) -> Option<bool> {
        let prog = &self.cx.analysis.program;
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        match self.cx.analysis.tcx.kind(resolved) {
            TyKind::Named { def, .. } if *def == prog.sender_def && prog.sender_def != DefId(0) => {
                Some(true)
            }
            TyKind::Named { def, .. }
                if *def == prog.receiver_def && prog.receiver_def != DefId(0) =>
            {
                Some(false)
            }
            _ => None,
        }
    }

    /// Read the `chan` id field from an already-evaluated `Sender`/`Receiver`
    /// struct value. Shared by the AST and HIR walks.
    pub(crate) fn emit_channel_id(&mut self, ptr: Value, rty: Ty, span: Span) -> CgResult<Value> {
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(span, "channel end is not a struct"))?;
        let off = layout.offsets[layout.index_of("chan").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// `Shared.new(v)` over an already-evaluated value. Shared by both walks.
    pub(crate) fn emit_shared_new(&mut self, v: Option<Value>, elem: Ty, result_ty: Ty, span: Span)
        -> CgResult<Option<Value>>
    {
        let raw = self.elem_to_i64(v, elem, span)?;
        let id = self.call_intrinsic("lang_shared_new", &[types::I64], Some(types::I64), &[raw])
            .expect("shared id");
        let shared = self.build_channel_end(result_ty, id, span)?; // {id} struct, same shape
        Ok(Some(shared))
    }

    /// `Shared<T>.lock(body)` / `.try_lock(body)` (`docs/20` §4): build the async
    /// lock future over an already-read mutex `id` and already-built body-closure
    /// `env`. The future acquires the lock (suspending the *task* — never the OS
    /// thread — while contended for `lock`; resolving to `LockBusy` on a failed
    /// `try_lock`), runs the body under the lock — driving it to completion if
    /// `body_is_async`, so the lock is HELD across the body's `await`s — clones the
    /// result out *while held* (detachment rule), releases, and resolves to `R`
    /// (or `R | LockBusy`). The caller's `await` drives the returned `Future`.
    pub(crate) fn emit_shared_lock(
        &mut self,
        id: Value,
        elem: Ty,
        method: &str,
        env: Value,
        r_ty: Ty,
        body_is_async: bool,
        span: Span,
    ) -> CgResult<Option<Value>> {
        let prog = &self.cx.analysis.program;
        let ready_tid = 1000 + prog.ready_def.index() as i64;
        let pending_tid = 1000 + prog.pending_def.index() as i64;
        let busy_def = prog.lock_busy_def;
        let is_try = method == "try_lock";

        // The body closure is invoked as `fn(env, value) -> R` over a uniform
        // integer/pointer ABI; a float-typed protected value or body result would
        // travel in a different register class — reject it with a clear error
        // rather than miscompile (wrap the float in a struct).
        let elem_res = resolve_shallow(self.cx.analysis, elem, &self.subst);
        let r_res = resolve_shallow(self.cx.analysis, r_ty, &self.subst);
        let is_float = |k: &TyKind| matches!(k, TyKind::Float(_));
        if is_float(self.cx.analysis.tcx.kind(elem_res)) || is_float(self.cx.analysis.tcx.kind(r_res)) {
            return Err(CodegenError::new(
                span,
                "locking a `Shared` whose value or body result is a float is not yet \
                 supported; wrap it in a struct",
            ));
        }

        let r_is_ptr = is_managed_ptr(self.cx.analysis, r_res);
        // Clone-out thunk for a managed `R` (the detachment-rule deep copy); a
        // non-pointer `R` aliases nothing, so no clone is needed.
        let clone_fn = if r_is_ptr {
            self.emit_clone_thunk(r_ty, span)?
        } else {
            self.b.ins().iconst(PTR, 0)
        };
        let r_tid = if is_try { self.type_id_of(r_ty) } else { 0 };
        let busy_tid = if is_try { 1000 + busy_def.index() as i64 } else { 0 };

        let body_async_v = self.b.ins().iconst(types::I64, body_is_async as i64);
        let r_is_ptr_v = self.b.ins().iconst(types::I64, r_is_ptr as i64);
        let is_try_v = self.b.ins().iconst(types::I64, is_try as i64);
        let r_tid_v = self.b.ins().iconst(types::I64, r_tid);
        let busy_tid_v = self.b.ins().iconst(types::I64, busy_tid);
        let ready_v = self.b.ins().iconst(types::I64, ready_tid);
        let pending_v = self.b.ins().iconst(types::I64, pending_tid);

        Ok(self.call_intrinsic(
            "lang_shared_lock_future",
            &[
                types::I64, PTR, PTR, types::I64, types::I64, types::I64, types::I64,
                types::I64, types::I64, types::I64,
            ],
            Some(PTR),
            &[
                id, env, clone_fn, body_async_v, r_is_ptr_v, is_try_v, r_tid_v, busy_tid_v,
                ready_v, pending_v,
            ],
        ))
    }

    /// Declare a `Shared` lock-body clone-out thunk for `r_ty` (`extern "C" fn(R)
    /// -> R` deep-cloning its argument) and queue its definition; returns its
    /// address as a value for the lock future to call (`docs/20` §4).
    pub(crate) fn emit_clone_thunk(&mut self, r_ty: Ty, span: Span) -> CgResult<Value> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        let name = format!("clonethunk.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let fid = self
            .module
            .declare_function(&name, Linkage::Local, &sig)
            .expect("declare clone thunk");
        self.clone_thunks.push(crate::CloneThunkJob {
            func_id: fid,
            r_ty,
            subst: self.subst.clone(),
            span,
        });
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        Ok(self.b.ins().func_addr(PTR, fref))
    }

    /// Read the `id` field from an already-evaluated `Shared` struct value.
    pub(crate) fn emit_shared_id(&mut self, ptr: Value, rty: Ty, span: Span) -> CgResult<Value> {
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(span, "`Shared` is not a struct"))?;
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// Declare a lifted closure function `(env, params…) -> ret` and build its
    /// heap environment `[fn_ptr][cap0_cell]…`, returning `(func id, env value)`.
    /// Shared by the AST and HIR closure paths; the caller queues the body in
    /// the matching IR via a [`crate::ClosureJob`].
    pub(crate) fn emit_closure_value(
        &mut self,
        info: &compiler::sema::results::ClosureInfo,
        span: Span,
    ) -> CgResult<(FuncId, Value)> {
        self.emit_closure_value_kind(info, false, span)
    }

    /// As [`Self::emit_closure_value`], but `by_value` selects the capture
    /// discipline. `false` (ordinary closures, `docs/09` §7): every capture is
    /// by reference — the env slot stores the captured local's *cell pointer*,
    /// so mutations are shared with the outer scope. `true` (`Thread.spawn`
    /// closures, `docs/20` §6 cross-thread isolation): every capture is an
    /// independent *value snapshot* taken at the spawn site — the env slot
    /// stores the captured value itself (a primitive copy, or an immutable
    /// managed pointer), so the worker never shares a mutable cell with the
    /// spawner. Only managed value slots are GC-traced in the by-value layout.
    pub(crate) fn emit_closure_value_kind(
        &mut self,
        info: &compiler::sema::results::ClosureInfo,
        by_value: bool,
        span: Span,
    ) -> CgResult<(FuncId, Value)> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        for (_, ty) in &info.params {
            let ct = self.cx_clty(*ty)
                .ok_or_else(|| CodegenError::new(span, "closure parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        if let Some(rc) = self.cx_clty(info.ret) {
            sig.returns.push(AbiParam::new(rc));
        }
        let name = format!("closure.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let func_id = self.module.declare_function(&name, Linkage::Local, &sig)
            .expect("declare closure");

        // Environment layout: [fn_ptr][cap0][cap1]… By reference, each cap slot
        // holds a managed cell pointer (all traced). By value, each slot holds
        // the captured value, so only managed-typed slots are traced.
        let n = info.captures.len();
        let size = (8 + n * 8) as u32;
        let ptr_offsets: Vec<u32> = if by_value {
            info.captures
                .iter()
                .enumerate()
                .filter(|(_, (_, ty))| {
                    let r = resolve_shallow(self.cx.analysis, *ty, &self.subst);
                    is_managed_ptr(self.cx.analysis, r)
                })
                .map(|(k, _)| (8 + k * 8) as u32)
                .collect()
        } else {
            (0..n).map(|k| (8 + k * 8) as u32).collect()
        };
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let env = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let fref = self.module.declare_func_in_func(func_id, self.b.func);
        let faddr = self.b.ins().func_addr(PTR, fref);
        self.b.ins().store(MemFlags::trusted(), faddr, env, 0);
        for (k, (local, _)) in info.captures.iter().enumerate() {
            let v = if by_value {
                // Snapshot the captured local's current value (loading through
                // its cell if it is cell-backed in the outer scope).
                self.read_local(*local)
                    .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?
            } else {
                // Capture-by-reference: the cell pointer held in the var.
                let var = *self.vars.get(local)
                    .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
                self.b.use_var(var)
            };
            self.b.ins().store(MemFlags::trusted(), v, env, (8 + k * 8) as i32);
        }
        Ok((func_id, env))
    }

    /// A named function used as a first-class value (`docs/09` §4): wrap it in a
    /// closure-style environment `[thunk_ptr]`. The thunk is a lifted closure
    /// `(env, params…) -> ret` whose body simply forwards its parameters to a
    /// `Direct` call of the function — adapting the function's `(params…) -> ret`
    /// ABI to the uniform closure-call ABI. Because it reuses the closure path,
    /// such a value is callable, storable, and passable exactly like a closure.
    pub(crate) fn emit_fn_value(&mut self, def: DefId, ty: Ty, span: Span) -> CgResult<Value> {
        use compiler::hir;
        if !self.cx.analysis.program.def(def).generics.is_empty() {
            return Err(CodegenError::new(
                span,
                "a generic function cannot yet be used as a first-class value; \
                 call it directly or wrap it in a closure",
            ));
        }
        let fsig = self.cx.analysis.hir.fn_sigs.get(&def).ok_or_else(|| {
            CodegenError::new(span, "function used as a value has no signature")
        })?;
        let params: Vec<(LocalId, Ty)> = fsig.params.clone();
        let ret = fsig.ret;
        // Synthetic body: `def(p0, p1, …)` forwarding each parameter local.
        let args: Vec<hir::Expr> = params
            .iter()
            .map(|(local, pty)| hir::Expr {
                kind: hir::ExprKind::Name(hir::Res::Local(*local)),
                ty: *pty,
                span,
            })
            .collect();
        let body = hir::Expr {
            kind: hir::ExprKind::Call {
                kind: hir::CallKind::Direct { def, type_args: vec![] },
                args,
                callee_span: span,
                callee_ty: ty,
            },
            ty: ret,
            span,
        };
        let info = compiler::sema::results::ClosureInfo { params, captures: vec![], ret };
        let (func_id, env) = self.emit_closure_value(&info, span)?;
        self.closures.push(crate::ClosureJob {
            func_id,
            info,
            body,
            subst: self.subst.clone(),
            span,
            by_value: false,
        });
        Ok(env)
    }

    /// Call a closure `env` value with already-evaluated arguments: load its
    /// function pointer (offset 0) and call indirectly, passing the env first.
    pub(crate) fn emit_closure_call(&mut self, env: Value, args: &[Value], ret_clty: Option<ClType>) -> Option<Value> {
        self.mark_root(env);
        let fnptr = self.b.ins().load(PTR, MemFlags::trusted(), env, 0);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR)); // env
        let mut arg_vals = vec![env];
        for &v in args {
            sig.params.push(AbiParam::new(self.b.func.dfg.value_type(v)));
            arg_vals.push(v);
        }
        if let Some(rc) = ret_clty {
            sig.returns.push(AbiParam::new(rc));
        }
        let sigref = self.b.import_signature(sig);
        let call = self.b.ins().call_indirect(sigref, fnptr, &arg_vals);
        self.b.inst_results(call).first().copied()
    }

    /// Index of an interface method within its interface (its vtable slot).
    pub(crate) fn vtable_slot(&self, iface_method: DefId) -> Option<usize> {
        let prog = &self.cx.analysis.program;
        let iface = prog.def(iface_method).parent?;
        (0..prog.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let de = prog.def(d);
                de.kind == DefKind::InterfaceMethod && de.parent == Some(iface)
            })
            .position(|d| d == iface_method)
    }

    /// Emit an indirect call through an interface object's vtable: `obj` is the
    /// `{vtable, data}` box, `slot` the method index, `args` the (already
    /// evaluated) non-self arguments.
    pub(crate) fn emit_vtable_call(
        &mut self,
        slot: usize,
        obj: Value,
        args: &[Value],
        ret_clty: Option<ClType>,
    ) -> CgResult<Option<Value>> {
        self.mark_root(obj);
        let vtable = self.b.ins().load(PTR, MemFlags::trusted(), obj, 0);
        let fnptr = self.b.ins().load(PTR, MemFlags::trusted(), vtable, (slot * 8) as i32);
        let data = self.b.ins().load(PTR, MemFlags::trusted(), obj, 8);

        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR)); // self (data pointer)
        let mut arg_vals = vec![data];
        for &v in args {
            sig.params.push(AbiParam::new(self.b.func.dfg.value_type(v)));
            arg_vals.push(v);
        }
        if let Some(r) = ret_clty {
            sig.returns.push(AbiParam::new(r));
        }
        let sigref = self.b.import_signature(sig);
        let call = self.b.ins().call_indirect(sigref, fnptr, &arg_vals);
        Ok(self.b.inst_results(call).first().copied())
    }

    /// Resolve an interface method to the concrete `extend` method for the
    /// receiver's (monomorphized) type, plus the extend's type arguments.
    pub(crate) fn resolve_iface_method(&self, iface_method: DefId, recv: Ty) -> Option<(DefId, Vec<Ty>)> {
        let prog = &self.cx.analysis.program;
        let iface = prog.def(iface_method).parent?;
        let mname = prog.def(iface_method).name.clone();
        let recv = resolve_shallow(self.cx.analysis, recv, &self.subst);
        let TyKind::Named { def: cdef, args } = self.cx.analysis.tcx.kind(recv).clone() else {
            return None;
        };
        let ext = self.cx.hir.iface_impls.get(&(cdef, iface)).copied()?;
        let method = (0..prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = prog.def(d);
            def.kind == DefKind::ExtendMethod && def.parent == Some(ext) && def.name == mname
        })?;
        // A generic `extend Name<P0, …>` takes the receiver's type arguments in
        // order (the common form); a concrete `extend` takes none.
        let targs = if prog.def(ext).generics.is_empty() { Vec::new() } else { args };
        Some((method, targs))
    }

    /// Emit a direct call to a compiled instance, declaring it on demand.
    pub(crate) fn emit_call(
        &mut self,
        def: DefId,
        type_args: Vec<Ty>,
        arg_vals: &[Value],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let func_id = match self.funcs.get(&(def, type_args.clone())).copied() {
            Some(f) => f,
            None => declare_instance(
                self.module,
                self.funcs,
                self.worklist,
                self.cx.analysis,
                def,
                type_args,
            )?
            .ok_or_else(|| CodegenError::new(span, "callee is not lowerable"))?,
        };
        let func_ref = self.module.declare_func_in_func(func_id, self.b.func);
        let inst = self.b.ins().call(func_ref, arg_vals);
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// Emit a cooperative GC safepoint poll (`docs/20`). Placed at loop headers
    /// so a compute-bound thread reaches a safepoint promptly when another
    /// thread requests a stop-the-world collection. Cheap on the common path
    /// (a flag load + branch inside the runtime).
    pub(crate) fn emit_safepoint(&mut self) {
        self.call_intrinsic("lang_gc_safepoint", &[], None, &[]);
    }

    /// Terminate the current block after a `never`-returning call: emit a trap
    /// (the runtime call does not return) and mark the block terminated.
    pub(crate) fn emit_unreachable(&mut self) {
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;
    }

}

/// Map an `Eq`/`Ord` method name to the binary comparison operator it stands
/// for, or `None` if the name is not a comparison method.
pub(crate) fn compare_op(name: &str) -> Option<BinaryOp> {
    Some(match name {
        "eq" => BinaryOp::Eq,
        "lt" => BinaryOp::Lt,
        "le" => BinaryOp::Le,
        "gt" => BinaryOp::Gt,
        "ge" => BinaryOp::Ge,
        _ => return None,
    })
}
