//! Per-function codegen: runtime intrinsics, string literals, casts, and
//! union/interface narrowing (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- runtime intrinsics, strings, casts ---------------------------------

    /// Declare (idempotently) and call a runtime intrinsic by symbol name.
    pub(crate) fn call_intrinsic(
        &mut self,
        name: &str,
        params: &[ClType],
        ret: Option<ClType>,
        args: &[Value],
    ) -> Option<Value> {
        let mut sig = self.module.make_signature();
        for p in params {
            sig.params.push(AbiParam::new(*p));
        }
        if let Some(r) = ret {
            sig.returns.push(AbiParam::new(r));
        }
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .expect("declare intrinsic");
        let fref = self.module.declare_func_in_func(id, self.b.func);
        let inst = self.b.ins().call(fref, args);
        self.b.inst_results(inst).first().copied()
    }

    /// Lower a string literal to a `str` pointer. Interpolation is not yet
    /// lowerable; text parts have their escapes processed.
    pub(crate) fn gen_str_literal(&mut self, s: &StringLit) -> CgResult<Value> {
        // Interpolation desugars to a chain of `+` (concat) over each part's
        // `to_str` (`docs/01` §8). Each part becomes one `str` value.
        let mut parts: Vec<Value> = Vec::new();
        for part in &s.parts {
            match part {
                StringPart::Text { text, .. } => {
                    let mut bytes = Vec::new();
                    unescape_into(text, &mut bytes);
                    parts.push(self.emit_str_bytes(bytes));
                }
                StringPart::Ident(id) => {
                    let ty = self.cx.analysis.results.expr_ty(id.span)
                        .unwrap_or(self.cx.analysis.tcx.error);
                    let raw = self.gen_local_use(id.span)?;
                    // Apply narrowing/widening recorded for this use.
                    let v = self.apply_adjustment(id.span, raw)?;
                    parts.push(self.stringify(v, ty, id.span)?);
                }
                StringPart::Expr(e) => {
                    let ty = self.cx.analysis.results.expr_ty(e.span)
                        .unwrap_or(self.cx.analysis.tcx.error);
                    let v = self.gen_expr(e)?;
                    parts.push(self.stringify(v, ty, e.span)?);
                }
            }
        }
        if parts.is_empty() {
            return Ok(self.const_str(""));
        }
        // Each part is a managed `str` held live across the remaining parts'
        // allocations and the concat chain; root them all so a collection
        // mid-build cannot free a part that has not yet been concatenated.
        for &p in &parts {
            self.mark_root(p);
        }
        let mut acc = parts[0];
        for &p in &parts[1..] {
            acc = self
                .call_intrinsic("lang_str_concat", &[PTR, PTR], Some(PTR), &[acc, p])
                .expect("concat returns a value");
            self.mark_root(acc);
        }
        Ok(acc)
    }

    /// Convert an interpolated value to a `str`.
    pub(crate) fn stringify(&mut self, v: Option<Value>, ty: Ty, span: Span) -> CgResult<Value> {
        // A user type with a `to_str(self): str` method (e.g. `@Derive(ToStr)`):
        // call it with the value as the receiver.
        if let Some(&mdef) = self.cx.analysis.results.stringify_methods.get(&span) {
            let recv = v.ok_or_else(|| CodegenError::new(span, "interpolated value has no payload"))?;
            let targs = self.instance_args(span);
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

    /// Build a `str` value from raw UTF-8 bytes via a read-only data object.
    pub(crate) fn emit_str_bytes(&mut self, bytes: Vec<u8>) -> Value {
        let len = bytes.len();
        let name = format!("str.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare data");
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &desc).expect("define data");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        let addr = self.b.ins().global_value(PTR, gv);
        let len_val = self.b.ins().iconst(PTR, len as i64);
        self.call_intrinsic("lang_str_from_utf8", &[PTR, PTR], Some(PTR), &[addr, len_val])
            .expect("from_utf8 returns a value")
    }

    /// A `str` value for a compile-time-known message (e.g. a panic reason).
    pub(crate) fn const_str(&mut self, text: &str) -> Value {
        self.emit_str_bytes(text.as_bytes().to_vec())
    }

    pub(crate) fn gen_cast(&mut self, inner: &Expr, from: Ty, to: Ty) -> CgResult<Option<Value>> {
        // Narrowing a union/`dynamic`: the operand is a box; check its type id.
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "union operand has no value")
            })?;
            return self.gen_union_narrow(ptr, to);
        }
        // Downcast an interface object to a concrete type: verify the stored
        // type id, then return the data pointer (panic on mismatch).
        if self.is_interface_ty(from) && !self.is_interface_ty(to) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "interface operand has no value")
            })?;
            return self.gen_dyn_downcast(ptr, to);
        }
        // Upcast a concrete value to an interface object (build its vtable box).
        if !self.is_interface_ty(from) && self.is_interface_ty(to) {
            let v = self.gen_expr(inner)?;
            return Ok(Some(self.gen_widen_dyn(v, from, to, inner.span)?));
        }
        let v = self.gen_expr(inner)?.ok_or_else(|| {
            CodegenError::new(inner.span, "cast operand has no value")
        })?;
        let tcx = &self.cx.analysis.tcx;
        // Casts to `str` go through the runtime stringifiers.
        if matches!(tcx.kind(to), TyKind::Str) {
            return Ok(Some(self.cast_to_str(v, from, inner.span)?));
        }
        let from_k = tcx.kind(from).clone();
        let to_k = tcx.kind(to).clone();
        let out = match (&from_k, &to_k) {
            (TyKind::Int(a), TyKind::Int(b)) => self.convert_int(v, *a, int_clty(*b), a.is_signed()),
            // char is a 32-bit unsigned scalar; the integer must be a valid
            // Unicode scalar value or the cast panics (`docs/14` §2).
            (TyKind::Int(a), TyKind::Char) => {
                let cp = self.resize_int(v, a.is_signed(), int_clty(*a), types::I32);
                self.guard_valid_char(cp);
                cp
            }
            (TyKind::Char, TyKind::Int(b)) => self.resize_int(v, false, types::I32, int_clty(*b)),
            (TyKind::Int(a), TyKind::Float(f)) => {
                let ft = float_clty(*f);
                if a.is_signed() { self.b.ins().fcvt_from_sint(ft, v) }
                else { self.b.ins().fcvt_from_uint(ft, v) }
            }
            // float → int panics on NaN or out-of-range (`docs/14` §2/§6).
            (TyKind::Float(f), TyKind::Int(b)) => self.gen_float_to_int(v, *f, *b),
            (TyKind::Float(a), TyKind::Float(b)) => match (a, b) {
                (FloatTy::F32, FloatTy::F64) => self.b.ins().fpromote(types::F64, v),
                (FloatTy::F64, FloatTy::F32) => self.b.ins().fdemote(types::F32, v),
                _ => v,
            },
            _ if from == to => v,
            // Union narrowing of a represented value is identity for the
            // primitive subset (no tagged unions compiled yet).
            _ => v,
        };
        Ok(Some(out))
    }

    pub(crate) fn cast_to_str(&mut self, v: Value, from: Ty, span: Span) -> CgResult<Value> {
        let from_k = self.cx.analysis.tcx.kind(from).clone();
        let result = match from_k {
            TyKind::Int(it) => {
                let widened = self.resize_int(v, it.is_signed(), int_clty(it), types::I64);
                let func = if it.is_signed() { "lang_int_to_str" } else { "lang_uint_to_str" };
                self.call_intrinsic(func, &[types::I64], Some(PTR), &[widened])
            }
            TyKind::Float(f) => {
                let promoted = if matches!(f, FloatTy::F32) {
                    self.b.ins().fpromote(types::F64, v)
                } else {
                    v
                };
                self.call_intrinsic("lang_float_to_str", &[types::F64], Some(PTR), &[promoted])
            }
            TyKind::Bool => self.call_intrinsic("lang_bool_to_str", &[types::I8], Some(PTR), &[v]),
            TyKind::Char => self.call_intrinsic("lang_char_to_str", &[types::I32], Some(PTR), &[v]),
            // `str as str` is the identity (e.g. a derived `to_str` casting a
            // `str` field).
            TyKind::Str => return Ok(v),
            TyKind::Null => return Ok(self.const_str("null")),
            _ => return Err(CodegenError::new(span, "cannot stringify this type")),
        };
        Ok(result.expect("stringifier returns a value"))
    }

    /// Resize an integer value between two Cranelift int types per signedness.
    pub(crate) fn resize_int(&mut self, v: Value, signed: bool, fromc: ClType, toc: ClType) -> Value {
        use std::cmp::Ordering::*;
        match toc.bits().cmp(&fromc.bits()) {
            Greater => {
                if signed { self.b.ins().sextend(toc, v) } else { self.b.ins().uextend(toc, v) }
            }
            Less => self.b.ins().ireduce(toc, v),
            Equal => v,
        }
    }

    pub(crate) fn convert_int(&mut self, v: Value, from: IntTy, toc: ClType, signed: bool) -> Value {
        self.resize_int(v, signed, int_clty(from), toc)
    }

    // -- unions --------------------------------------------------------------

    /// Compute `id ∈ {type_id(v) : v ∈ variants(to)}` as an i8 boolean, where
    /// `id` is a union box's stored type id.
    pub(crate) fn tag_in_target(&mut self, id: Value, to: Ty) -> Value {
        let mut acc: Option<Value> = None;
        for vt in self.cx.analysis.tcx.variants(to) {
            let tid = self.type_id_of(vt);
            let c = {
                let k = self.b.ins().iconst(types::I64, tid);
                self.b.ins().icmp(IntCC::Equal, id, k)
            };
            acc = Some(match acc {
                None => c,
                Some(a) => self.b.ins().bor(a, c),
            });
        }
        acc.unwrap_or_else(|| self.b.ins().iconst(types::I8, 0))
    }

    /// `v as T` where `v` is a union/`dynamic` box `ptr`. Panics if the stored
    /// type id is not in `to`'s variant set; otherwise unboxes (single variant)
    /// or returns the box (narrowing to a sub-union).
    pub(crate) fn gen_union_narrow(&mut self, ptr: Value, to: Ty) -> CgResult<Option<Value>> {
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
        let ok = self.tag_in_target(id, to);

        let cont = self.b.create_block();
        let panic_bb = self.b.create_block();
        self.b.ins().brif(ok, cont, &[], panic_bb, &[]);
        self.term = true;

        self.switch(panic_bb);
        let msg = self.const_str("cast failed: value is not the requested type");
        self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;

        self.switch(cont);
        // Narrowing to a sub-union keeps the box; to a single variant unboxes.
        if matches!(self.cx.analysis.tcx.kind(to), TyKind::Union(_) | TyKind::Dynamic) {
            return Ok(Some(ptr));
        }
        match clty_of(self.cx.analysis, to) {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
            None => Ok(None), // narrowed to `null`
        }
    }

    /// Whether `ty` (resolved) is an interface object type.
    pub(crate) fn is_interface_ty(&self, ty: Ty) -> bool {
        matches!(
            self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)),
            TyKind::Named { def, .. } if self.cx.analysis.program.def(*def).kind == DefKind::Interface
        )
    }

    /// Downcast an interface object `ptr` to concrete type `to`: check the
    /// stored type id, panic on mismatch, and return the data pointer.
    pub(crate) fn gen_dyn_downcast(&mut self, ptr: Value, to: Ty) -> CgResult<Option<Value>> {
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 16);
        let want = self.type_id_of(to);
        let want_v = self.b.ins().iconst(types::I64, want);
        let ok = self.b.ins().icmp(IntCC::Equal, id, want_v);

        let cont = self.b.create_block();
        let panic_bb = self.b.create_block();
        self.b.ins().brif(ok, cont, &[], panic_bb, &[]);
        self.term = true;

        self.switch(panic_bb);
        let msg = self.const_str("cast failed: interface object is not the requested type");
        self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;

        self.switch(cont);
        // The data pointer (offset 8) is the concrete value.
        Ok(Some(self.b.ins().load(PTR, MemFlags::trusted(), ptr, 8)))
    }

    /// `v is T` — a runtime tag check on a union/`dynamic`, an interface object's
    /// stored type id, or a static answer for a concrete operand.
    pub(crate) fn gen_is(&mut self, inner: &Expr, from: Ty, to: Ty) -> CgResult<Option<Value>> {
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "`is` operand has no value")
            })?;
            let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
            return Ok(Some(self.tag_in_target(id, to)));
        }
        // Interface object: compare the concrete type id stored at offset 16.
        if self.is_interface_ty(from) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "`is` operand has no value")
            })?;
            let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 16);
            let want = self.type_id_of(to);
            let want_v = self.b.ins().iconst(types::I64, want);
            return Ok(Some(self.b.ins().icmp(IntCC::Equal, id, want_v)));
        }
        // Concrete operand: the answer is known at compile time.
        self.gen_expr(inner)?; // evaluate for any side effects
        let answer = self.cx.analysis.tcx.variants(to).contains(&from);
        Ok(Some(self.b.ins().iconst(types::I8, i64::from(answer))))
    }

    // -- name resolution helpers --------------------------------------------

    pub(crate) fn resolve_local(&self, span: Span) -> CgResult<LocalId> {
        match self.cx.analysis.results.resolution(span) {
            Some(ValueRes::Local(id)) => Ok(id),
            _ => Err(CodegenError::new(span, "expected a local binding")),
        }
    }

}
