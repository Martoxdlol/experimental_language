//! Per-function codegen: builtin `List<T>`, `Map<K,V>`, and `str` methods (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- builtin List<T> -----------------------------------------------------

    /// If `ty` (resolved) is `List<E>`, return `E` (resolved).
    pub(crate) fn list_elem_of(&self, ty: Ty) -> Option<Ty> {
        let ty = resolve_shallow(self.cx.analysis, ty, &self.subst);
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Named { def, args }
                if *def == self.cx.analysis.program.list_def && args.len() == 1 =>
            {
                Some(resolve_shallow(self.cx.analysis, args[0], &self.subst))
            }
            _ => None,
        }
    }

    /// Create an empty list, telling the runtime whether elements are managed
    /// pointers (so the collector traces them).
    pub(crate) fn gen_list_new(&mut self, elem: Ty) -> Value {
        let resolved = resolve_shallow(self.cx.analysis, elem, &self.subst);
        let is_ptr = i64::from(is_managed_ptr(self.cx.analysis, resolved));
        let flag = self.b.ins().iconst(types::I64, is_ptr);
        self.call_intrinsic("lang_list_new", &[types::I64], Some(PTR), &[flag])
            .expect("list_new returns a pointer")
    }

    /// Widen an element to the list's 8-byte slot (`i64`).
    pub(crate) fn elem_to_i64(&mut self, v: Option<Value>, elem: Ty, span: Span) -> CgResult<Value> {
        let v = v.ok_or_else(|| CodegenError::new(span, "list element has no value"))?;
        match self.cx_clty(elem) {
            Some(c) if c == types::I64 => Ok(v),
            Some(c) if c.is_int() => Ok(self.b.ins().uextend(types::I64, v)),
            _ => Err(CodegenError::new(span, "this element type is not yet storable in a List")),
        }
    }

    /// Narrow an 8-byte slot back to the element type.
    pub(crate) fn i64_to_elem(&mut self, v: Value, elem: Ty, span: Span) -> CgResult<Option<Value>> {
        match self.cx_clty(elem) {
            Some(c) if c == types::I64 => Ok(Some(v)),
            Some(c) if c.is_int() => Ok(Some(self.b.ins().ireduce(c, v))),
            None => Ok(None),
            _ => Err(CodegenError::new(span, "this element type is not yet readable from a List")),
        }
    }

    /// The element Cranelift type and size of a fixed-array type `[T; N]`.
    pub(crate) fn array_elem_clty(&self, arr_ty: Ty) -> Option<ClType> {
        if let TyKind::Array { elem, .. } = self.cx.analysis.tcx.kind(arr_ty).clone() {
            let elem = resolve_shallow(self.cx.analysis, elem, &self.subst);
            return clty_of(self.cx.analysis, elem);
        }
        None
    }

    /// Builtin `List<E>` method dispatch over an already-evaluated receiver and
    /// argument values. `arg_tys` carries each argument's type so a closure
    /// argument (whose value is its lifted env) can recover its return type.
    /// Shared by the AST and HIR walks.
    pub(crate) fn emit_list_method(
        &mut self,
        list: Value,
        elem: Ty,
        name: &str,
        args: &[Option<Value>],
        arg_tys: &[Ty],
        recv_span: Span,
    ) -> CgResult<Option<Value>> {
        let arg = |i: usize| args.get(i).copied().flatten();
        match name {
            "push" => {
                let raw = self.elem_to_i64(arg(0), elem, recv_span)?;
                self.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[list, raw]);
                Ok(None)
            }
            "size" => Ok(self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])),
            "is_empty" => {
                let n = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "set" => {
                let idx = arg(0).ok_or_else(|| CodegenError::new(recv_span, "index has no value"))?;
                let raw = self.elem_to_i64(arg(1), elem, recv_span)?;
                self.call_intrinsic("lang_list_set", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
                Ok(None)
            }
            "clear" => {
                self.call_intrinsic("lang_list_clear", &[PTR], None, &[list]);
                Ok(None)
            }
            // `pop(): E | null` — remove + return the last element (boxed union).
            "pop" => {
                let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                let nonempty = self.b.ins().icmp(IntCC::SignedGreaterThan, size, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(nonempty, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_list_pop", &[PTR], Some(types::I64), &[list])
                    .expect("pop");
                let ev = self.i64_to_elem(raw, elem, recv_span)?;
                let boxed = self.box_value(ev, elem);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "insert" => {
                let idx = arg(0).ok_or_else(|| CodegenError::new(recv_span, "index has no value"))?;
                let raw = self.elem_to_i64(arg(1), elem, recv_span)?;
                self.call_intrinsic("lang_list_insert", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
                Ok(None)
            }
            // `remove(i): E | null` — bounds-checked; result is a boxed union.
            "remove" => {
                let idx = arg(0).ok_or_else(|| CodegenError::new(recv_span, "index has no value"))?;
                let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                let ge0 = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, zero);
                let lt = self.b.ins().icmp(IntCC::SignedLessThan, idx, size);
                let in_range = self.b.ins().band(ge0, lt);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(in_range, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_list_remove", &[PTR, types::I64], Some(types::I64), &[list, idx])
                    .expect("remove");
                let ev = self.i64_to_elem(raw, elem, recv_span)?;
                let boxed = self.box_value(ev, elem);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            // `get(i): E | null` — bounds-checked; result is a boxed union.
            "get" => {
                let idx = arg(0).ok_or_else(|| CodegenError::new(recv_span, "index has no value"))?;
                let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                let ge0 = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, zero);
                let lt = self.b.ins().icmp(IntCC::SignedLessThan, idx, size);
                let in_range = self.b.ins().band(ge0, lt);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(in_range, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, idx])
                    .expect("get");
                let ev = self.i64_to_elem(raw, elem, recv_span)?;
                let boxed = self.box_value(ev, elem);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "truncate" => {
                let n = arg(0).ok_or_else(|| CodegenError::new(recv_span, "count has no value"))?;
                self.call_intrinsic("lang_list_truncate", &[PTR, types::I64], None, &[list, n]);
                Ok(None)
            }
            // `iter(): Iterator<E>` — wrap the live list in a prelude `ListIter`
            // struct with `index = 0` (reads through to the list per `next()`).
            "iter" => {
                self.mark_root(list);
                let def = self.cx.analysis.program.list_iter_def;
                Ok(Some(self.build_iter_struct(def, &[elem], &[("list", list)])))
            }
            // `contains(v): bool` — true iff some element equals `v`.
            "contains" => {
                let target = arg(0).ok_or_else(|| CodegenError::new(recv_span, "argument has no value"))?;
                let idx = self.emit_list_find(list, elem, target, recv_span)?;
                let neg1 = self.b.ins().iconst(types::I64, -1);
                Ok(Some(self.b.ins().icmp(IntCC::NotEqual, idx, neg1)))
            }
            // `index_of(v): i64 | null` — index of the first equal element, or
            // the `null` variant if absent.
            "index_of" => {
                let target = arg(0).ok_or_else(|| CodegenError::new(recv_span, "argument has no value"))?;
                let idx = self.emit_list_find(list, elem, target, recv_span)?;
                let zero = self.b.ins().iconst(types::I64, 0);
                let found = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(found, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let i64t = self.cx.analysis.tcx.int(IntTy::I64);
                let boxed = self.box_value(Some(idx), i64t);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "map" => {
                let f = arg(0).ok_or_else(|| CodegenError::new(recv_span, "closure has no value"))?;
                let u = self.func_ret(arg_tys.first().copied().unwrap_or(elem));
                self.emit_list_map(list, elem, f, u, recv_span)
            }
            "filter" => {
                let f = arg(0).ok_or_else(|| CodegenError::new(recv_span, "closure has no value"))?;
                self.emit_list_filter(list, elem, f, recv_span)
            }
            "each" => {
                let f = arg(0).ok_or_else(|| CodegenError::new(recv_span, "closure has no value"))?;
                self.emit_list_each(list, elem, f, recv_span)
            }
            "fold" => {
                let init = arg(0);
                let f = arg(1).ok_or_else(|| CodegenError::new(recv_span, "closure has no value"))?;
                let acc = self.func_ret(arg_tys.get(1).copied().unwrap_or(elem));
                self.emit_list_fold(list, elem, init, f, acc, recv_span)
            }
            other => Err(CodegenError::new(
                recv_span,
                format!("`List` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// `xs.map(f)` — a new list of `f` applied to each element.
    /// `xs.map(f)` — a new list of `f` applied to each element. `f` is the
    /// already-evaluated closure value; `u` is its return type; `span` is for
    /// diagnostics. Shared by the AST and HIR walks.
    pub(crate) fn emit_list_map(&mut self, list: Value, elem: Ty, f: Value, u: Ty, span: Span)
        -> CgResult<Option<Value>>
    {
        self.mark_root(list);
        self.mark_root(f);
        let result = self.gen_list_new(u);
        self.mark_root(result);
        let u_clty = self.cx_clty(u);
        self.list_for_each(list, elem, span, |this, ev| {
            let out = this.emit_closure_call(f, &[ev], u_clty);
            let raw = this.elem_to_i64(out, u, span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.filter(pred)` — a new list of the elements for which `pred` is true.
    pub(crate) fn emit_list_filter(&mut self, list: Value, elem: Ty, f: Value, span: Span)
        -> CgResult<Option<Value>>
    {
        self.mark_root(list);
        self.mark_root(f);
        let result = self.gen_list_new(elem);
        self.mark_root(result);
        self.list_for_each(list, elem, span, |this, ev| {
            let keep = this.emit_closure_call(f, &[ev], Some(types::I8))
                .expect("predicate returns bool");
            let then_bb = this.b.create_block();
            let cont = this.b.create_block();
            this.b.ins().brif(keep, then_bb, &[], cont, &[]);
            this.term = true;
            this.switch(then_bb);
            let raw = this.elem_to_i64(Some(ev), elem, span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            this.b.ins().jump(cont, &[]);
            this.term = true;
            this.switch(cont);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.each(f)` — call `f` on each element for its side effects.
    pub(crate) fn emit_list_each(&mut self, list: Value, elem: Ty, f: Value, span: Span)
        -> CgResult<Option<Value>>
    {
        self.mark_root(list);
        self.mark_root(f);
        self.list_for_each(list, elem, span, |this, ev| {
            this.emit_closure_call(f, &[ev], None);
            Ok(())
        })?;
        Ok(None)
    }

    /// `xs.fold(init, f)` — left fold, threading the accumulator. `init_v` is the
    /// evaluated initial value, `f` the evaluated closure, `acc_ty` its result
    /// type. Shared by the AST and HIR walks.
    pub(crate) fn emit_list_fold(
        &mut self,
        list: Value,
        elem: Ty,
        init_v: Option<Value>,
        f: Value,
        acc_ty: Ty,
        span: Span,
    ) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let acc_clty = self.cx_clty(acc_ty);
        self.mark_root(f);
        let acc_var = self.b.declare_var(acc_clty.unwrap_or(types::I64));
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, acc_ty, &self.subst)) {
            self.b.declare_var_needs_stack_map(acc_var);
        }
        if let Some(v) = init_v {
            self.b.def_var(acc_var, v);
        }
        self.list_for_each(list, elem, span, |this, ev| {
            let acc = this.b.use_var(acc_var);
            let out = this.emit_closure_call(f, &[acc, ev], acc_clty)
                .ok_or_else(|| CodegenError::new(span, "fold closure has no result"))?;
            this.b.def_var(acc_var, out);
            Ok(())
        })?;
        Ok(init_v.map(|_| self.b.use_var(acc_var)))
    }

    /// The result type `R` of a closure of type `(…) => R`.
    pub(crate) fn func_ret(&self, fty: Ty) -> Ty {
        let fty = resolve_shallow(self.cx.analysis, fty, &self.subst);
        match self.cx.analysis.tcx.kind(fty) {
            TyKind::Func { ret, .. } => *ret,
            _ => self.cx.analysis.tcx.error,
        }
    }

    /// Run `body` for each element of `list` (narrowed to `elem`), as an index
    /// loop. Used by the higher-order `List` methods. `span` is for diagnostics.
    pub(crate) fn list_for_each<F>(&mut self, list: Value, elem: Ty, span: Span, mut body: F) -> CgResult<()>
    where
        F: FnMut(&mut Self, Value) -> CgResult<()>,
    {
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
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
        let ev = self.i64_to_elem(raw, elem, span)?
            .ok_or_else(|| CodegenError::new(span, "list element is zero-sized"))?;
        body(self, ev)?;
        let i3 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(exit);
        Ok(())
    }

    // -- builtin Map<K, V> ---------------------------------------------------

    /// If `ty` (resolved) is `Map<K, V>`, return `(K, V)` (both resolved).
    pub(crate) fn map_kv_of(&self, ty: Ty) -> Option<(Ty, Ty)> {
        let ty = resolve_shallow(self.cx.analysis, ty, &self.subst);
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Named { def, args }
                if *def == self.cx.analysis.program.map_def && args.len() == 2 =>
            {
                Some((
                    resolve_shallow(self.cx.analysis, args[0], &self.subst),
                    resolve_shallow(self.cx.analysis, args[1], &self.subst),
                ))
            }
            _ => None,
        }
    }

    /// Create an empty map, telling the runtime whether keys/values are managed
    /// pointers (so the collector traces them) and providing optional `hash`/
    /// `eq` function pointers for user-typed keys (`docs/15` §7). The builtin
    /// integer/`str` strategies are reused when the function pointers are
    /// null — that path matches what existed before this slice.
    pub(crate) fn gen_map_new(&mut self, kt: Ty, vt: Ty) -> Value {
        let kt_r = resolve_shallow(self.cx.analysis, kt, &self.subst);
        let vt_r = resolve_shallow(self.cx.analysis, vt, &self.subst);
        let kp = i64::from(is_managed_ptr(self.cx.analysis, kt_r));
        let vp = i64::from(is_managed_ptr(self.cx.analysis, vt_r));
        let kpv = self.b.ins().iconst(types::I64, kp);
        let vpv = self.b.ins().iconst(types::I64, vp);
        let (hash_fn, eq_fn) = self.map_key_ops(kt_r);
        self.call_intrinsic(
            "lang_map_new",
            &[types::I64, types::I64, types::I64, types::I64],
            Some(PTR),
            &[kpv, vpv, hash_fn, eq_fn],
        )
        .expect("map_new returns a pointer")
    }

    /// Compute the `(hash_fn, eq_fn)` function-pointer pair `lang_map_new`
    /// stores in the map handle. Builtin keys (primitives, `str`) leave both
    /// null — the runtime dispatches them with its built-in strategy. User
    /// keys (any other `Named` type implementing `Eq + Hash`) get the addresses
    /// of their compiled `extend … : Hash` / `: Eq` `hash`/`eq` methods.
    fn map_key_ops(&mut self, kt: Ty) -> (Value, Value) {
        let null = self.b.ins().iconst(types::I64, 0);
        match self.cx.analysis.tcx.kind(kt) {
            // Builtin-keyed maps — runtime fallback handles these.
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str => {
                (null, null)
            }
            TyKind::Named { def, args } => {
                let prog = &self.cx.analysis.program;
                // Skip the language's builtin generic types (List/Map don't act
                // as map keys); only user nominal types reach the impl lookup.
                if *def == prog.list_def || *def == prog.map_def {
                    return (null, null);
                }
                let cdef = *def;
                let cargs = args.clone();
                let hash = self
                    .extend_method_addr(cdef, &cargs, prog.hash_def, "hash")
                    .unwrap_or(null);
                let eq = self
                    .extend_method_addr(cdef, &cargs, prog.eq_def, "eq")
                    .unwrap_or(null);
                (hash, eq)
            }
            _ => (null, null),
        }
    }

    /// Look up `extend Type<args>: Iface` for `iface`, find its method named
    /// `mname`, declare its monomorphized instance, and return that function's
    /// runtime address as a value. Returns `None` if no such impl exists (e.g.
    /// the type does not implement the interface).
    fn extend_method_addr(
        &mut self,
        cdef: DefId,
        cargs: &[Ty],
        iface: DefId,
        mname: &str,
    ) -> Option<Value> {
        let fref = self.extend_method_fref(cdef, cargs, iface, mname)?;
        Some(self.b.ins().func_addr(PTR, fref))
    }

    /// Resolve `extend Type<args>: Iface`'s method `mname` to a callable
    /// `FuncRef` in the current function (declaring its monomorphized instance
    /// on demand). Returns `None` if the type does not implement `iface`.
    /// Shared by `extend_method_addr` (which needs the address as a value) and
    /// direct callers like the `List.contains`/`index_of` element-equality path.
    fn extend_method_fref(
        &mut self,
        cdef: DefId,
        cargs: &[Ty],
        iface: DefId,
        mname: &str,
    ) -> Option<cranelift_codegen::ir::FuncRef> {
        if iface == DefId(0) {
            return None;
        }
        let prog = &self.cx.analysis.program;
        let ext = self.cx.hir.iface_impls.get(&(cdef, iface)).copied()?;
        let method = (0..prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = prog.def(d);
            def.kind == DefKind::ExtendMethod && def.parent == Some(ext) && def.name == mname
        })?;
        // A generic `extend Name<P0, …>` takes the type's args in order; a
        // concrete `extend` takes none.
        let targs = if prog.def(ext).generics.is_empty() {
            Vec::new()
        } else {
            cargs.to_vec()
        };
        let func_id = match self.funcs.get(&(method, targs.clone())).copied() {
            Some(f) => f,
            None => declare_instance(
                self.module,
                self.funcs,
                self.worklist,
                self.cx.analysis,
                method,
                targs,
            )
            .ok()??,
        };
        Some(self.module.declare_func_in_func(func_id, self.b.func))
    }

    /// Emit value equality between two already-evaluated elements of type
    /// `elem` (resolved), returning an `i8` boolean. Primitives/`char` compare
    /// with `icmp`, floats with `fcmp`, `str` via `lang_str_eq`, and user types
    /// through their `Eq` impl's `eq(self, other): bool` method (the checker has
    /// already required `T: Eq`). Used by `List.contains`/`index_of`.
    fn gen_elem_eq(&mut self, a: Value, b: Value, elem: Ty, span: Span) -> CgResult<Value> {
        let elem_r = resolve_shallow(self.cx.analysis, elem, &self.subst);
        match self.cx.analysis.tcx.kind(elem_r) {
            TyKind::Int(_) | TyKind::Bool | TyKind::Char => {
                Ok(self.b.ins().icmp(IntCC::Equal, a, b))
            }
            TyKind::Float(_) => Ok(self.b.ins().fcmp(FloatCC::Equal, a, b)),
            TyKind::Str => Ok(self.gen_str_compare(BinaryOp::Eq, a, b)),
            TyKind::Named { def, args } => {
                let cdef = *def;
                let cargs = args.clone();
                let eq_def = self.cx.analysis.program.eq_def;
                let fref = self
                    .extend_method_fref(cdef, &cargs, eq_def, "eq")
                    .ok_or_else(|| {
                        CodegenError::new(span, "element type does not implement `Eq`")
                    })?;
                let call = self.b.ins().call(fref, &[a, b]);
                Ok(self.b.inst_results(call)[0])
            }
            _ => Err(CodegenError::new(
                span,
                "list element type does not support equality",
            )),
        }
    }

    /// Linear search for `target` in `list`; returns the `i64` index of the
    /// first equal element, or `-1` if absent. Shared by `List.contains`
    /// (`!= -1`) and `List.index_of` (boxed `i64 | null`).
    fn emit_list_find(&mut self, list: Value, elem: Ty, target: Value, span: Span)
        -> CgResult<Value>
    {
        self.mark_root(list);
        let elem_r = resolve_shallow(self.cx.analysis, elem, &self.subst);
        if is_managed_ptr(self.cx.analysis, elem_r) {
            self.mark_root(target);
        }
        let result = self.b.declare_var(types::I64);
        let neg1 = self.b.ins().iconst(types::I64, -1);
        self.b.def_var(result, neg1);
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let found_bb = self.b.create_block();
        let cont_bb = self.b.create_block();
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
        let ev = self.i64_to_elem(raw, elem, span)?
            .ok_or_else(|| CodegenError::new(span, "list element is zero-sized"))?;
        let eq = self.gen_elem_eq(target, ev, elem, span)?;
        self.b.ins().brif(eq, found_bb, &[], cont_bb, &[]);
        self.term = true;

        self.switch(found_bb);
        let i3 = self.b.use_var(iv);
        self.b.def_var(result, i3);
        self.b.ins().jump(exit, &[]);
        self.term = true;

        self.switch(cont_bb);
        let i4 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i4, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(exit);
        Ok(self.b.use_var(result))
    }

    /// Lower a builtin `Map<K, V>` method call.
    /// Builtin `Map<K,V>` method dispatch over an already-evaluated receiver
    /// `map` and argument values.
    pub(crate) fn emit_map_method(
        &mut self,
        map: Value,
        kt: Ty,
        vt: Ty,
        name: &str,
        args: &[Option<Value>],
        recv_span: Span,
    ) -> CgResult<Option<Value>> {
        let arg = |i: usize| args.get(i).copied().flatten();
        match name {
            "set" => {
                let key = self.elem_to_i64(arg(0), kt, recv_span)?;
                let val = self.elem_to_i64(arg(1), vt, recv_span)?;
                self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, key, val]);
                Ok(None)
            }
            "size" => Ok(self.call_intrinsic("lang_map_size", &[PTR], Some(types::I64), &[map])),
            "is_empty" => {
                let n = self.call_intrinsic("lang_map_size", &[PTR], Some(types::I64), &[map])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "clear" => {
                self.call_intrinsic("lang_map_clear", &[PTR], None, &[map]);
                Ok(None)
            }
            "contains" => {
                let key = self.elem_to_i64(arg(0), kt, recv_span)?;
                let c = self.call_intrinsic("lang_map_contains", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("contains");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::NotEqual, c, zero)))
            }
            // `get(k): V | null` / `remove(k): V | null` — boxed-union result.
            "get" | "remove" => {
                let removing = name == "remove";
                let key = self.elem_to_i64(arg(0), kt, recv_span)?;
                let present = self.call_intrinsic("lang_map_contains", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("contains");
                let zero = self.b.ins().iconst(types::I64, 0);
                let found = self.b.ins().icmp(IntCC::NotEqual, present, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(found, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_map_get", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("get");
                let ev = self.i64_to_elem(raw, vt, recv_span)?;
                let boxed = self.box_value(ev, vt);
                if removing {
                    self.call_intrinsic("lang_map_remove", &[PTR, types::I64], None, &[map, key]);
                }
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "keys" => Ok(Some(self.gen_map_keys_iter(map, kt))),
            "values" => Ok(Some(self.gen_map_values_iter(map, vt))),
            "entries" => Ok(Some(self.gen_map_entries_iter(map, kt, vt))),
            other => Err(CodegenError::new(
                recv_span,
                format!("`Map` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// Build the `MapKeys<K>` iterator returned by `Map.keys()` (`docs/18` §6):
    /// snapshot the keys into a fresh `List<K>` (`lang_map_entries(map, 1)`) and
    /// wrap it in a prelude `MapKeys` struct with `index = 0`. Iterating with
    /// `for k in m.keys()` then dispatches through the `Iterator<K>` protocol.
    fn gen_map_keys_iter(&mut self, map: Value, kt: Ty) -> Value {
        let one = self.b.ins().iconst(types::I64, 1);
        let snapshot = self
            .call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, one])
            .expect("map_entries returns a list");
        // The snapshot is unrooted between the `lang_map_entries` call and the
        // `alloc_struct` inside `build_iter_struct` — a stress collect there
        // would free it. Mark it as a stack-map root for the rest of this
        // frame (the same fix `box_value` uses for unrooted payloads).
        self.mark_root(snapshot);
        let def = self.cx.analysis.program.map_keys_def;
        self.build_iter_struct(def, &[kt], &[("snapshot", snapshot)])
    }

    /// Build the `MapValues<V>` iterator returned by `Map.values()`.
    fn gen_map_values_iter(&mut self, map: Value, vt: Ty) -> Value {
        let zero = self.b.ins().iconst(types::I64, 0);
        let snapshot = self
            .call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, zero])
            .expect("map_entries returns a list");
        self.mark_root(snapshot);
        let def = self.cx.analysis.program.map_values_def;
        self.build_iter_struct(def, &[vt], &[("snapshot", snapshot)])
    }

    /// Build the `MapEntries<K, V>` iterator returned by `Map.entries()`. The
    /// keys are snapshotted up front (so insertions during iteration are
    /// invisible) but values are looked up lazily per `next()` — matching
    /// `for entry in map`.
    fn gen_map_entries_iter(&mut self, map: Value, kt: Ty, vt: Ty) -> Value {
        let one = self.b.ins().iconst(types::I64, 1);
        let keys = self
            .call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, one])
            .expect("map_entries returns a list");
        self.mark_root(keys);
        let def = self.cx.analysis.program.map_entries_def;
        self.build_iter_struct(def, &[kt, vt], &[("map", map), ("keys", keys)])
    }

    /// Allocate a builtin map-iterator struct (`MapKeys`/`MapValues`/
    /// `MapEntries`), populate the named pointer fields, and zero the `index`
    /// counter. The struct descriptor (built by `alloc_struct`) records each
    /// `List<…>` / `Map<…>` field as a managed pointer offset so the GC
    /// traces them; the iterator itself stays live across `next()` calls via
    /// the caller's stack root.
    fn build_iter_struct(&mut self, def: DefId, args: &[Ty], fields: &[(&str, Value)]) -> Value {
        let layout = self.struct_layout(def, args);
        let ptr = self.alloc_struct(&layout);
        for (name, val) in fields {
            let off = layout.offsets[layout.index_of(name).expect("iterator field")] as i32;
            self.b.ins().store(MemFlags::trusted(), *val, ptr, off);
        }
        let idx_off = layout.offsets[layout.index_of("index").expect("iterator index")] as i32;
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, ptr, idx_off);
        ptr
    }

    /// Build a prelude `StrChars`/`StrBytes` iterator: call `rt` to snapshot the
    /// scalars/bytes into a fresh `List`, then wrap it in the `def` struct with
    /// `index = 0`. The snapshot is rooted across the `alloc_struct` (a stress
    /// collect there would otherwise free the unrooted list).
    fn gen_str_iter(&mut self, rt: &str, def: DefId, s: Value) -> Value {
        let snapshot = self
            .call_intrinsic(rt, &[PTR], Some(PTR), &[s])
            .expect("str iterator snapshot returns a list");
        self.mark_root(snapshot);
        self.build_iter_struct(def, &[], &[("snapshot", snapshot)])
    }

    /// Builtin `str` method dispatch over an already-evaluated receiver `s` and
    /// argument values.
    pub(crate) fn emit_str_method(
        &mut self,
        s: Value,
        name: &str,
        args: &[Option<Value>],
        recv_span: Span,
    ) -> CgResult<Option<Value>> {
        let arg_str = |_this: &mut Self, i: usize| -> CgResult<Value> {
            args.get(i)
                .copied()
                .flatten()
                .ok_or_else(|| CodegenError::new(recv_span, "argument has no value"))
        };
        match name {
            "size" => Ok(self.call_intrinsic("lang_str_size", &[PTR], Some(types::I64), &[s])),
            "byte_size" => {
                Ok(self.call_intrinsic("lang_str_byte_size", &[PTR], Some(types::I64), &[s]))
            }
            "is_empty" => {
                let n = self.call_intrinsic("lang_str_byte_size", &[PTR], Some(types::I64), &[s])
                    .expect("byte_size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "contains" | "starts_with" | "ends_with" => {
                let arg = arg_str(self, 0)?;
                let func = match name {
                    "contains" => "lang_str_contains",
                    "starts_with" => "lang_str_starts_with",
                    _ => "lang_str_ends_with",
                };
                Ok(self.call_intrinsic(func, &[PTR, PTR], Some(types::I8), &[s, arg]))
            }
            "substring" => {
                let a = arg_str(self, 0)?;
                let b = arg_str(self, 1)?;
                Ok(self.call_intrinsic(
                    "lang_str_substring",
                    &[PTR, types::I64, types::I64],
                    Some(PTR),
                    &[s, a, b],
                ))
            }
            "to_upper" | "to_lower" | "trim" => {
                let func = match name {
                    "to_upper" => "lang_str_to_upper",
                    "to_lower" => "lang_str_to_lower",
                    _ => "lang_str_trim",
                };
                Ok(self.call_intrinsic(func, &[PTR], Some(PTR), &[s]))
            }
            "repeat" => {
                let n = arg_str(self, 0)?;
                Ok(self.call_intrinsic("lang_str_repeat", &[PTR, types::I64], Some(PTR), &[s, n]))
            }
            "replace" => {
                let from = arg_str(self, 0)?;
                let to = arg_str(self, 1)?;
                Ok(self.call_intrinsic(
                    "lang_str_replace", &[PTR, PTR, PTR], Some(PTR), &[s, from, to],
                ))
            }
            // `index_of(needle): i64 | null` — runtime returns the byte index or
            // `-1`; box the index or the `null` variant accordingly.
            "index_of" => {
                let needle = arg_str(self, 0)?;
                let raw = self.call_intrinsic("lang_str_index_of", &[PTR, PTR], Some(types::I64), &[s, needle])
                    .expect("index_of");
                let zero = self.b.ins().iconst(types::I64, 0);
                let found = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, raw, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(found, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let i64t = self.cx.analysis.tcx.int(compiler::ty::IntTy::I64);
                let boxed = self.box_value(Some(raw), i64t);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            // `chars(): Iterator<char>` / `bytes(): Iterator<u8>` — snapshot the
            // scalars/bytes into a `List` and wrap it in a prelude `StrChars`/
            // `StrBytes` iterator struct (driven by the `Iterator` protocol).
            "chars" => {
                let def = self.cx.analysis.program.str_chars_def;
                Ok(Some(self.gen_str_iter("lang_str_to_chars", def, s)))
            }
            "bytes" => {
                let def = self.cx.analysis.program.str_bytes_def;
                Ok(Some(self.gen_str_iter("lang_str_to_bytes", def, s)))
            }
            // `split(sep): List<str>` — the runtime builds the list (under a GC
            // pause) and returns the managed handle.
            "split" => {
                let sep = arg_str(self, 0)?;
                Ok(self.call_intrinsic("lang_str_split", &[PTR, PTR], Some(PTR), &[s, sep]))
            }
            // `get(i): char | null` — runtime returns the codepoint or `-1` when
            // out of range; box the `char` or the `null` variant accordingly.
            "get" => {
                let idx = arg_str(self, 0)?;
                let raw = self.call_intrinsic("lang_str_char_at", &[PTR, types::I64], Some(types::I64), &[s, idx])
                    .expect("char_at");
                let zero = self.b.ins().iconst(types::I64, 0);
                let found = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, raw, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(found, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let char_ty = self.cx.analysis.tcx.char;
                let ev = self.i64_to_elem(raw, char_ty, recv_span)?;
                let boxed = self.box_value(ev, char_ty);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            other => Err(CodegenError::new(
                recv_span,
                format!("`str` method `{other}` is not yet lowerable"),
            )),
        }
    }

}
