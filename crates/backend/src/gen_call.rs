//! Per-function codegen: calls: functions, methods, closures, builtins, concurrency, FFI (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    pub(crate) fn gen_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        // `Thread.spawn { … }` and `JoinHandle.join()` (`docs/20`): recognised by
        // their checker-recorded span tables (no normal resolution).
        if self.cx.analysis.results.thread_spawns.contains_key(&span) {
            return self.gen_thread_spawn(args, span);
        }
        if self.cx.analysis.results.channel_news.contains(&span) {
            return self.gen_channel_new(span);
        }
        if self.cx.analysis.results.shared_news.contains(&span) {
            return self.gen_shared_new(args, span);
        }
        if self.cx.analysis.results.yield_nows.contains(&span) {
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            return Ok(self.call_intrinsic(
                "lang_async_yield", &[types::I64, types::I64], Some(PTR), &[rt, pt],
            ));
        }
        if self.cx.analysis.results.async_sleeps.contains(&span) {
            let ms = self.gen_expr(&args[0])?.ok_or_else(|| {
                CodegenError::new(args[0].span, "sleep argument has no value")
            })?;
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            return Ok(self.call_intrinsic(
                "lang_async_sleep", &[types::I64, types::I64, types::I64], Some(PTR), &[ms, rt, pt],
            ));
        }
        // `fut.cancel()` (`docs/21` §8): evaluate the receiver for effect; a
        // compute-only future has nothing to release.
        if let ExprKind::Field { receiver, .. } = &callee.kind {
            if self.cx.analysis.results.future_cancels.contains(&callee.span) {
                self.gen_expr(receiver)?;
                return Ok(None);
            }
        }
        if let Some(intr) = self.cx.analysis.results.num_intrinsics.get(&span).copied() {
            return self.gen_num_intrinsic(intr, args);
        }
        if self.cx.analysis.results.thread_joins.contains_key(&span) {
            if let ExprKind::Field { receiver, .. } = &callee.kind {
                return self.gen_thread_join(receiver, span);
            }
        }
        // Empty-collection constructors `Map<K,V>()` / `List<T>()` (and `.new`
        // forms): the checker recorded the type to allocate, keyed by call span.
        if let Some(ty) = self.cx.analysis.results.builtin_ctors.get(&span).copied() {
            if let Some((kt, vt)) = self.map_kv_of(ty) {
                return Ok(Some(self.gen_map_new(kt, vt)));
            }
            if let Some(elem) = self.list_elem_of(ty) {
                return Ok(Some(self.gen_list_new(elem)));
            }
            return Err(CodegenError::new(span, "unknown builtin constructor"));
        }
        // Builtin `.clone()` for primitives, `str`, and immutable-element
        // collections (`docs/15` §8). User/derived clones resolve as methods.
        if let ExprKind::Field { receiver, .. } = &callee.kind {
            if let Some(kind) = self.cx.analysis.results.clone_kinds.get(&callee.span).copied() {
                return self.gen_builtin_clone(receiver, kind);
            }
        }
        // Builtin `List<E>`/`Map<K,V>`/`str` methods. The checker records no
        // resolution for these — so a *resolved* call (e.g. a `T: Clone` bound's
        // `clone`, monomorphized to a `List` receiver) must skip this and go
        // through the method-dispatch path below instead.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if self.cx.analysis.results.resolution(callee.span).is_none() {
            let rty = self.cx.analysis.results.expr_ty(receiver.span)
                .unwrap_or(self.cx.analysis.tcx.error);
            if let Some(elem) = self.list_elem_of(rty) {
                return self.gen_list_method(receiver, elem, &name.name, args);
            }
            if let Some((kt, vt)) = self.map_kv_of(rty) {
                return self.gen_map_method(receiver, kt, vt, &name.name, args);
            }
            if matches!(self.cx.analysis.tcx.kind(rty), TyKind::Str) {
                return self.gen_str_method(receiver, &name.name, args);
            }
            // Builtin `Sender<T>`/`Receiver<T>` methods (`docs/20` §2).
            if let TyKind::Named { def, args: targs } = self.cx.analysis.tcx.kind(rty).clone() {
                if def == self.cx.analysis.program.sender_def && self.cx.analysis.program.sender_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_channel_send(receiver, elem, args);
                }
                if def == self.cx.analysis.program.receiver_def && self.cx.analysis.program.receiver_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_channel_recv(receiver, elem, &name.name, span);
                }
                if def == self.cx.analysis.program.shared_def && self.cx.analysis.program.shared_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_shared_lock(receiver, elem, &name.name, args, span);
                }
            }
            }
        }
        // Calling a closure *value* — a local/global of `Func` type, or any
        // other `Func`-typed expression that is not a named function/method.
        let is_value_callee = matches!(
            self.cx.analysis.results.resolution(callee.span),
            Some(ValueRes::Local(_)) | Some(ValueRes::Global(_)) | None
        );
        if is_value_callee {
            let callee_ty = resolve_shallow(
                self.cx.analysis,
                self.cx.analysis.results.expr_ty(callee.span).unwrap_or(self.cx.analysis.tcx.error),
                &self.subst,
            );
            if let TyKind::Func { ret, is_extern: false, .. } = self.cx.analysis.tcx.kind(callee_ty).clone() {
                return self.gen_closure_call(callee, ret, args);
            }
        }
        let def = match self.cx.analysis.results.resolution(callee.span) {
            Some(ValueRes::Function(d)) => d,
            Some(ValueRes::Builtin(b)) => return self.gen_builtin_call(b, args),
            Some(ValueRes::StructCtor(d)) => return self.gen_tuple_ctor(d, args, span),
            Some(ValueRes::Method(d)) => {
                if self.cx.analysis.results.static_calls.contains(&callee.span) {
                    return self.gen_static_call(d, callee, args, span);
                }
                return self.gen_method_call(d, callee, args, span);
            }
            _ => return Err(CodegenError::new(callee.span, "call target not lowerable")),
        };
        // An `extern function` is a direct C-ABI call by its real symbol name —
        // no monomorphization, no body (`docs/19`).
        if self.cx.analysis.program.def(def).kind == DefKind::ExternFunction {
            return self.gen_extern_call(def, args, span);
        }
        // The instance's generic arguments, resolved through this instance's
        // own substitution (for nested generic calls).
        let targs = self.instance_args(callee.span);
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(def, targs, &arg_vals, span)
    }

    /// Lower a numeric-namespace intrinsic (`docs/18` §10, `docs/14` §5):
    /// constants, float predicates, and the integer overflow-arithmetic families.
    pub(crate) fn gen_num_intrinsic(&mut self, intr: NumIntrinsic, args: &[Expr]) -> CgResult<Option<Value>> {
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
                let v = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "float predicate arg has no value")
                })?;
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
                self.gen_int_arith(ty, family, op, args)
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

    /// Lower a `{wrapping,saturating,checked,overflowing}_{add,sub,mul}` call.
    pub(crate) fn gen_int_arith(&mut self, ty: Ty, family: u8, op: u8, args: &[Expr]) -> CgResult<Option<Value>> {
        let it = self.int_ty_of(ty);
        let signed = it.is_signed();
        let a = self.gen_expr(&args[0])?.ok_or_else(|| CodegenError::new(args[0].span, "arg"))?;
        let b = self.gen_expr(&args[1])?.ok_or_else(|| CodegenError::new(args[1].span, "arg"))?;
        let (res, ovf) = match (op, signed) {
            (0, true) => self.b.ins().sadd_overflow(a, b),
            (0, false) => self.b.ins().uadd_overflow(a, b),
            (1, true) => self.b.ins().ssub_overflow(a, b),
            (1, false) => self.b.ins().usub_overflow(a, b),
            (2, true) => self.b.ins().smul_overflow(a, b),
            _ => self.b.ins().umul_overflow(a, b),
        };
        let ct = int_clty(it);
        match family {
            0 => Ok(Some(res)), // wrapping: the two's-complement result
            1 => {
                // saturating: on overflow clamp to MIN/MAX by the result's sign.
                let (lo, hi) = int_min_max(it);
                let min = self.b.ins().iconst(ct, lo);
                let max = self.b.ins().iconst(ct, hi);
                let zero = self.b.ins().iconst(ct, 0);
                let clamp = if !signed {
                    // unsigned: add/mul overflow → MAX; sub underflow → MIN(0).
                    if op == 1 { min } else { max }
                } else {
                    // signed: pick by the sign the true result would have had.
                    let to_max = match op {
                        0 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero), // a+b overflow: same sign as a
                        1 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero), // a-b overflow: sign of a
                        _ => {
                            // mul: result positive iff operands have equal sign.
                            let an = self.b.ins().icmp(IntCC::SignedLessThan, a, zero);
                            let bn = self.b.ins().icmp(IntCC::SignedLessThan, b, zero);
                            let diff = self.b.ins().bxor(an, bn);
                            let one = self.b.ins().iconst(types::I8, 1);
                            self.b.ins().bxor(diff, one) // positive (→MAX) when signs equal
                        }
                    };
                    self.b.ins().select(to_max, max, min)
                };
                Ok(Some(self.b.ins().select(ovf, clamp, res)))
            }
            2 => {
                // checked: `T | null` — null on overflow, else the value boxed.
                let union_ty = self.cx.analysis.tcx.error; // not needed; box by elem
                let _ = union_ty;
                let one = self.b.ins().iconst(types::I8, 1);
                let no_ovf = self.b.ins().bxor(ovf, one);
                // Build the union: value box when no overflow, else null box.
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
                Ok(Some(self.b.block_params(merge)[0]))
            }
            _ => {
                // overflowing: `(T, bool)` tuple.
                let elems = vec![ty, self.cx.analysis.tcx.bool];
                let layout = tuple_layout(self.cx.analysis, &elems);
                let ptr = self.alloc_struct(&layout);
                self.b.ins().store(MemFlags::trusted(), res, ptr, layout.offsets[0] as i32);
                self.b.ins().store(MemFlags::trusted(), ovf, ptr, layout.offsets[1] as i32);
                Ok(Some(ptr))
            }
        }
    }

    /// Lower a builtin `.clone()` (`docs/15` §8). Immutable values clone to
    /// themselves (sharing is sound); collections of immutable elements copy
    /// their backing storage into a fresh managed object.
    pub(crate) fn gen_builtin_clone(
        &mut self,
        receiver: &Expr,
        kind: CloneKind,
    ) -> CgResult<Option<Value>> {
        
        let v = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "clone receiver has no value")
        })?;
        Ok(match kind {
            CloneKind::Identity => Some(v),
            CloneKind::List => self.call_intrinsic("lang_list_clone", &[PTR], Some(PTR), &[v]),
            CloneKind::Map => self.call_intrinsic("lang_map_clone", &[PTR], Some(PTR), &[v]),
        })
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

    /// Emit an intrinsic comparison for an `Eq`/`Ord` method (`eq`/`lt`/`le`/
    /// `gt`/`ge`) on a primitive or `str` receiver — the same code paths
    /// `gen_binary` uses for `==`/`<`/… on builtins, so a bounded `T`'s
    /// comparison agrees with a direct operator.
    pub(crate) fn gen_primitive_compare(
        &mut self,
        op: BinaryOp,
        recv_ty: Ty,
        receiver: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let other = args.first().ok_or_else(|| {
            CodegenError::new(span, "comparison method missing its argument")
        })?;
        let l = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "comparison receiver has no value")
        })?;
        let r = self.gen_expr(other)?.ok_or_else(|| {
            CodegenError::new(other.span, "comparison argument has no value")
        })?;
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

    /// Lower `Thread.spawn { … }` (`docs/20` §1): evaluate the closure to its
    /// heap environment, spawn an OS thread to run it, and wrap the returned
    /// worker id in a `JoinHandle<R>`.
    pub(crate) fn gen_thread_spawn(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let clo = args.first().ok_or_else(|| {
            CodegenError::new(span, "`Thread.spawn` closure argument missing")
        })?;
        let env = self.gen_expr(clo)?.ok_or_else(|| {
            CodegenError::new(clo.span, "spawn closure has no value")
        })?;
        let id = self
            .call_intrinsic("lang_thread_spawn", &[PTR], Some(types::I64), &[env])
            .expect("lang_thread_spawn returns an id");
        let r = self.cx.analysis.results.thread_spawns.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
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

    /// Lower `JoinHandle<R>.join()` (`docs/20` §1): build the async, non-
    /// blocking `Future<Joined<R> | Panicked>` whose poll function (in the
    /// runtime) reports `Pending` until the worker finishes and then resolves
    /// to `Joined<R> { value } | Panicked { message }`. Awaiting (or
    /// `block_on`-ing) the future drives it to completion without parking the
    /// calling OS thread (`docs/21`).
    pub(crate) fn gen_thread_join(&mut self, receiver: &Expr, span: Span) -> CgResult<Option<Value>> {
        let r = self.cx.analysis.results.thread_joins.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        let jh = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "join receiver has no value")
        })?;
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

    /// Lower `channel<T>()` (`docs/20` §2): allocate a runtime channel and build
    /// the `(Sender<T>, Receiver<T>)` tuple, both carrying the channel id.
    pub(crate) fn gen_channel_new(&mut self, span: Span) -> CgResult<Option<Value>> {
        let id = self.call_intrinsic("lang_channel_new", &[], Some(types::I64), &[])
            .expect("channel id");
        let result_ty = self.cx.analysis.results.expr_ty(span)
            .unwrap_or(self.cx.analysis.tcx.error);
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

    /// Lower `Sender<T>.send(value)` (`docs/20` §2): enqueue onto the channel.
    pub(crate) fn gen_channel_send(&mut self, receiver: &Expr, elem: Ty, args: &[Expr]) -> CgResult<Option<Value>> {
        let chan = self.gen_channel_id(receiver)?;
        let v = self.gen_expr(&args[0])?;
        let raw = self.elem_to_i64(v, elem, args[0].span)?;
        self.call_intrinsic("lang_chan_send", &[types::I64, types::I64], None, &[chan, raw]);
        Ok(None)
    }

    /// Lower `Receiver<T>.recv()` / `.try_recv()` (`docs/20` §2).
    pub(crate) fn gen_channel_recv(&mut self, receiver: &Expr, elem: Ty, method: &str, span: Span)
        -> CgResult<Option<Value>>
    {
        let chan = self.gen_channel_id(receiver)?;
        if method == "recv" {
            // Async recv: build a `Future<T>` interface-object box. The runtime
            // future's `poll` pops a message (→ `Ready<T>`) or registers the
            // executor waker and reports `Pending` (`docs/20` §2 / `docs/21`).
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            let resolved = resolve_shallow(self.cx.analysis, elem, &self.subst);
            let is_ptr = is_managed_ptr(self.cx.analysis, resolved) as i64;
            let ip = self.b.ins().iconst(types::I64, is_ptr);
            let fut = self.call_intrinsic(
                "lang_chan_recv_future",
                &[types::I64, types::I64, types::I64, types::I64],
                Some(PTR),
                &[chan, rt, pt, ip],
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

    /// Read the channel id field from a `Sender`/`Receiver` receiver value.
    pub(crate) fn gen_channel_id(&mut self, receiver: &Expr) -> CgResult<Value> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "channel end is not a struct"))?;
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "channel receiver has no value")
        })?;
        let off = layout.offsets[layout.index_of("chan").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// Lower `Shared.new(value)` (`docs/20` §4): create a runtime mutex cell and
    /// wrap its id in a `Shared<T>` handle.
    pub(crate) fn gen_shared_new(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let elem = self.cx.analysis.results.expr_ty(args[0].span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let v = self.gen_expr(&args[0])?;
        let raw = self.elem_to_i64(v, elem, args[0].span)?;
        let id = self.call_intrinsic("lang_shared_new", &[types::I64], Some(types::I64), &[raw])
            .expect("shared id");
        let result_ty = self.cx.analysis.results.expr_ty(span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let shared = self.build_channel_end(result_ty, id, span)?; // {id} struct, same shape
        Ok(Some(shared))
    }

    /// Lower `Shared<T>.lock(body)` / `.try_lock(body)` (`docs/20` §4): acquire
    /// the lock, run the closure with the protected value, release.
    pub(crate) fn gen_shared_lock(&mut self, receiver: &Expr, elem: Ty, method: &str, args: &[Expr], span: Span)
        -> CgResult<Option<Value>>
    {
        let try_lock = method == "try_lock";
        let id = self.gen_shared_id(receiver)?;
        // The closure that runs under the lock, and its result clty.
        let r_ty = self.cx.analysis.results.closures.get(&args[0].span).map(|c| c.ret)
            .unwrap_or(self.cx.analysis.tcx.error);
        let r_clty = self.cx_clty(r_ty);

        if !try_lock {
            let raw = self.call_intrinsic("lang_shared_lock", &[types::I64], Some(types::I64), &[id])
                .expect("lock value");
            let inner = self.i64_to_elem(raw, elem, span)?;
            let env = self.gen_expr(&args[0])?.ok_or_else(|| {
                CodegenError::new(args[0].span, "lock body has no value")
            })?;
            let call_args: Vec<Value> = inner.into_iter().collect();
            let r = self.emit_closure_call(env, &call_args, r_clty);
            self.call_intrinsic("lang_shared_unlock", &[types::I64], None, &[id]);
            return Ok(r);
        }

        // try_lock → `R | LockBusy`.
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let got_ptr = self.b.ins().stack_addr(PTR, slot, 0);
        let raw = self.call_intrinsic(
            "lang_shared_try_lock", &[types::I64, PTR], Some(types::I64), &[id, got_ptr],
        ).expect("try_lock value");
        let got = self.b.ins().load(types::I64, MemFlags::trusted(), got_ptr, 0);
        let zero = self.b.ins().iconst(types::I64, 0);
        let acquired = self.b.ins().icmp(IntCC::NotEqual, got, zero);

        let union_ty = self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error);
        let busy_def = self.cx.analysis.program.lock_busy_def;
        let busy_ty = self.cx.analysis.tcx.variants(union_ty).into_iter()
            .find(|t| matches!(self.cx.analysis.tcx.kind(*t), TyKind::Named { def, .. } if *def == busy_def))
            .unwrap_or(self.cx.analysis.tcx.error);

        let ok_bb = self.b.create_block();
        let busy_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(acquired, ok_bb, &[], busy_bb, &[]);
        self.term = true;

        self.switch(ok_bb);
        let inner = self.i64_to_elem(raw, elem, span)?;
        let env = self.gen_expr(&args[0])?.ok_or_else(|| {
            CodegenError::new(args[0].span, "try_lock body has no value")
        })?;
        let call_args: Vec<Value> = inner.into_iter().collect();
        let r = self.emit_closure_call(env, &call_args, r_clty);
        self.call_intrinsic("lang_shared_unlock", &[types::I64], None, &[id]);
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, r_ty, &self.subst)) {
            if let Some(v) = r { self.mark_root(v); }
        }
        let boxed_ok = self.box_value(r, r_ty);
        self.b.ins().jump(merge, &[boxed_ok.into()]);
        self.term = true;

        self.switch(busy_bb);
        let busy_box = self.box_value(None, busy_ty);
        self.b.ins().jump(merge, &[busy_box.into()]);
        self.term = true;

        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Read the channel/mutex id field from a `Shared` receiver value.
    pub(crate) fn gen_shared_id(&mut self, receiver: &Expr) -> CgResult<Value> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "`Shared` is not a struct"))?;
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "`Shared` receiver has no value")
        })?;
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// Lower a call to an `extern function`: declare it as a C-ABI import by its
    /// real symbol name (the `object` crate applies platform mangling for native
    /// output; the JIT resolves it via `dlsym`) and call it directly (`docs/19`).
    pub(crate) fn gen_extern_call(&mut self, def: DefId, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let (ptys, rty) = self
            .cx
            .analysis
            .results
            .extern_sigs
            .get(&def)
            .cloned()
            .ok_or_else(|| CodegenError::new(span, "extern signature not recorded"))?;
        let mut sig = self.module.make_signature();
        for pt in &ptys {
            let ct = clty_of(self.cx.analysis, *pt)
                .ok_or_else(|| CodegenError::new(span, "extern parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        let ret_clty = clty_of(self.cx.analysis, rty);
        if let Some(rc) = ret_clty {
            sig.returns.push(AbiParam::new(rc));
        }
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self
                .gen_expr(a)?
                .ok_or_else(|| CodegenError::new(a.span, "extern argument has no value"))?;
            arg_vals.push(v);
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

    /// The generic arguments recorded for the call at `callee_span`, resolved
    /// through the current instance's substitution.
    pub(crate) fn instance_args(&self, callee_span: Span) -> Vec<Ty> {
        match self.cx.analysis.results.type_args(callee_span) {
            Some(ts) => ts
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Lower a static method call `Type.method(args)` / `T.method(args)`
    /// (`docs/09` §6, `docs/10`): no receiver is passed. For an interface static
    /// method reached through a bound, resolve it to the concrete impl using the
    /// (substituted) receiver type the checker recorded.
    pub(crate) fn gen_static_call(
        &mut self,
        def: DefId,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let (target, targs) = if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod {
            let recv = self.cx.analysis.results.static_recv.get(&callee.span).copied()
                .unwrap_or(self.cx.analysis.tcx.error);
            let recv = resolve_shallow(self.cx.analysis, recv, &self.subst);
            self.resolve_iface_method(def, recv).ok_or_else(|| {
                CodegenError::new(span, "cannot resolve static interface method to a concrete impl")
            })?
        } else {
            (def, self.instance_args(callee.span))
        };
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(target, targs, &arg_vals, span)
    }

    /// Lower `recv.method(args)`: the receiver becomes the leading `self` arg.
    pub(crate) fn gen_method_call(
        &mut self,
        def: DefId,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let ExprKind::Field { receiver, .. } = &callee.kind else {
            return Err(CodegenError::new(span, "malformed method call"));
        };
        let recv_ty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(receiver.span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        // A method on an interface object dispatches dynamically via its vtable.
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && matches!(self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Named { def: d, .. } if self.cx.analysis.program.def(*d).kind == DefKind::Interface)
        {
            return self.gen_dyn_method_call(def, receiver, args, span);
        }
        // `Clone.clone` reached through a `T: Clone` bound: if the monomorphized
        // receiver is a builtin-cloneable type (primitive/`str`/immutable
        // collection), emit the intrinsic clone rather than seeking an `extend`.
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && self.cx.analysis.program.def(def).parent == Some(self.cx.analysis.program.clone_def)
        {
            if let Some(kind) = self.builtin_clone_kind(recv_ty) {
                return self.gen_builtin_clone(receiver, kind);
            }
        }
        // `Eq.eq` / `Ord.{lt,le,gt,ge}` reached through a `T: Eq`/`T: Ord` bound:
        // if the monomorphized receiver is a primitive or `str`, emit the
        // intrinsic comparison rather than seeking an `extend` impl (primitives
        // implement these structurally — `docs/15`).
        let parent = self.cx.analysis.program.def(def).parent;
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && (parent == Some(self.cx.analysis.program.eq_def)
                || parent == Some(self.cx.analysis.program.ord_def))
            && self.cx.analysis.program.eq_def != DefId(0)
        {
            if let Some(op) = compare_op(&self.cx.analysis.program.def(def).name) {
                if self.is_primitive_comparable(recv_ty) {
                    return self.gen_primitive_compare(op, recv_ty, receiver, args, span);
                }
            }
        }
        // `ToStr.to_str` reached through a `T: ToStr` bound on a directly
        // stringifiable receiver (primitive/`str`/`null`): emit the `as str`
        // intrinsic rather than seeking an `extend` impl (`docs/15`).
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && parent == Some(self.cx.analysis.program.to_str_def)
            && self.cx.analysis.program.to_str_def != DefId(0)
        {
            if matches!(
                self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
            ) {
                let v = self.gen_expr(receiver)?.ok_or_else(|| {
                    CodegenError::new(receiver.span, "to_str receiver has no value")
                })?;
                return Ok(Some(self.cast_to_str(v, recv_ty, span)?));
            }
        }
        // An interface method on a generic type parameter is resolved to the
        // concrete `extend` impl of whatever the parameter was monomorphized to.
        let (target, targs) = if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod {
            self.resolve_iface_method(def, recv_ty).ok_or_else(|| {
                CodegenError::new(span, "cannot resolve interface method to a concrete impl")
            })?
        } else {
            // A generic `extend`'s method takes the extend's type arguments,
            // recorded by the checker at the call site.
            (def, self.instance_args(callee.span))
        };
        let self_val = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "method receiver has no value")
        })?;
        let mut arg_vals = vec![self_val];
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(target, targs, &arg_vals, span)
    }

    /// Lower a closure expression to a heap environment `[fn_ptr, captures…]`
    /// and queue its lifted function for compilation. The environment pointer
    /// is the closure value.
    pub(crate) fn gen_closure(&mut self, body: &Expr, span: Span) -> CgResult<Option<Value>> {
        let info = self.cx.analysis.results.closures.get(&span).cloned()
            .ok_or_else(|| CodegenError::new(span, "closure was not analysed"))?;

        // Declare the lifted function: (env, params…) -> ret.
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

        // Environment layout: [fn_ptr][cap0][cap1]… ; managed captures are GC
        // roots traced via the descriptor.
        let n = info.captures.len();
        let size = (8 + n * 8) as u32;
        let mut ptr_offsets = Vec::new();
        for (k, (_, ty)) in info.captures.iter().enumerate() {
            let resolved = resolve_shallow(self.cx.analysis, *ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, resolved) {
                ptr_offsets.push((8 + k * 8) as u32);
            }
        }
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let env = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        // Store the function pointer at offset 0.
        let fref = self.module.declare_func_in_func(func_id, self.b.func);
        let faddr = self.b.ins().func_addr(PTR, fref);
        self.b.ins().store(MemFlags::trusted(), faddr, env, 0);
        // Capture each enclosing local by value.
        for (k, (local, _)) in info.captures.iter().enumerate() {
            let var = *self.vars.get(local)
                .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, env, (8 + k * 8) as i32);
        }

        self.closures.push(ClosureJob {
            func_id,
            info,
            body: body.clone(),
            subst: self.subst.clone(),
            span,
        });
        Ok(Some(env))
    }

    /// Lower a bare `async { … }` block (`docs/21` §6) to a `Future` state
    /// machine: allocate a state struct holding the captured locals, wrap it in
    /// a `Future<Output>` box, and queue the block's body as the `poll` function.
    pub(crate) fn gen_async_block(&mut self, block: &Block, span: Span) -> CgResult<Option<Value>> {
        let info = self.cx.analysis.results.async_blocks.get(&span).cloned()
            .ok_or_else(|| CodegenError::new(span, "async block was not analysed"))?;
        if !info.params.is_empty() {
            return Err(CodegenError::new(span, "async closure lowering is not yet implemented"));
        }

        // Declare the poll function: (self: ptr, ctx: ptr) -> ptr.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        let name = format!("asyncblk.{}$poll", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let poll_fid = self.module.declare_function(&name, Linkage::Local, &sig)
            .expect("declare async block poll");

        // A block containing `await` needs the full state-machine layout (room
        // for every body local + the inner future); an await-free block only
        // needs to store the captures. The constructor here and the `poll`
        // function (in `define_async_job`) compute the same layout.
        let (size, ptr_offsets, cap_offs): (u32, Vec<u32>, Vec<i32>) = if block_has_await(block) {
            let cap_ids: Vec<LocalId> = info.captures.iter().map(|(l, _)| *l).collect();
            let layout = async_state_layout(self.cx.analysis, &self.subst, &cap_ids, block);
            let cap_offs = cap_ids.iter().map(|l| layout.slot_off[l]).collect();
            (layout.state_size, layout.ptr_offsets, cap_offs)
        } else {
            // [state @0][cap0 @8][cap1 @16]…
            let n = info.captures.len();
            let mut ptr_offsets = Vec::new();
            let mut cap_offs = Vec::new();
            for (k, (_, ty)) in info.captures.iter().enumerate() {
                let off = (8 + k * 8) as i32;
                cap_offs.push(off);
                let resolved = resolve_shallow(self.cx.analysis, *ty, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    ptr_offsets.push(off as u32);
                }
            }
            ((8 + n * 8) as u32, ptr_offsets, cap_offs)
        };
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let state = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, state, 0);
        for (k, (local, _)) in info.captures.iter().enumerate() {
            let var = *self.vars.get(local)
                .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, state, cap_offs[k]);
        }
        let out = info.output;
        let fut = self.emit_future_box(poll_fid, state);
        // The block body becomes the poll function body.
        let body = Expr { kind: ExprKind::Block(block.clone()), span };
        self.async_jobs.push(AsyncJob {
            poll_fid,
            info,
            body,
            subst: self.subst.clone(),
            span,
            out,
        });
        Ok(Some(fut))
    }

    /// Call a closure value: load its function pointer and call indirectly,
    /// passing the environment as the leading argument.
    pub(crate) fn gen_closure_call(
        &mut self,
        callee: &Expr,
        ret: Ty,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let env = self.gen_expr(callee)?.ok_or_else(|| {
            CodegenError::new(callee.span, "closure value has no value")
        })?;
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            // Implicit widenings recorded by the checker are applied by gen_expr.
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        let ret_clty = self.cx_clty(ret);
        Ok(self.emit_closure_call(env, &arg_vals, ret_clty))
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

    /// Dispatch `obj.method(args)` through `obj`'s vtable: load the function
    /// pointer at the method's slot and call it indirectly with the data pointer
    /// as `self`.
    pub(crate) fn gen_dyn_method_call(
        &mut self,
        iface_method: DefId,
        receiver: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let iface = self.cx.analysis.program.def(iface_method).parent
            .ok_or_else(|| CodegenError::new(span, "interface method has no interface"))?;
        let prog = &self.cx.analysis.program;
        let slot = (0..prog.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let de = prog.def(d);
                de.kind == DefKind::InterfaceMethod && de.parent == Some(iface)
            })
            .position(|d| d == iface_method)
            .ok_or_else(|| CodegenError::new(span, "method not found in interface"))?;

        let obj = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "interface receiver has no value")
        })?;
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        let ret_ty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        let ret_clty = clty_of(self.cx.analysis, ret_ty);
        self.emit_vtable_call(slot, obj, &arg_vals, ret_clty)
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
        let ext = self.cx.analysis.results.iface_impls.get(&(cdef, iface)).copied()?;
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

    /// Lower a call to a builtin (`print`/`println`): one `str` argument.
    pub(crate) fn gen_builtin_call(&mut self, b: Builtin, args: &[Expr]) -> CgResult<Option<Value>> {
        match b {
            Builtin::Print | Builtin::Println => {
                let arg = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "builtin argument has no value")
                })?;
                let name = if matches!(b, Builtin::Print) { "lang_print" } else { "lang_println" };
                self.call_intrinsic(name, &[PTR], None, &[arg]);
                Ok(None)
            }
            // Diverging builtins (`never`): call the runtime, then terminate the
            // block with a trap so any code after the call is correctly dead.
            Builtin::Panic => {
                let msg = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "panic message has no value")
                })?;
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            // The attached value is evaluated (its side effects run, it is boxed
            // into `dynamic`) but the language never inspects it; the thread
            // terminates with a generic message.
            Builtin::PanicWith => {
                let _ = self.gen_expr(&args[0])?;
                let msg = self.const_str("explicit panic (panic_with)");
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::Exit => {
                let code = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "exit code has no value")
                })?;
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
