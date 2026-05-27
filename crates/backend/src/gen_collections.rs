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

    pub(crate) fn gen_index_load(&mut self, receiver: &Expr, index: &Expr) -> CgResult<Option<Value>> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        // `map[key]` — panics on a missing key.
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self.gen_expr(receiver)?.ok_or_else(|| {
                CodegenError::new(receiver.span, "map has no value")
            })?;
            let kv = self.gen_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self.call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key])
                .expect("map_index returns a value");
            return self.i64_to_elem(raw, vt, receiver.span);
        }
        let elem = self.list_elem_of(rty).ok_or_else(|| {
            CodegenError::new(receiver.span, "indexing is only supported on `List` and `Map`")
        })?;
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        let idx = self.gen_expr(index)?.ok_or_else(|| {
            CodegenError::new(index.span, "index has no value")
        })?;
        let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, idx])
            .expect("list_get returns a value");
        self.i64_to_elem(raw, elem, receiver.span)
    }

    pub(crate) fn gen_index_store(&mut self, receiver: &Expr, index: &Expr, val: Option<Value>) -> CgResult<()> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        // `map[key] = v` — insert or replace.
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self.gen_expr(receiver)?.ok_or_else(|| {
                CodegenError::new(receiver.span, "map has no value")
            })?;
            let kv = self.gen_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self.elem_to_i64(val, vt, receiver.span)?;
            self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, key, raw]);
            return Ok(());
        }
        let elem = self.list_elem_of(rty).ok_or_else(|| {
            CodegenError::new(receiver.span, "indexed assignment is only supported on `List` and `Map`")
        })?;
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        let idx = self.gen_expr(index)?.ok_or_else(|| {
            CodegenError::new(index.span, "index has no value")
        })?;
        let raw = self.elem_to_i64(val, elem, receiver.span)?;
        self.call_intrinsic("lang_list_set", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
        Ok(())
    }

    /// Lower a builtin `List<E>` method call.
    pub(crate) fn gen_list_method(
        &mut self,
        receiver: &Expr,
        elem: Ty,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        match name {
            "push" => {
                let v = self.gen_expr(&args[0])?;
                let raw = self.elem_to_i64(v, elem, args[0].span)?;
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
                let idx = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "index has no value")
                })?;
                let v = self.gen_expr(&args[1])?;
                let raw = self.elem_to_i64(v, elem, args[1].span)?;
                self.call_intrinsic("lang_list_set", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
                Ok(None)
            }
            // `get(i): E | null` — bounds-checked; result is a boxed union.
            "get" => {
                let idx = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "index has no value")
                })?;
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
                let ev = self.i64_to_elem(raw, elem, receiver.span)?;
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
            "map" => self.gen_list_map(list, elem, &args[0]),
            "filter" => self.gen_list_filter(list, elem, &args[0]),
            "each" => self.gen_list_each(list, elem, &args[0]),
            "fold" => self.gen_list_fold(list, elem, &args[0], &args[1]),
            other => Err(CodegenError::new(
                receiver.span,
                format!("`List` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// The closure-argument's return type (the `R` of its `(…) => R`).
    pub(crate) fn closure_ret(&self, arg: &Expr) -> Ty {
        let fty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(arg.span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        match self.cx.analysis.tcx.kind(fty) {
            TyKind::Func { ret, .. } => *ret,
            _ => self.cx.analysis.tcx.error,
        }
    }

    /// `xs.map(f)` — a new list of `f` applied to each element.
    pub(crate) fn gen_list_map(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        let u = self.closure_ret(fexpr);
        let result = self.gen_list_new(u);
        self.mark_root(result);
        let u_clty = self.cx_clty(u);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let out = this.emit_closure_call(f, &[ev], u_clty);
            let raw = this.elem_to_i64(out, u, fexpr.span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.filter(pred)` — a new list of the elements for which `pred` is true.
    pub(crate) fn gen_list_filter(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        let result = self.gen_list_new(elem);
        self.mark_root(result);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let keep = this.emit_closure_call(f, &[ev], Some(types::I8))
                .expect("predicate returns bool");
            let then_bb = this.b.create_block();
            let cont = this.b.create_block();
            this.b.ins().brif(keep, then_bb, &[], cont, &[]);
            this.term = true;
            this.switch(then_bb);
            let raw = this.elem_to_i64(Some(ev), elem, fexpr.span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            this.b.ins().jump(cont, &[]);
            this.term = true;
            this.switch(cont);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.each(f)` — call `f` on each element for its side effects.
    pub(crate) fn gen_list_each(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            this.emit_closure_call(f, &[ev], None);
            Ok(())
        })?;
        Ok(None)
    }

    /// `xs.fold(init, f)` — left fold, threading the accumulator.
    pub(crate) fn gen_list_fold(&mut self, list: Value, elem: Ty, init: &Expr, fexpr: &Expr)
        -> CgResult<Option<Value>>
    {
        self.mark_root(list);
        let acc_ty = self.closure_ret(fexpr);
        let acc_clty = self.cx_clty(acc_ty);
        let init_v = self.gen_expr(init)?;
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        // The accumulator threads through the loop as a block parameter.
        let acc_var = self.b.declare_var(acc_clty.unwrap_or(types::I64));
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, acc_ty, &self.subst)) {
            self.b.declare_var_needs_stack_map(acc_var);
        }
        if let Some(v) = init_v {
            self.b.def_var(acc_var, v);
        }
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let acc = this.b.use_var(acc_var);
            let out = this.emit_closure_call(f, &[acc, ev], acc_clty)
                .ok_or_else(|| CodegenError::new(fexpr.span, "fold closure has no result"))?;
            this.b.def_var(acc_var, out);
            Ok(())
        })?;
        Ok(init_v.map(|_| self.b.use_var(acc_var)))
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
    /// pointers (so the collector traces them).
    pub(crate) fn gen_map_new(&mut self, kt: Ty, vt: Ty) -> Value {
        let kp = i64::from(is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, kt, &self.subst)));
        let vp = i64::from(is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, vt, &self.subst)));
        let kpv = self.b.ins().iconst(types::I64, kp);
        let vpv = self.b.ins().iconst(types::I64, vp);
        self.call_intrinsic("lang_map_new", &[types::I64, types::I64], Some(PTR), &[kpv, vpv])
            .expect("map_new returns a pointer")
    }

    /// Lower a builtin `Map<K, V>` method call.
    pub(crate) fn gen_map_method(
        &mut self,
        receiver: &Expr,
        kt: Ty,
        vt: Ty,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let map = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "map has no value")
        })?;
        match name {
            "set" => {
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
                let vv = self.gen_expr(&args[1])?;
                let val = self.elem_to_i64(vv, vt, args[1].span)?;
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
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
                let c = self.call_intrinsic("lang_map_contains", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("contains");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::NotEqual, c, zero)))
            }
            // `get(k): V | null` / `remove(k): V | null` — boxed-union result.
            "get" | "remove" => {
                let removing = name == "remove";
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
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
                let ev = self.i64_to_elem(raw, vt, receiver.span)?;
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
            "keys" | "values" => {
                let want_keys = self.b.ins().iconst(types::I64, i64::from(name == "keys"));
                Ok(self.call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, want_keys]))
            }
            other => Err(CodegenError::new(
                receiver.span,
                format!("`Map` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// Lower a map literal `{ k: v, ..base }` to an allocation plus inserts.
    pub(crate) fn gen_map_lit(&mut self, items: &[MapItem], ty: Ty, span: Span) -> CgResult<Option<Value>> {
        let (kt, vt) = self.map_kv_of(ty).ok_or_else(|| {
            CodegenError::new(span, "map literal has non-map type")
        })?;
        let map = self.gen_map_new(kt, vt);
        for item in items {
            match item {
                MapItem::Entry { key, value, .. } => {
                    let kv = self.gen_expr(key)?;
                    let k = self.elem_to_i64(kv, kt, key.span)?;
                    let vv = self.gen_expr(value)?;
                    let v = self.elem_to_i64(vv, vt, value.span)?;
                    self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, k, v]);
                }
                MapItem::Spread(base) => {
                    let src = self.gen_expr(base)?.ok_or_else(|| {
                        CodegenError::new(base.span, "map spread source has no value")
                    })?;
                    self.call_intrinsic("lang_map_extend", &[PTR, PTR], None, &[map, src]);
                }
            }
        }
        Ok(Some(map))
    }

    /// Lower a builtin `str` method call.
    pub(crate) fn gen_str_method(
        &mut self,
        receiver: &Expr,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let s = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "str receiver has no value")
        })?;
        let arg_str = |this: &mut Self, i: usize| -> CgResult<Value> {
            this.gen_expr(&args[i])?
                .ok_or_else(|| CodegenError::new(args[i].span, "argument has no value"))
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
            other => Err(CodegenError::new(
                receiver.span,
                format!("`str` method `{other}` is not yet lowerable"),
            )),
        }
    }

}
