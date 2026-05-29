//! Per-function codegen: expression dispatch, operators, boxing, `if`, and `await` (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- expressions ---------------------------------------------------------

    /// Wrap a concrete value into an interface object: allocate a managed
    /// `{vtable, data}` box and point its vtable at the (concrete-type,
    /// interface) method table.
    pub(crate) fn gen_widen_dyn(&mut self, v: Option<Value>, from: Ty, iface: Ty, span: Span)
        -> CgResult<Value>
    {
        let data = v.ok_or_else(|| CodegenError::new(span, "interface value has no data"))?;
        self.mark_root(data);
        let vtable = self.emit_vtable(from, iface, span)?;
        // box: [vtable: *const (unmanaged)][data: *managed][type_id: i64]
        // The type id supports `is`/`as` downcasts back to the concrete type.
        let desc = self.emit_descriptor(24, GC_KIND_PLAIN, &[8]);
        let ptr = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), vtable, ptr, 0);
        self.b.ins().store(MemFlags::trusted(), data, ptr, 8);
        let tid = self.type_id_of(from);
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, ptr, 16);
        Ok(ptr)
    }

    /// Build (or reference) the vtable for `(concrete type, interface)`: a data
    /// object of one function pointer per interface method, in declaration
    /// order, each pointing at the concrete type's monomorphized impl. Returns
    /// the vtable's address.
    pub(crate) fn emit_vtable(&mut self, concrete: Ty, iface: Ty, span: Span) -> CgResult<Value> {
        let analysis = self.cx.analysis;
        let hir = self.cx.hir;
        let concrete = resolve_shallow(analysis, concrete, &self.subst);
        let TyKind::Named { def: cdef, args: cargs } = analysis.tcx.kind(concrete).clone() else {
            return Err(CodegenError::new(span, "interface data is not a nominal type"));
        };
        let TyKind::Named { def: idef, .. } = analysis.tcx.kind(iface).clone() else {
            return Err(CodegenError::new(span, "interface target is not an interface"));
        };
        let ext = hir.iface_impls.get(&(cdef, idef)).copied()
            .ok_or_else(|| CodegenError::new(span, "no impl of interface for this type"))?;
        // Interface methods, in declaration order.
        let methods: Vec<DefId> = (0..analysis.program.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let def = analysis.program.def(d);
                def.kind == DefKind::InterfaceMethod && def.parent == Some(idef)
            })
            .collect();
        let ext_generic = !analysis.program.def(ext).generics.is_empty();
        // Resolve each interface method to the concrete impl's FuncId.
        let mut func_ids = Vec::with_capacity(methods.len());
        for m in &methods {
            let mname = analysis.program.def(*m).name.clone();
            let impl_def = (0..analysis.program.defs.len() as u32).map(DefId).find(|&d| {
                let def = analysis.program.def(d);
                def.kind == DefKind::ExtendMethod && def.parent == Some(ext) && def.name == mname
            }).ok_or_else(|| CodegenError::new(span, "interface method has no impl"))?;
            let targs = if ext_generic { cargs.clone() } else { Vec::new() };
            let fid = declare_instance(
                self.module, self.funcs, self.worklist, analysis, impl_def, targs,
            )?.ok_or_else(|| CodegenError::new(span, "impl method is not lowerable"))?;
            func_ids.push(fid);
        }
        // Emit the vtable data object: one pointer slot per method.
        let name = format!("vtable.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self.module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare vtable data");
        let mut desc = DataDescription::new();
        desc.set_align(8); // slots are 8-byte function pointers
        desc.define(vec![0u8; func_ids.len() * 8].into_boxed_slice());
        for (slot, fid) in func_ids.iter().enumerate() {
            let fref = self.module.declare_func_in_data(*fid, &mut desc);
            desc.write_function_addr((slot * 8) as u32, fref);
        }
        self.module.define_data(data_id, &desc).expect("define vtable");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        Ok(self.b.ins().global_value(PTR, gv))
    }

    /// Box `v` into a union/`dynamic` value, unless it is already boxed.
    pub(crate) fn apply_widen(&mut self, v: Option<Value>, from: Ty) -> Value {
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            // Already a `{type_id, data}` box — widening is a no-op.
            return v.expect("boxed union value is a pointer");
        }
        self.box_value(v, from)
    }

    /// Allocate a `{type_id: i64, data: i64}` box for a union/dynamic value.
    /// The payload (offset 8) is a managed pointer iff the boxed type is one.
    pub(crate) fn box_value(&mut self, v: Option<Value>, from: Ty) -> Value {
        let resolved = resolve_shallow(self.cx.analysis, from, &self.subst);
        let managed = is_managed_ptr(self.cx.analysis, resolved);
        // If the payload is itself a managed pointer, it must survive the box
        // allocation below (which is a GC safepoint) even though it is not yet
        // stored anywhere — root it so a collection cannot free it.
        if managed {
            if let Some(val) = v {
                self.mark_root(val);
            }
        }
        let ptr_offsets: &[u32] = if managed { &[8] } else { &[] };
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, ptr_offsets);
        let ptr = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let id = { let id = self.type_id_of(from); self.b.ins().iconst(types::I64, id) };
        self.b.ins().store(MemFlags::trusted(), id, ptr, 0);
        if let Some(v) = v {
            self.b.ins().store(MemFlags::trusted(), v, ptr, 8);
        }
        ptr
    }

    /// Box `val` into a `Ready<out> | Pending` union (the `poll` result): build a
    /// `Ready<out>` whose single `value` field holds `val` *widened to an 8-byte
    /// slot* (so the runtime executor and `await` can read it as one machine
    /// word regardless of `out`'s width), then a `{type_id, payload}` union box
    /// tagged with `Ready<out>`'s type id (`docs/21` §1).
    pub(crate) fn box_ready(&mut self, val: Option<Value>, out_ty: Ty) -> Value {
        let out_resolved = resolve_shallow(self.cx.analysis, out_ty, &self.subst);
        let out_managed = is_managed_ptr(self.cx.analysis, out_resolved);
        // Widen the result to a single i64 slot.
        let widened = match val {
            Some(v) => {
                let c = self.b.func.dfg.value_type(v);
                if c == types::I64 {
                    v
                } else if c.is_int() {
                    self.b.ins().uextend(types::I64, v)
                } else if c == types::F64 {
                    self.b.ins().bitcast(types::I64, MemFlags::new(), v)
                } else {
                    // f32: reinterpret to i32, then zero-extend into the slot.
                    let i = self.b.ins().bitcast(types::I32, MemFlags::new(), v);
                    self.b.ins().uextend(types::I64, i)
                }
            }
            None => self.b.ins().iconst(types::I64, 0),
        };
        // Root a managed payload across the `Ready` allocation (a safepoint).
        if out_managed {
            self.mark_root(widened);
        }
        let ready_def = self.cx.analysis.program.ready_def;
        let ptr_offsets: &[u32] = if out_managed { &[0] } else { &[] };
        let rdesc = self.emit_descriptor(8, GC_KIND_PLAIN, ptr_offsets);
        let ready = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[rdesc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), widened, ready, 0);
        self.mark_root(ready);
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, &[8]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let tid = 1000 + ready_def.index() as i64;
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, bx, 0);
        self.b.ins().store(MemFlags::trusted(), ready, bx, 8);
        bx
    }

    /// Box a `Pending` value into a `Ready<out> | Pending` union (the `poll`
    /// result for a not-yet-complete future). `Pending` is a unit struct, so the
    /// payload is null; only the tag matters (`docs/21` §1). Used by the `await`
    /// suspension path (in progress).
    #[allow(dead_code)]
    pub(crate) fn box_pending(&mut self) -> Value {
        let pending_def = self.cx.analysis.program.pending_def;
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, &[]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let tid = 1000 + pending_def.index() as i64;
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, bx, 0);
        let zero = self.b.ins().iconst(PTR, 0);
        self.b.ins().store(MemFlags::trusted(), zero, bx, 8);
        bx
    }

    /// Build a one-slot vtable data object for a generated `Future`: slot 0 is
    /// the `poll` function pointer (the `Future` interface has only `poll`).
    /// Returns the vtable's address.
    pub(crate) fn emit_future_vtable(&mut self, poll_fid: FuncId) -> Value {
        let name = format!("future_vtable.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self.module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare future vtable");
        let mut desc = DataDescription::new();
        desc.define(vec![0u8; 8].into_boxed_slice());
        desc.set_align(8); // holds a function pointer
        let fref = self.module.declare_func_in_data(poll_fid, &mut desc);
        desc.write_function_addr(0, fref);
        self.module.define_data(data_id, &desc).expect("define future vtable");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Allocate and initialise a `Future<Out>` interface-object box for a state
    /// machine: `[vtable @0][data = state struct @8][type_id @16]`. The data
    /// pointer is GC-traced (offset 8). `type_id` is 0 (downcasts on generated
    /// futures are a follow-up).
    pub(crate) fn emit_future_box(&mut self, poll_fid: FuncId, state: Value) -> Value {
        self.mark_root(state);
        let vtable = self.emit_future_vtable(poll_fid);
        let desc = self.emit_descriptor(24, GC_KIND_PLAIN, &[8]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), vtable, bx, 0);
        self.b.ins().store(MemFlags::trusted(), state, bx, 8);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, bx, 16);
        bx
    }

    /// A zero value of Cranelift type `ct` (for initialising state-machine
    /// local slots that have not yet been assigned).
    pub(crate) fn zero_val(&mut self, ct: ClType) -> Value {
        if ct == types::F64 {
            self.b.ins().f64const(0.0)
        } else if ct == types::F32 {
            self.b.ins().f32const(0.0)
        } else {
            self.b.ins().iconst(ct, 0)
        }
    }

    /// Suspend on `fut` at the `await` site keyed by `await_span` (shared by
    /// `await` expressions and `for await` loops): save every live local + the
    /// inner future, return `Pending` if the inner poll is not ready, otherwise
    /// continue with the unwrapped value narrowed to `out`. The `await_span`
    /// must be a registered suspend site (statement-level / `for await`).
    pub(crate) fn emit_await_suspend(&mut self, fut: Value, await_span: Span, out: Ty)
        -> CgResult<Option<Value>>
    {
        let (state_n, poll_block, inner_off, self_val, ctx_val, pending_block, saves) = {
            let actx = self.async_ctx.as_ref().ok_or_else(|| {
                CodegenError::new(await_span, "`await` outside an async body")
            })?;
            let &(state_n, poll_block, _resume) = actx.awaits.get(&await_span).ok_or_else(|| {
                CodegenError::new(
                    await_span,
                    "`await` in this position is not yet supported — use it as a \
                     statement (`var x = await e;` or `await e;`), a trailing \
                     expression, or a `return` operand",
                )
            })?;
            (state_n, poll_block, actx.inner_off, actx.self_val, actx.ctx_val,
             actx.pending_block, actx.save_locals.clone())
        };
        // Suspend: persist every live local + the inner future + resume state.
        for (local, off) in &saves {
            if let Some(&var) = self.vars.get(local) {
                let v = self.b.use_var(var);
                self.b.ins().store(MemFlags::trusted(), v, self_val, *off);
            }
        }
        self.b.ins().store(MemFlags::trusted(), fut, self_val, inner_off);
        let st = self.b.ins().iconst(types::I64, state_n);
        self.b.ins().store(MemFlags::trusted(), st, self_val, 0);
        self.b.ins().jump(poll_block, &[]);
        self.switch(poll_block);

        // Poll the inner future through its vtable (slot 0 = `poll`), forwarding
        // our `Context`.
        let innerv = self.b.ins().load(PTR, MemFlags::trusted(), self_val, inner_off);
        let r = self.emit_vtable_call(0, innerv, &[ctx_val], Some(PTR))?
            .ok_or_else(|| CodegenError::new(await_span, "poll returned no value"))?;
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), r, 0);
        let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
        let ptid = self.b.ins().iconst(types::I64, pending_tid);
        let is_pending = self.b.ins().icmp(IntCC::Equal, tag, ptid);
        let got = self.b.create_block();
        self.b.ins().brif(is_pending, pending_block, &[], got, &[]);
        self.switch(got);
        // Ready<Out>: payload @8 is the `Ready` struct; its widened `value` (@0)
        // is the result. Narrow it back to the await's output type.
        let ready = self.b.ins().load(PTR, MemFlags::trusted(), r, 8);
        let valw = self.b.ins().load(types::I64, MemFlags::trusted(), ready, 0);
        self.i64_to_elem(valw, out, await_span)
    }

    /// `spawn EXPR` over an already-evaluated inner future value: schedule the
    /// future on a worker and return a fresh `Future<T>`.
    pub(crate) fn emit_spawn(&mut self, fut: Value, out: Ty) -> CgResult<Option<Value>> {
        let prog = &self.cx.analysis.program;
        let ready_tid = 1000 + prog.ready_def.index() as i64;
        let pending_tid = 1000 + prog.pending_def.index() as i64;
        let rt = self.b.ins().iconst(types::I64, ready_tid);
        let pt = self.b.ins().iconst(types::I64, pending_tid);
        let out_res = resolve_shallow(self.cx.analysis, out, &self.subst);
        let is_ptr = is_managed_ptr(self.cx.analysis, out_res) as i64;
        let ip = self.b.ins().iconst(types::I64, is_ptr);
        Ok(self.call_intrinsic(
            "lang_async_spawn_future",
            &[PTR, types::I64, types::I64, types::I64],
            Some(PTR),
            &[fut, rt, pt, ip],
        ))
    }

    /// Emit a primitive (non-overloaded, non-short-circuit) binary operation
    /// given already-evaluated operand values and the left operand's type.
    /// Shared by the AST and HIR code paths so their arithmetic is identical.
    pub(crate) fn emit_binop(&mut self, op: BinaryOp, lty: Ty, l: Value, r: Value)
        -> CgResult<Option<Value>>
    {
        use BinaryOp::*;
        // `str + str` → runtime concatenation.
        if matches!(op, Add) && matches!(self.cx.analysis.tcx.kind(lty), TyKind::Str) {
            let s = self.call_intrinsic("lang_str_concat", &[PTR, PTR], Some(PTR), &[l, r]);
            return Ok(s);
        }
        // `str` comparisons are by content (byte-wise / lexicographic), not by
        // pointer identity (`docs/02` §7).
        if matches!(self.cx.analysis.tcx.kind(lty), TyKind::Str)
            && matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
        {
            return Ok(Some(self.gen_str_compare(op, l, r)));
        }
        let (is_float, signed) = match self.cx.analysis.tcx.kind(lty) {
            TyKind::Float(_) => (true, true),
            TyKind::Int(it) => (false, it.is_signed()),
            _ => (false, true),
        };
        // Integer division/modulo by zero always panics (`docs/14`, `docs/02`).
        if matches!(op, Div | Rem) && !is_float {
            self.guard_nonzero(r);
            // Signed `INT_MIN / -1` (and `% -1`) overflows. In debug this panics
            // (Cranelift would otherwise trap raw); in release it wraps, handled
            // inside the signed div/rem arms below (`docs/14` §2/§5).
            if signed && !is_release() {
                self.guard_div_overflow(l, r);
            }
        }
        let out = match op {
            Add if is_float => self.b.ins().fadd(l, r),
            Add => self.checked_arith(Add, signed, l, r),
            Sub if is_float => self.b.ins().fsub(l, r),
            Sub => self.checked_arith(Sub, signed, l, r),
            Mul if is_float => self.b.ins().fmul(l, r),
            Mul => self.checked_arith(Mul, signed, l, r),
            Div if is_float => self.b.ins().fdiv(l, r),
            Div if signed => self.gen_signed_div(l, r),
            Div => self.b.ins().udiv(l, r),
            Rem if signed => self.gen_signed_rem(l, r),
            Rem => self.b.ins().urem(l, r),
            BitAnd => self.b.ins().band(l, r),
            BitOr => self.b.ins().bor(l, r),
            BitXor => self.b.ins().bxor(l, r),
            Shl => { self.guard_shift(l, r); self.b.ins().ishl(l, r) }
            Shr if signed => { self.guard_shift(l, r); self.b.ins().sshr(l, r) }
            Shr => { self.guard_shift(l, r); self.b.ins().ushr(l, r) }
            Eq | Ne | Lt | Le | Gt | Ge => {
                return Ok(Some(self.gen_compare(op, is_float, signed, l, r)));
            }
            And | Or => unreachable!(),
        };
        Ok(Some(out))
    }

    /// The address of an `extern var` C global (`docs/19` §4), via an imported,
    /// writable data symbol named by its real (unmangled) symbol name. The JIT
    /// resolves it with `dlsym`; the native linker resolves it against libc.
    pub(crate) fn extern_var_addr(&mut self, def: DefId) -> Value {
        let name = self.cx.analysis.program.def(def).name.clone();
        let data_id = self
            .module
            .declare_data(&name, Linkage::Import, true, false)
            .expect("declare extern var");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Emit a guarded panic: when `cond` (an `I8` boolean) is true, call
    /// `lang_panic(msg)` and trap; otherwise fall through to the continuation.
    pub(crate) fn guard_panic(&mut self, cond: Value, msg: &str) {
        let panic_bb = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, panic_bb, &[], cont, &[]);
        self.switch(panic_bb);
        let m = self.const_str(msg);
        self.call_intrinsic("lang_panic", &[PTR], None, &[m]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;
        self.switch(cont);
    }

    /// Convert a float to an integer, panicking on NaN or out-of-range inputs
    /// (`docs/14` §2/§6). Valid inputs satisfy `lo <= v < hi`, where the bounds
    /// are the smallest/largest representable magnitudes for the target width;
    /// NaN fails both comparisons and therefore panics.
    pub(crate) fn gen_float_to_int(&mut self, v: Value, ff: FloatTy, b: IntTy) -> Value {
        let w = b.bits().unwrap_or(64) as i32;
        let signed = b.is_signed();
        let (lo, hi): (f64, f64) = if signed {
            (-(2f64.powi(w - 1)), 2f64.powi(w - 1))
        } else {
            (0.0, 2f64.powi(w))
        };
        let (lo_v, hi_v) = match ff {
            FloatTy::F32 => (self.b.ins().f32const(lo as f32), self.b.ins().f32const(hi as f32)),
            FloatTy::F64 => (self.b.ins().f64const(lo), self.b.ins().f64const(hi)),
        };
        let ge_lo = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, v, lo_v);
        let lt_hi = self.b.ins().fcmp(FloatCC::LessThan, v, hi_v);
        let in_range = self.b.ins().band(ge_lo, lt_hi);
        let one = self.b.ins().iconst(types::I8, 1);
        let oor = self.b.ins().bxor(in_range, one);
        self.guard_panic(oor, "cast from float to integer is out of range or NaN");
        let it = int_clty(b);
        if signed { self.b.ins().fcvt_to_sint(it, v) } else { self.b.ins().fcvt_to_uint(it, v) }
    }

    /// Panic if `cp` (an `I32` code point) is not a valid Unicode scalar value:
    /// it must be `<= 0x10FFFF` and outside the surrogate range
    /// `0xD800..=0xDFFF` (`docs/14` §2).
    pub(crate) fn guard_valid_char(&mut self, cp: Value) {
        let max = self.b.ins().iconst(types::I32, 0x10_FFFF);
        let too_big = self.b.ins().icmp(IntCC::UnsignedGreaterThan, cp, max);
        self.guard_panic(too_big, "cast to char is out of the Unicode range");
        let lo = self.b.ins().iconst(types::I32, 0xD800);
        let hi = self.b.ins().iconst(types::I32, 0xDFFF);
        let ge = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, cp, lo);
        let le = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, cp, hi);
        let is_surrogate = self.b.ins().band(ge, le);
        self.guard_panic(is_surrogate, "cast to char is a surrogate code point");
    }

    /// Panic if `divisor` is zero (integer `/`/`%` are always-panic per spec).
    pub(crate) fn guard_nonzero(&mut self, divisor: Value) {
        let ity = self.b.func.dfg.value_type(divisor);
        let zero = self.b.ins().iconst(ity, 0);
        let is_zero = self.b.ins().icmp(IntCC::Equal, divisor, zero);
        self.guard_panic(is_zero, "divide by zero");
    }

    /// Integer add/sub/mul. In **debug** an overflow panics; in **release** it
    /// wraps (two's complement / modular), the fast path (`docs/14` §2/§5).
    pub(crate) fn checked_arith(&mut self, op: BinaryOp, signed: bool, l: Value, r: Value) -> Value {
        use BinaryOp::*;
        if is_release() {
            return match op {
                Add => self.b.ins().iadd(l, r),
                Sub => self.b.ins().isub(l, r),
                Mul => self.b.ins().imul(l, r),
                _ => unreachable!("checked_arith only handles +/-/*"),
            };
        }
        let (res, ovf) = match (op, signed) {
            (Add, true) => self.b.ins().sadd_overflow(l, r),
            (Add, false) => self.b.ins().uadd_overflow(l, r),
            (Sub, true) => self.b.ins().ssub_overflow(l, r),
            (Sub, false) => self.b.ins().usub_overflow(l, r),
            (Mul, true) => self.b.ins().smul_overflow(l, r),
            (Mul, false) => self.b.ins().umul_overflow(l, r),
            _ => unreachable!("checked_arith only handles +/-/*"),
        };
        let what = match op {
            Add => "add",
            Sub => "subtract",
            Mul => "multiply",
            _ => unreachable!(),
        };
        self.guard_panic(ovf, &format!("attempt to {what} with overflow"));
        res
    }

    /// Panic on signed division overflow: `INT_MIN / -1` (the one case where a
    /// signed `/` or `%` overflows the result type, `docs/14` §2).
    pub(crate) fn guard_div_overflow(&mut self, l: Value, r: Value) {
        let ity = self.b.func.dfg.value_type(l);
        let bits = ity.bits();
        let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
        let min_v = self.b.ins().iconst(ity, min);
        let neg1 = self.b.ins().iconst(ity, -1);
        let l_is_min = self.b.ins().icmp(IntCC::Equal, l, min_v);
        let r_is_neg1 = self.b.ins().icmp(IntCC::Equal, r, neg1);
        let both = self.b.ins().band(l_is_min, r_is_neg1);
        self.guard_panic(both, "attempt to divide with overflow");
    }

    /// Compute `(is_overflow, safe_divisor)` for signed `INT_MIN / -1`: the
    /// overflow flag, plus the divisor replaced by `1` in that case so the
    /// hardware `sdiv`/`srem` does not trap (used only in release, where the
    /// overflowing case wraps rather than panics).
    pub(crate) fn div_overflow_select(&mut self, l: Value, r: Value) -> (Value, Value) {
        let ity = self.b.func.dfg.value_type(l);
        let bits = ity.bits();
        let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
        let min_v = self.b.ins().iconst(ity, min);
        let neg1 = self.b.ins().iconst(ity, -1);
        let l_is_min = self.b.ins().icmp(IntCC::Equal, l, min_v);
        let r_is_neg1 = self.b.ins().icmp(IntCC::Equal, r, neg1);
        let ovf = self.b.ins().band(l_is_min, r_is_neg1);
        let one = self.b.ins().iconst(ity, 1);
        let safe_r = self.b.ins().select(ovf, one, r);
        (ovf, safe_r)
    }

    /// Signed division. Debug callers have already guarded `INT_MIN / -1`; in
    /// release that case wraps to `INT_MIN` (`docs/14` §5).
    pub(crate) fn gen_signed_div(&mut self, l: Value, r: Value) -> Value {
        if !is_release() {
            return self.b.ins().sdiv(l, r);
        }
        let (ovf, safe_r) = self.div_overflow_select(l, r);
        let q = self.b.ins().sdiv(l, safe_r);
        // `INT_MIN / -1` wraps to `INT_MIN`, which is `l` in the overflow case.
        self.b.ins().select(ovf, l, q)
    }

    /// Signed remainder. In release `INT_MIN % -1` wraps to `0` (`docs/14` §5).
    pub(crate) fn gen_signed_rem(&mut self, l: Value, r: Value) -> Value {
        if !is_release() {
            return self.b.ins().srem(l, r);
        }
        let (ovf, safe_r) = self.div_overflow_select(l, r);
        let ity = self.b.func.dfg.value_type(l);
        let rem = self.b.ins().srem(l, safe_r);
        let zero = self.b.ins().iconst(ity, 0);
        self.b.ins().select(ovf, zero, rem)
    }

    /// A shift (`<<`/`>>`) panics — in debug *and* release — when the shift
    /// amount is `>=` the operand bit width (`docs/14` §2).
    pub(crate) fn guard_shift(&mut self, value: Value, amount: Value) {
        let width = self.b.func.dfg.value_type(value).bits() as i64;
        let amt_ty = self.b.func.dfg.value_type(amount);
        let width_v = self.b.ins().iconst(amt_ty, width);
        // The shift amount is unsigned (a bit position); compare unsigned.
        let too_big = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, amount, width_v);
        self.guard_panic(too_big, "shift amount >= bit width");
    }

    /// Lower a `str` comparison via the runtime (content equality / ordering).
    pub(crate) fn gen_str_compare(&mut self, op: BinaryOp, l: Value, r: Value) -> Value {
        use BinaryOp::*;
        match op {
            Eq => self.call_intrinsic("lang_str_eq", &[PTR, PTR], Some(types::I8), &[l, r])
                .expect("str_eq"),
            Ne => {
                let eq = self.call_intrinsic("lang_str_eq", &[PTR, PTR], Some(types::I8), &[l, r])
                    .expect("str_eq");
                let zero = self.b.ins().iconst(types::I8, 0);
                self.b.ins().icmp(IntCC::Equal, eq, zero)
            }
            _ => {
                let cmp = self.call_intrinsic("lang_str_cmp", &[PTR, PTR], Some(types::I64), &[l, r])
                    .expect("str_cmp");
                let zero = self.b.ins().iconst(types::I64, 0);
                let cc = match op {
                    Lt => IntCC::SignedLessThan,
                    Le => IntCC::SignedLessThanOrEqual,
                    Gt => IntCC::SignedGreaterThan,
                    Ge => IntCC::SignedGreaterThanOrEqual,
                    _ => unreachable!(),
                };
                self.b.ins().icmp(cc, cmp, zero)
            }
        }
    }

    pub(crate) fn gen_compare(&mut self, op: BinaryOp, is_float: bool, signed: bool, l: Value, r: Value)
        -> Value
    {
        use BinaryOp::*;
        if is_float {
            let cc = match op {
                Eq => FloatCC::Equal,
                Ne => FloatCC::NotEqual,
                Lt => FloatCC::LessThan,
                Le => FloatCC::LessThanOrEqual,
                Gt => FloatCC::GreaterThan,
                Ge => FloatCC::GreaterThanOrEqual,
                _ => unreachable!(),
            };
            self.b.ins().fcmp(cc, l, r)
        } else {
            let cc = match (op, signed) {
                (Eq, _) => IntCC::Equal,
                (Ne, _) => IntCC::NotEqual,
                (Lt, true) => IntCC::SignedLessThan,
                (Lt, false) => IntCC::UnsignedLessThan,
                (Le, true) => IntCC::SignedLessThanOrEqual,
                (Le, false) => IntCC::UnsignedLessThanOrEqual,
                (Gt, true) => IntCC::SignedGreaterThan,
                (Gt, false) => IntCC::UnsignedGreaterThan,
                (Ge, true) => IntCC::SignedGreaterThanOrEqual,
                (Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                _ => unreachable!(),
            };
            self.b.ins().icmp(cc, l, r)
        }
    }

    pub(crate) fn jump_to_merge(&mut self, merge: cranelift_codegen::ir::Block, val: Option<Value>,
        result_ct: Option<ClType>) -> CgResult<()>
    {
        if self.term {
            return Ok(()); // branch diverged (e.g. `return`)
        }
        match (result_ct, val) {
            (Some(_), Some(v)) => self.b.ins().jump(merge, &[v.into()]),
            (Some(ct), None) => {
                // Branch produced no value but a value is expected: only valid
                // if this path is unreachable; supply a placeholder.
                let zero = self.b.ins().iconst(if ct.is_int() { ct } else { types::I64 }, 0);
                self.b.ins().jump(merge, &[zero.into()])
            }
            (None, _) => self.b.ins().jump(merge, &[]),
        };
        self.term = true;
        Ok(())
    }

}
