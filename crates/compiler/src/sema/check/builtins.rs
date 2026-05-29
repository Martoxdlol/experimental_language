//! Type checker: builtin `List`/`Map`/`str`/channel/Shared/thread/async (`impl Checker`, split from `check.rs`).

use super::*;

impl<'a> Checker<'a> {
    // -- builtin List<T> -----------------------------------------------------

    /// If `ty` is `List<E>`, return `E`.
    pub(crate) fn list_elem(&self, ty: Ty) -> Option<Ty> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.list_def && args.len() == 1 => {
                Some(args[0])
            }
            _ => None,
        }
    }

    pub(crate) fn mk_list(&mut self, elem: Ty) -> Ty {
        let def = self.prog.list_def;
        self.tcx.mk_named(def, vec![elem])
    }

    pub(crate) fn check_list_lit(&mut self, elems: &[Expr], expected: Option<Ty>, span: Span) -> Ty {
        let exp_elem = expected.and_then(|e| self.list_elem(e));
        if elems.is_empty() {
            return match exp_elem {
                Some(e) => self.mk_list(e),
                None => {
                    self.emit(span, SemaErrorKind::Message(
                        "cannot infer the element type of an empty list; annotate it".into(),
                    ));
                    self.tcx.error
                }
            };
        }
        // The element type is the annotation if given, else the first element's.
        let elem = match exp_elem {
            Some(e) => e,
            None => self.check_expr(&elems[0], None),
        };
        for el in elems {
            let t = self.check_expr(el, Some(elem));
            self.expect(t, elem, el.span);
        }
        self.mk_list(elem)
    }

    pub(crate) fn map_kv(&self, ty: Ty) -> Option<(Ty, Ty)> {
        match self.tcx.kind(ty) {
            TyKind::Named { def, args } if *def == self.prog.map_def && args.len() == 2 => {
                Some((args[0], args[1]))
            }
            _ => None,
        }
    }

    pub(crate) fn mk_map(&mut self, k: Ty, v: Ty) -> Ty {
        let def = self.prog.map_def;
        self.tcx.mk_named(def, vec![k, v])
    }

    /// A map key must implement `Eq + Hash` (`docs/15` §7, `docs/18` §6). The
    /// runtime handles `str` and integer types via built-in hash/eq strategies;
    /// `bool`/`char` are also intrinsically hashable. Other types are accepted
    /// when an `extend … : Hash` and `extend … : Eq` are in scope (covering
    /// `@Derive(Eq, Hash)` and hand-written impls).
    pub(crate) fn is_valid_map_key(&mut self, ty: Ty) -> bool {
        if self.is_hashable(ty) {
            return true;
        }
        let hash_def = self.prog.hash_def;
        let eq_def = self.prog.eq_def;
        if hash_def == DefId(0) || eq_def == DefId(0) {
            return false;
        }
        self.type_implements(ty, hash_def) && self.type_implements(ty, eq_def)
    }

    pub(crate) fn check_map_lit(&mut self, items: &[MapItem], expected: Option<Ty>, span: Span) -> Ty {
        let exp_kv = expected.and_then(|e| self.map_kv(e));
        // Determine K/V from the annotation, else from the first entry.
        let mut kv = exp_kv;
        if kv.is_none() {
            for it in items {
                if let MapItem::Entry { key, value, .. } = it {
                    let k = self.check_expr(key, None);
                    let v = self.check_expr(value, None);
                    kv = Some((k, v));
                    break;
                }
            }
        }
        let Some((kt, vt)) = kv else {
            self.emit(span, SemaErrorKind::Message(
                "cannot infer the key/value types of an empty map; annotate it or use `Map<K, V>()`".into(),
            ));
            return self.tcx.error;
        };
        if !self.is_valid_map_key(kt) && !self.tcx.is_error(kt) {
            self.emit(span, SemaErrorKind::Message(format!(
                "`{}` cannot be used as a map key (expected `str` or an integer type)",
                self.display(kt)
            )));
        }
        let map_ty = self.mk_map(kt, vt);
        for it in items {
            match it {
                MapItem::Entry { key, value, .. } => {
                    let k = self.check_expr(key, Some(kt));
                    self.expect(k, kt, key.span);
                    let v = self.check_expr(value, Some(vt));
                    self.expect(v, vt, value.span);
                }
                MapItem::Spread(base) => {
                    let bt = self.check_expr(base, Some(map_ty));
                    self.expect(bt, map_ty, base.span);
                }
            }
        }
        map_ty
    }

    /// Type-check a builtin `Map<K, V>` method call (`docs/18` §6).
    pub(crate) fn check_map_method(&mut self, kt: Ty, vt: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let i64t = self.tcx.int(IntTy::I64);
        let check_args = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "size" => { check_args(self, &[]); i64t }
            "is_empty" => { check_args(self, &[]); self.tcx.bool }
            "clear" => { check_args(self, &[]); self.tcx.null }
            "contains" => { check_args(self, &[kt]); self.tcx.bool }
            "get" => { check_args(self, &[kt]); self.tcx.mk_union([vt, self.tcx.null]) }
            "remove" => { check_args(self, &[kt]); self.tcx.mk_union([vt, self.tcx.null]) }
            "set" => { check_args(self, &[kt, vt]); self.tcx.null }
            "keys" => {
                check_args(self, &[]);
                let def = self.prog.map_keys_def;
                self.tcx.mk_named(def, vec![kt])
            }
            "values" => {
                check_args(self, &[]);
                let def = self.prog.map_values_def;
                self.tcx.mk_named(def, vec![vt])
            }
            "entries" => {
                check_args(self, &[]);
                let def = self.prog.map_entries_def;
                self.tcx.mk_named(def, vec![kt, vt])
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`Map` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    pub(crate) fn check_index(&mut self, receiver: &Expr, index: &Expr) -> Ty {
        let rty = self.check_expr(receiver, None);
        if self.tcx.is_error(rty) {
            return self.tcx.error;
        }
        if let Some(elem) = self.list_elem(rty) {
            let i64t = self.tcx.int(IntTy::I64);
            let it = self.check_expr(index, Some(i64t));
            self.expect(it, i64t, index.span);
            return elem;
        }
        // `map[key]` — indexed read/write; panics on a missing key (`docs/18`).
        if let Some((kt, vt)) = self.map_kv(rty) {
            let it = self.check_expr(index, Some(kt));
            self.expect(it, kt, index.span);
            return vt;
        }
        // A fixed-size FFI array `[T; N]` (`docs/19` §4): `arr[i]` reads/writes
        // element `T` (no bounds check — raw FFI). Used on extern struct fields.
        if let TyKind::Array { elem, .. } = self.tcx.kind(rty) {
            let elem = *elem;
            let i64t = self.tcx.int(IntTy::I64);
            let it = self.check_expr(index, Some(i64t));
            self.expect(it, i64t, index.span);
            return elem;
        }
        self.emit(receiver.span, SemaErrorKind::Message(format!(
            "type `{}` cannot be indexed with `[]`", self.display(rty)
        )));
        self.tcx.error
    }

    /// Type-check a builtin `List<E>` method call.
    pub(crate) fn check_list_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let i64t = self.tcx.int(IntTy::I64);
        let check_args = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "push" => {
                check_args(self, &[elem]);
                self.tcx.null
            }
            "size" => {
                check_args(self, &[]);
                i64t
            }
            "is_empty" => {
                check_args(self, &[]);
                self.tcx.bool
            }
            "get" => {
                check_args(self, &[i64t]);
                self.tcx.mk_union([elem, self.tcx.null])
            }
            "set" => {
                check_args(self, &[i64t, elem]);
                self.tcx.null
            }
            "clear" => {
                check_args(self, &[]);
                self.tcx.null
            }
            "pop" => {
                check_args(self, &[]);
                self.tcx.mk_union([elem, self.tcx.null])
            }
            "insert" => {
                check_args(self, &[i64t, elem]);
                self.tcx.null
            }
            "remove" => {
                check_args(self, &[i64t]);
                self.tcx.mk_union([elem, self.tcx.null])
            }
            "truncate" => {
                check_args(self, &[i64t]);
                self.tcx.null
            }
            // `iter(): Iterator<E>` — a cursor view over the live list
            // (`docs/18` §5); drives `for x in xs.iter()` via the protocol.
            "iter" => {
                check_args(self, &[]);
                self.tcx.mk_named(self.prog.list_iter_def, vec![elem])
            }
            // `contains(v): bool` / `index_of(v): i64 | null` — require `T: Eq`
            // (`docs/18` §5); element equality dispatches through the element
            // type's `eq` (intrinsic for primitives/`str`, the impl otherwise).
            "contains" | "index_of" => {
                check_args(self, &[elem]);
                if !self.is_equatable(elem) {
                    self.emit(span, SemaErrorKind::Message(format!(
                        "`List.{}` requires the element type to implement `Eq`",
                        name.name
                    )));
                }
                if name.name == "contains" {
                    self.tcx.bool
                } else {
                    self.tcx.mk_union([i64t, self.tcx.null])
                }
            }
            // Higher-order methods take a closure (often written as a trailing
            // closure with an implicit `it`).
            "map" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                // Expected `(E) => U`; `U` is inferred from the closure body.
                let want = self.tcx.mk_func(vec![elem], self.tcx.error, false);
                let ct = self.check_expr(&args[0], Some(want));
                match self.tcx.kind(ct).clone() {
                    TyKind::Func { ret, .. } => self.mk_list(ret),
                    _ => self.tcx.error,
                }
            }
            "filter" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                let want = self.tcx.mk_func(vec![elem], self.tcx.bool, false);
                let ct = self.check_expr(&args[0], Some(want));
                self.expect(ct, want, args[0].span);
                self.mk_list(elem)
            }
            "each" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.null;
                }
                let want = self.tcx.mk_func(vec![elem], self.tcx.null, false);
                self.check_expr(&args[0], Some(want));
                self.tcx.null
            }
            "fold" => {
                if args.len() != 2 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 2, found: args.len() });
                    return self.tcx.error;
                }
                let acc = self.check_expr(&args[0], None);
                let want = self.tcx.mk_func(vec![acc, elem], acc, false);
                let ct = self.check_expr(&args[1], Some(want));
                self.expect(ct, want, args[1].span);
                acc
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`List` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Type-check a builtin `str` method call (`docs/18` §4).
    pub(crate) fn check_str_method(&mut self, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let str_ty = self.tcx.str;
        let i64t = self.tcx.int(IntTy::I64);
        let check = |this: &mut Self, expect: &[Ty]| {
            if args.len() != expect.len() {
                this.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
            }
            for (a, e) in args.iter().zip(expect) {
                let at = this.check_expr(a, Some(*e));
                this.expect(at, *e, a.span);
            }
        };
        match name.name.as_str() {
            "size" | "byte_size" => {
                check(self, &[]);
                i64t
            }
            "is_empty" => {
                check(self, &[]);
                self.tcx.bool
            }
            "contains" | "starts_with" | "ends_with" => {
                check(self, &[str_ty]);
                self.tcx.bool
            }
            "substring" => {
                check(self, &[i64t, i64t]);
                str_ty
            }
            "to_upper" | "to_lower" | "trim" => {
                check(self, &[]);
                str_ty
            }
            "repeat" => {
                check(self, &[i64t]);
                str_ty
            }
            "replace" => {
                check(self, &[str_ty, str_ty]);
                str_ty
            }
            "index_of" => {
                check(self, &[str_ty]);
                self.tcx.mk_union([i64t, self.tcx.null])
            }
            "split" => {
                check(self, &[str_ty]);
                self.mk_list(str_ty)
            }
            "get" => {
                check(self, &[i64t]);
                self.tcx.mk_union([self.tcx.char, self.tcx.null])
            }
            // `chars(): Iterator<char>` / `bytes(): Iterator<u8>` — snapshot
            // iterators (prelude `StrChars`/`StrBytes`), driven by the general
            // `Iterator` protocol in `for ch in s.chars()` (`docs/18` §4).
            "chars" => {
                check(self, &[]);
                self.tcx.mk_named(self.prog.str_chars_def, vec![])
            }
            "bytes" => {
                check(self, &[]);
                self.tcx.mk_named(self.prog.str_bytes_def, vec![])
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`str` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Whether `ty` is an immutable value: cloning it can share the existing
    /// value (no observable mutation). Primitives, `char`, `bool`, `str`, and
    /// `null` qualify (`docs/15` §8 — `str` is immutable, so sharing is sound).
    /// Whether `ty` supports value equality (`docs/18` §5 — `List.contains`/
    /// `index_of` require `T: Eq`). Primitives and `str` are intrinsically
    /// equatable; user types qualify through an `Eq` impl (derived or
    /// hand-written), including a type parameter bounded by `Eq`.
    pub(crate) fn is_equatable(&mut self, ty: Ty) -> bool {
        self.is_immutable_value(ty) || self.type_implements(ty, self.prog.eq_def)
    }

    pub(crate) fn is_immutable_value(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        )
    }

    /// Whether `ty` is safe to capture into a spawned thread by value: an
    /// immutable value, or a thread-safe channel endpoint (`Sender`/`Receiver`,
    /// whose struct just carries a synchronized channel's id) (`docs/20`).
    pub(crate) fn is_thread_shareable(&self, ty: Ty) -> bool {
        if self.is_immutable_value(ty) {
            return true;
        }
        matches!(self.tcx.kind(ty),
            TyKind::Named { def, .. }
                if *def == self.prog.sender_def
                    || *def == self.prog.receiver_def
                    || *def == self.prog.shared_def)
    }

    /// Resolve a builtin `.hash()` on a primitive or `str` receiver — types
    /// the runtime hashes intrinsically (`docs/15` §7). Records the `Hash`
    /// interface's `hash` method as the call's resolution so the backend's
    /// existing `InterfaceMethod`/`parent == hash_def` dispatch picks it up
    /// and emits the right `lang_hash_*` call. Returns `Some(u64)` on match,
    /// `None` for user types (which resolve through their own `Hash` impl).
    pub(crate) fn check_builtin_hash(&mut self, rty: Ty, callee_span: Span) -> Option<Ty> {
        if !self.is_hashable(rty) {
            return None;
        }
        let iface = self.prog.hash_def;
        if iface == DefId(0) {
            return None;
        }
        let method = (0..self.prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = self.prog.def(d);
            def.kind == DefKind::InterfaceMethod && def.parent == Some(iface) && def.name == "hash"
        })?;
        self.record_res(callee_span, ValueRes::Method(method), self.tcx.error);
        Some(self.tcx.int(IntTy::U64))
    }

    /// Resolve a builtin `.clone()`. Returns `Some(result type)` for the
    /// receiver kinds the compiler clones intrinsically (recording a
    /// [`CloneKind`] for codegen); `None` for user types, which clone through
    /// their own `Clone` impl. Emits an error for collections whose elements are
    /// not (yet) cloneable.
    pub(crate) fn check_builtin_clone(&mut self, rty: Ty, _callee_span: Span, name_span: Span) -> Option<Ty> {
        use crate::sema::results::CloneKind;
        if self.is_immutable_value(rty) {
            self.pending_clone_kind.set(Some(CloneKind::Identity));
            return Some(rty);
        }
        // A `Shared<T>` handle clones to another handle for the *same* cell
        // (`docs/20` §4: clone the handle, not the value). The handle is an
        // immutable id, so sharing it is the intended clone.
        if matches!(self.tcx.kind(rty),
            TyKind::Named { def, .. }
                if *def == self.prog.shared_def
                    || *def == self.prog.sender_def
                    || *def == self.prog.receiver_def)
        {
            self.pending_clone_kind.set(Some(CloneKind::Identity));
            return Some(rty);
        }
        if let Some(elem) = self.list_elem(rty) {
            if self.is_immutable_value(elem) {
                self.pending_clone_kind.set(Some(CloneKind::List));
                return Some(rty);
            }
            // A `List` of mutable elements is still cloneable when the
            // element type implements `Clone` (`docs/10`): codegen emits a
            // per-element deep clone.
            let clone_def = self.prog.clone_def;
            if clone_def != DefId(0) && self.type_implements(elem, clone_def) {
                self.pending_clone_kind.set(Some(CloneKind::ListDeep));
                return Some(rty);
            }
            self.emit(name_span, SemaErrorKind::Message(format!(
                "cannot `clone` a `List` of `{}` — its element type does not \
                 implement `Clone`",
                self.display(elem)
            )));
            return Some(self.tcx.error);
        }
        if let Some((kt, vt)) = self.map_kv(rty) {
            if self.is_immutable_value(kt) && self.is_immutable_value(vt) {
                self.pending_clone_kind.set(Some(CloneKind::Map));
                return Some(rty);
            }
            // The key type must be immutable (its hash would otherwise become
            // unstable across the clone). The value type can be deep-cloned
            // when it implements `Clone`.
            let clone_def = self.prog.clone_def;
            if self.is_immutable_value(kt)
                && clone_def != DefId(0)
                && self.type_implements(vt, clone_def)
            {
                self.pending_clone_kind.set(Some(CloneKind::MapDeep));
                return Some(rty);
            }
            self.emit(name_span, SemaErrorKind::Message(format!(
                "cannot `clone` a `Map<{}, {}>` — key must be immutable and \
                 value must implement `Clone`",
                self.display(kt), self.display(vt)
            )));
            return Some(self.tcx.error);
        }
        None
    }

    /// Type-check `Thread.spawn(() => R)` / `Thread.spawn { … }` (`docs/20` §1).
    /// The single argument is a parameterless closure; the result is
    /// `JoinHandle<R>`. Captures must be immutable values (deep-cloning mutable
    /// captures across the spawn boundary is a follow-up — `docs/20` §1).
    pub(crate) fn check_thread_spawn(&mut self, args: &[Expr], trailing: Option<&Expr>, span: Span) -> Ty {
        let clo = match (args, trailing) {
            ([], Some(tc)) => tc,
            ([a], None) => a,
            _ => {
                self.emit(span, SemaErrorKind::Message(
                    "`Thread.spawn` takes a single closure argument".into(),
                ));
                for a in args {
                    self.check_expr(a, None);
                }
                return self.tcx.error;
            }
        };
        // Expect a parameterless closure; its return type is inferred.
        let want = self.tcx.mk_func(vec![], self.tcx.error, false);
        let cty = self.check_expr(clo, Some(want));
        let r = match self.tcx.kind(cty).clone() {
            TyKind::Func { params, ret, .. } if params.is_empty() => ret,
            TyKind::Error => return self.tcx.error,
            _ => {
                self.emit(clo.span, SemaErrorKind::Message(
                    "`Thread.spawn` expects a parameterless closure `() => R`".into(),
                ));
                return self.tcx.error;
            }
        };
        // A float result is carried across the worker boundary as its raw bit
        // pattern (the code generator selects the result ABI; `docs/20`), so
        // float-returning spawns are supported.
        // Captures must be safe to share across threads: an immutable value, or
        // a thread-safe handle (`Sender`/`Receiver` — the channel itself is
        // synchronized; the struct only carries an id). Other managed values
        // would need a deep clone at the boundary (a follow-up — `docs/20` §1).
        // The closure was just checked, so its HIR node (with the resolved
        // captures) is already in `node_hir` (was the `closures` side table).
        let cap_tys: Vec<Ty> = match self.node_hir.get(&clo.span).map(|n| &n.kind) {
            Some(crate::hir::ExprKind::Closure { captures, .. }) => {
                captures.iter().map(|(_, t)| *t).collect()
            }
            _ => Vec::new(),
        };
        for cap_ty in cap_tys {
            if !self.is_thread_shareable(cap_ty) {
                self.emit(clo.span, SemaErrorKind::Message(format!(
                    "`Thread.spawn` can only capture immutable values or channel \
                     endpoints so far; captured value of type `{}` would need a \
                     deep clone across the thread boundary (`docs/20` §1)",
                    self.display(cap_ty)
                )));
            }
        }
        self.tcx.mk_named(self.prog.join_handle_def, vec![r])
    }

    /// `JoinHandle<R>.join(): Future<Joined<R> | Panicked>` and
    /// `.detach(): null` (`docs/20` §1).
    ///
    /// `join` is **async and non-blocking** (`docs/21`): you `await` the
    /// returned future (or drive it with `block_on`) instead of parking the
    /// calling OS thread. The future resolves when the worker finishes.
    pub(crate) fn check_join_handle_method(&mut self, r: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        if !args.is_empty() {
            self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
        }
        match name.name.as_str() {
            "join" => {
                let joined = self.tcx.mk_named(self.prog.joined_def, vec![r]);
                let panicked = self.tcx.mk_named(self.prog.panicked_def, Vec::new());
                let union = self.tcx.mk_union([joined, panicked]);
                self.tcx.mk_named(self.prog.future_def, vec![union])
            }
            "detach" => self.tcx.null,
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`JoinHandle` has no method `{other}`"
                )));
                self.tcx.error
            }
        }
    }

    /// Try to interpret `recv_name.method(args)` as a static method call
    /// (`docs/09` §6, `docs/10`). Returns `Some(result type)` when `recv_name`
    /// is a concrete type or an in-scope generic parameter; `None` otherwise (so
    /// the caller falls through to the instance-method path).
    pub(crate) fn try_static_call(
        &mut self,
        recv_name: &str,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        generics: &[Type],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Option<Ty> {
        // A trailing closure is the call's final argument, as for instance calls.
        let merged: Vec<Expr>;
        let arg_slice: &[Expr] = match trailing {
            Some(tc) => {
                let mut v = args.to_vec();
                v.push(tc.clone());
                merged = v;
                &merged
            }
            None => args,
        };
        // (a) `T.static_method()` — a generic parameter, resolved via its bounds.
        if let Some(pty) = self.cur_generics.get(recv_name).copied() {
            if let TyKind::Param(pdef) = self.tcx.kind(pty).clone() {
                return Some(self.check_bound_static_call(pdef, pty, callee, method, arg_slice, span));
            }
        }
        // (b) `Type.static_method()` — a concrete (extendable) type.
        if let Some(def) = self.prog.resolve_type_in(self.current_module(), recv_name) {
            if matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
                return Some(self.check_type_static_call(def, callee, method, arg_slice, generics, span));
            }
        }
        None
    }

    /// `T.static_method(args)` where `T` is a generic parameter: the method must
    /// be a *static* method declared by one of `T`'s interface bounds. Codegen
    /// monomorphizes it to the concrete impl (`docs/10`).
    pub(crate) fn check_bound_static_call(
        &mut self,
        param: DefId,
        pty: Ty,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let Some((mdef, iface, iargs)) = self.resolve_bound_method(param, &method.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(method.span, SemaErrorKind::Message(format!(
                "no static method `{}` on type parameter `{}` through its bounds",
                method.name,
                self.display(pty)
            )));
            return self.tcx.error;
        };
        if !self.prog.def(mdef).is_static {
            self.emit(method.span, SemaErrorKind::Message(format!(
                "`{}` is an instance method; call it on a value, not on the type",
                method.name
            )));
        }
        self.record_res(callee.span, ValueRes::Method(mdef), self.tcx.error);
        let (params, ret) = self.iface_method_sig(mdef, iface, &iargs, pty);
        if args.len() != params.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: params.len(), found: args.len() });
        }
        for (a, pt) in args.iter().zip(&params) {
            let at = self.check_expr(a, Some(*pt));
            self.expect(at, *pt, a.span);
        }
        // Mark this a static call (with its receiver type) for the HIR `Call`
        // node — set after the args so a nested static call cannot clobber it.
        self.pending_static_recv.set(Some(pty));
        ret
    }

    /// `Type.static_method(args)` for a concrete extendable type: resolve a
    /// static method declared in an `extend` of `Type` (`docs/09` §6).
    /// When `Type` is generic and its type arguments are not annotated, infer
    /// them from the static method's argument types (`docs/11` §3) — so
    /// `Box.new(99)` deduces `Box<i64>` from the `99: i64`.
    pub(crate) fn check_type_static_call(
        &mut self,
        struct_def: DefId,
        callee: &Expr,
        method: &Ident,
        args: &[Expr],
        explicit_generics: &[Type],
        span: Span,
    ) -> Ty {
        // Form a parametric receiver: each struct generic stays a `Param` until
        // it is solved from the call's argument types (or remains an error if
        // there is nothing to infer it from).
        let struct_gens = self.prog.def(struct_def).generics.clone();
        let recv_args: Vec<Ty> = struct_gens.iter().map(|g| self.tcx.mk_param(*g)).collect();
        let recv_ty = self.tcx.mk_named(struct_def, recv_args);
        let Some((mdef, ext_subst)) = self.resolve_method(recv_ty, &method.name) else {
            for a in args {
                self.check_expr(a, None);
            }
            self.emit(method.span, SemaErrorKind::Message(format!(
                "type `{}` has no static method `{}`",
                self.prog.def(struct_def).name, method.name
            )));
            return self.tcx.error;
        };
        if !self.prog.def(mdef).is_static {
            self.emit(method.span, SemaErrorKind::Message(format!(
                "`{}` is an instance method on `{}`; call it on a value",
                method.name, self.prog.def(struct_def).name
            )));
        }
        self.record_res(callee.span, ValueRes::Method(mdef), self.tcx.error);

        // Substitution chain:
        //   * `ext_subst` maps the extend's generics → the struct's `Param`s.
        //   * The method's explicit `<...>` (if any) binds its own generics.
        //   * Inference fills in each struct generic from the matching argument.
        let mut subst = ext_subst.clone();
        let env = self.local_env();
        let method_gens = self.prog.def(mdef).generics.clone();
        for (g, t) in method_gens.iter().zip(explicit_generics) {
            let gt = self.lower_ty(t, &env);
            subst.insert(*g, gt);
        }
        let (menv, _) = self.fn_env(mdef);
        let Some(ItemKind::Function(f)) = self.prog.def(mdef).item.clone() else {
            return self.tcx.error;
        };
        // Parameter types with the partial substitution applied — any unsolved
        // struct generic still appears as `Param(g_struct)` here.
        let raw_param_tys: Vec<Ty> = f
            .params
            .iter()
            .filter_map(|p| match &p.kind {
                ParamKind::Normal { ty, .. } => {
                    let t = self.lower_ty(ty, &menv);
                    Some(self.subst_ty(t, &subst))
                }
                ParamKind::SelfParam => None,
            })
            .collect();

        // Infer the struct's generics by unifying each parameter against the
        // matching argument type (mirroring `check_generic_call`). With no
        // expected type passed in: a single check per arg, then re-`expect`
        // once the substitution is solved.
        for (i, a) in args.iter().enumerate() {
            let aty = self.check_expr(a, None);
            if let Some(pt) = raw_param_tys.get(i) {
                self.unify(*pt, aty, &mut subst);
            }
        }
        // The (now-solved) receiver type for codegen's static-call dispatch.
        let recv_args_solved: Vec<Ty> = struct_gens
            .iter()
            .map(|g| subst.get(g).copied().unwrap_or(self.tcx.error))
            .collect();
        let recv_ty_solved = self.tcx.mk_named(struct_def, recv_args_solved);
        // Static call (with solved receiver) for the HIR `Call` node; set after
        // arg inference so a nested static call cannot clobber the slot.
        self.pending_static_recv.set(Some(recv_ty_solved));
        // Report unsolved struct generics with a clear, struct-anchored message.
        for g in &struct_gens {
            if subst.get(g).is_none() {
                let gname = self.prog.def(*g).name.clone();
                self.emit(span, SemaErrorKind::Message(format!(
                    "cannot infer generic argument `{}` for `{}`; annotate it",
                    gname,
                    self.prog.def(struct_def).name
                )));
                subst.insert(*g, self.tcx.error);
            }
        }
        // Chain-resolve the substitution: an extend generic mapped to a struct
        // `Param` (e.g. `T_ext → Param(T_struct)`) should reach the struct
        // generic's solved type (e.g. `i64`) in one final lookup. Iterating
        // once is sufficient because chains are short (struct → extend → method),
        // but a small fixed-point bound guards against pathological cases.
        for _ in 0..struct_gens.len() + 2 {
            let keys: Vec<DefId> = subst.keys().copied().collect();
            let mut changed = false;
            for k in keys {
                let v = *subst.get(&k).expect("present");
                let resolved = self.subst_ty(v, &subst);
                if resolved != v {
                    subst.insert(k, resolved);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Final parameter types, with all generics substituted.
        let param_tys: Vec<Ty> = raw_param_tys
            .iter()
            .map(|t| self.subst_ty(*t, &subst))
            .collect();
        if args.len() != param_tys.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: param_tys.len(), found: args.len() });
        }
        for (i, a) in args.iter().enumerate() {
            if let Some(pt) = param_tys.get(i) {
                let at = self.expr_ty(a.span).unwrap_or(self.tcx.error);
                self.expect(at, *pt, a.span);
            }
        }
        // Record monomorphization args: the extend's generics (resolved through
        // the chain to concrete types), then the method's own generics.
        let parent = self.prog.def(mdef).parent;
        let ext_gens = parent.map(|p| self.prog.def(p).generics.clone()).unwrap_or_default();
        let mut targs: Vec<Ty> = ext_gens
            .iter()
            .map(|g| {
                let t = subst.get(g).copied().unwrap_or(self.tcx.error);
                self.subst_ty(t, &subst)
            })
            .collect();
        for g in &method_gens {
            targs.push(subst.get(g).copied().unwrap_or(self.tcx.error));
        }
        // Hand the solved type args to the HIR `Call` node (was `call_type_args`);
        // args were checked during inference above, none re-checked after here.
        if !targs.is_empty() {
            self.pending_type_args.set(Some(targs.clone()));
        }
        // Enforce each bound on the inferred type arguments.
        let mut all_gens = ext_gens.clone();
        all_gens.extend_from_slice(&method_gens);
        self.check_bounds(&all_gens, &targs, span);
        match &f.return_type {
            Some(t) => {
                let t = self.lower_ty(t, &menv);
                self.subst_ty(t, &subst)
            }
            None => self.tcx.null,
        }
    }

    /// Type-check `channel<T>(): (Sender<T>, Receiver<T>)` (`docs/20` §2).
    pub(crate) fn check_channel_new(&mut self, generics: &[Type], args: &[Expr], span: Span) -> Ty {
        if generics.len() != 1 {
            self.emit(span, SemaErrorKind::Message(
                "`channel` needs exactly one explicit type argument: `channel<T>()`".into(),
            ));
            return self.tcx.error;
        }
        let env = self.local_env();
        let elem = self.lower_ty(&generics[0], &env);
        // Only immutable element types are shared across threads for now (no
        // clone-on-send yet — `docs/20` §3); matches `Thread.spawn` captures.
        if !self.is_immutable_value(elem) && !self.tcx.is_error(elem) {
            self.emit(span, SemaErrorKind::Message(format!(
                "`channel` element type `{}` must be immutable so far (only \
                 primitives and `str` can cross threads without a deep clone)",
                self.display(elem)
            )));
        }
        if !args.is_empty() {
            self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
        }
        let sender = self.tcx.mk_named(self.prog.sender_def, vec![elem]);
        let receiver = self.tcx.mk_named(self.prog.receiver_def, vec![elem]);
        self.tcx.mk_tuple(vec![sender, receiver])
    }

    /// `Sender<T>` / `Receiver<T>` builtin methods (`docs/20` §2).
    pub(crate) fn check_channel_method(&mut self, def: DefId, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        let is_sender = def == self.prog.sender_def;
        match (is_sender, name.name.as_str()) {
            (true, "send") => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                } else {
                    let at = self.check_expr(&args[0], Some(elem));
                    self.expect(at, elem, args[0].span);
                }
                self.tcx.null
            }
            (false, "recv") => {
                // Async + non-blocking (`docs/20` §2 / `docs/21`): `recv()` builds
                // a `Future<T>` you `await` (or drive with `block_on`) rather than
                // parking the calling thread.
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.tcx.mk_named(self.prog.future_def, vec![elem])
            }
            (false, "try_recv") => {
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                self.tcx.mk_union([elem, self.tcx.null])
            }
            _ => {
                let tn = if is_sender { "Sender" } else { "Receiver" };
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`{tn}` has no method `{}`", name.name
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// Type-check `Shared.new(value): Shared<T>` (`docs/20` §4). `T` is inferred
    /// from the value.
    pub(crate) fn check_shared_new(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            for a in args {
                self.check_expr(a, None);
            }
            return self.tcx.error;
        }
        let elem = self.check_expr(&args[0], None);
        self.tcx.mk_named(self.prog.shared_def, vec![elem])
    }

    /// `Foreign.alloc<T>()` / `alloc_zeroed<T>()` / `free(p)` (`docs/19` §5):
    /// manual foreign-heap allocation. `alloc` returns `*T | null` (NPO);
    /// `free` takes a raw pointer and returns `null`.
    pub(crate) fn check_foreign_builtin(
        &mut self,
        method: &str,
        generics: &[Type],
        args: &[Expr],
        span: Span,
    ) -> Ty {
        match method {
            "alloc" | "alloc_zeroed" => {
                if generics.len() != 1 {
                    self.emit(span, SemaErrorKind::Message(format!(
                        "`Foreign.{method}` needs exactly one type argument, e.g. \
                         `Foreign.{method}<Pair>()` (`docs/19` §5)"
                    )));
                    return self.tcx.error;
                }
                if !args.is_empty() {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 0, found: args.len() });
                }
                let env = self.local_env();
                let t = self.lower_ty(&generics[0], &env);
                // The element must be C-ABI-compatible (it lives on the foreign
                // heap), or an extern struct.
                if !self.is_repr_c(t) && !self.is_extern_struct(t) {
                    self.emit(span, SemaErrorKind::Message(format!(
                        "`Foreign.{method}` requires a C-ABI (`ReprC`) type argument, \
                         got `{}` (`docs/19` §5)",
                        self.display(t)
                    )));
                    return self.tcx.error;
                }
                // `*T | null` — a raw nullable pointer (NPO).
                let ptr = self.tcx.mk_ptr(t);
                self.tcx.mk_union([ptr, self.tcx.null])
            }
            "free" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.null;
                }
                let at = self.check_expr(&args[0], None);
                if !matches!(self.tcx.kind(at), TyKind::Ptr(_)) && !self.is_npo_union(at) {
                    self.emit(args[0].span, SemaErrorKind::Message(format!(
                        "`Foreign.free` expects a raw pointer `*T`, got `{}`",
                        self.display(at)
                    )));
                }
                self.tcx.null
            }
            "realloc" => {
                if generics.len() != 1 {
                    self.emit(span, SemaErrorKind::Message(
                        "`Foreign.realloc` needs one type argument, e.g. \
                         `Foreign.realloc<Pair>(p, n)` (`docs/19` §5)".into(),
                    ));
                    return self.tcx.error;
                }
                if args.len() != 2 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 2, found: args.len() });
                    return self.tcx.error;
                }
                let env = self.local_env();
                let t = self.lower_ty(&generics[0], &env);
                let ptr_t = self.tcx.mk_ptr(t);
                let pt = self.check_expr(&args[0], Some(ptr_t));
                if !matches!(self.tcx.kind(pt), TyKind::Ptr(_)) && !self.is_npo_union(pt) {
                    self.emit(args[0].span, SemaErrorKind::Message(format!(
                        "`Foreign.realloc` expects a raw pointer `*T`, got `{}`", self.display(pt)
                    )));
                }
                let usize_t = self.tcx.int(IntTy::Usize);
                let szt = self.check_expr(&args[1], Some(usize_t));
                self.expect(szt, usize_t, args[1].span);
                self.tcx.mk_union([ptr_t, self.tcx.null])
            }
            "alloc_flex" => {
                if generics.len() != 2 {
                    self.emit(span, SemaErrorKind::Message(
                        "`Foreign.alloc_flex` needs two type arguments, e.g. \
                         `Foreign.alloc_flex<Msg, u8>(n)` (`docs/19` §5)".into(),
                    ));
                    return self.tcx.error;
                }
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                let env = self.local_env();
                let t = self.lower_ty(&generics[0], &env);
                let e = self.lower_ty(&generics[1], &env);
                for (ty, n) in [(t, 0), (e, 1)] {
                    if !self.is_repr_c(ty) && !self.is_extern_struct(ty) {
                        self.emit(span, SemaErrorKind::Message(format!(
                            "`Foreign.alloc_flex` type argument {} (`{}`) must be C-ABI \
                             (`ReprC`) (`docs/19` §5)", n + 1, self.display(ty)
                        )));
                    }
                }
                let usize_t = self.tcx.int(IntTy::Usize);
                let ct = self.check_expr(&args[0], Some(usize_t));
                self.expect(ct, usize_t, args[0].span);
                self.pending_foreign_flex.set(Some((t, e)));
                let ptr_t = self.tcx.mk_ptr(t);
                self.tcx.mk_union([ptr_t, self.tcx.null])
            }
            other => {
                self.emit(span, SemaErrorKind::Message(format!(
                    "`Foreign` has no method `{other}`; expected `alloc`, \
                     `alloc_zeroed`, or `free` (`docs/19` §5)"
                )));
                self.tcx.error
            }
        }
    }

    /// `CString.from_str(s): *u8` (`docs/19` §6): marshal a `str` into a fresh
    /// NUL-terminated C string on the foreign heap. The caller owns the buffer
    /// and releases it with `Foreign.free`.
    pub(crate) fn check_cstring_from_str(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            return self.tcx.error;
        }
        let at = self.check_expr(&args[0], Some(self.tcx.str));
        self.expect(at, self.tcx.str, args[0].span);
        self.tcx.mk_ptr(self.tcx.int(IntTy::U8))
    }

    /// `CStr.to_str(p): str` (`docs/19` §6): copy a NUL-terminated C string
    /// (any raw pointer) into a managed `str`.
    pub(crate) fn check_cstr_to_str(&mut self, args: &[Expr], span: Span) -> Ty {
        if args.len() != 1 {
            self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
            return self.tcx.error;
        }
        let at = self.check_expr(&args[0], None);
        if !matches!(self.tcx.kind(at), TyKind::Ptr(_)) {
            self.emit(args[0].span, SemaErrorKind::Message(format!(
                "`CStr.to_str` expects a raw pointer `*T`, got `{}`", self.display(at)
            )));
        }
        self.tcx.str
    }

    /// `Shared<T>` builtin methods (`docs/20` §4): `lock`/`try_lock` run a
    /// closure under the mutex with exclusive access to the value.
    pub(crate) fn check_shared_method(&mut self, elem: Ty, name: &Ident, args: &[Expr], span: Span) -> Ty {
        match name.name.as_str() {
            "lock" | "try_lock" => {
                if args.len() != 1 {
                    self.emit(span, SemaErrorKind::ArgCount { expected: 1, found: args.len() });
                    return self.tcx.error;
                }
                // The body is `(T) => R`; `R` is inferred from the closure.
                let want = self.tcx.mk_func(vec![elem], self.tcx.error, false);
                let cty = self.check_expr(&args[0], Some(want));
                let r = match self.tcx.kind(cty).clone() {
                    TyKind::Func { ret, .. } => ret,
                    _ => self.tcx.error,
                };
                if name.name == "lock" {
                    r
                } else {
                    let busy = self.tcx.mk_named(self.prog.lock_busy_def, Vec::new());
                    self.tcx.mk_union([r, busy])
                }
            }
            other => {
                self.emit(name.span, SemaErrorKind::Message(format!(
                    "`Shared` has no method `{other}`"
                )));
                for a in args {
                    self.check_expr(a, None);
                }
                self.tcx.error
            }
        }
    }

    /// `T.MIN`/`T.MAX` (integers) and `f*.INFINITY`/`NEG_INFINITY`/`NAN`
    /// (`docs/18` §10). Returns the constant's type, or `None` if `tyname` is not
    /// a primitive numeric type with that constant.
    pub(crate) fn check_num_constant(&mut self, tyname: &str, name: &Ident, field_span: Span) -> Option<Ty> {
        use crate::sema::results::{num_constant_of, NumIntrinsic};
        // Recognition lives in the shared `num_constant_of`; HIR lowering calls
        // the same helper, so no `num_intrinsics` side table is recorded.
        let _ = field_span;
        let intr = num_constant_of(&self.tcx, tyname, &name.name)?;
        Some(match intr {
            NumIntrinsic::IntBound { ty, .. } | NumIntrinsic::FloatConst { ty, .. } => ty,
            _ => unreachable!("num_constant_of yields only IntBound/FloatConst"),
        })
    }

    /// Numeric-namespace methods on a primitive type (`docs/18` §10, `docs/14`
    /// §5): the `{wrapping,saturating,checked,overflowing}_{add,sub,mul}` integer
    /// families and the `f*.is_nan`/`is_infinite`/`is_finite` float predicates.
    pub(crate) fn check_num_method(&mut self, tyname: &str, name: &Ident, args: &[Expr], span: Span) -> Option<Ty> {
        use crate::sema::results::{num_method_of, NumIntrinsic};
        // Recognition lives in the shared `num_method_of`; HIR lowering calls the
        // same helper, so no `num_intrinsics` side table is recorded here. This
        // function still validates argument arities and computes the result type.
        let intr = num_method_of(&self.tcx, tyname, &name.name)?;
        match intr {
            NumIntrinsic::FloatPred { ty, .. } => {
                self.check_num_args(args, &[ty], span);
                Some(self.tcx.bool)
            }
            // Argument arities differ by op (`docs/14` §5, `docs/18` §10): `neg`
            // is unary; `shl`/`shr` take `(T, u32)`; the rest take `(T, T)`.
            // Result by family: wrapping/saturating → T; checked → T | null;
            // overflowing → (T, bool).
            NumIntrinsic::IntArith { ty, family, op } => {
                let u32_ty = self.tcx.int(IntTy::U32);
                let expected: Vec<Ty> = match op {
                    5 => vec![ty],
                    6 | 7 => vec![ty, u32_ty],
                    _ => vec![ty, ty],
                };
                self.check_num_args(args, &expected, span);
                Some(match family {
                    2 => self.tcx.mk_union([ty, self.tcx.null]),
                    3 => self.tcx.mk_tuple(vec![ty, self.tcx.bool]),
                    _ => ty,
                })
            }
            _ => unreachable!("num_method_of yields only FloatPred/IntArith"),
        }
    }

    /// Check positional args against expected primitive types (for numeric
    /// intrinsics).
    pub(crate) fn check_num_args(&mut self, args: &[Expr], expect: &[Ty], span: Span) {
        if args.len() != expect.len() {
            self.emit(span, SemaErrorKind::ArgCount { expected: expect.len(), found: args.len() });
        }
        for (a, e) in args.iter().zip(expect) {
            let at = self.check_expr(a, Some(*e));
            self.expect(at, *e, a.span);
        }
    }

}
