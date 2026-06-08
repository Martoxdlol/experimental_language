//! Per-function codegen: struct/tuple construction, layout, and field access (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- structs -------------------------------------------------------------

    /// Emit a managed-type descriptor blob (`docs/16` §3 — `gc` module) and
    /// return its address: `[size:u64][kind:u64][n_ptrs:u64][off:u32 …]`.
    pub(crate) fn emit_descriptor(&mut self, size: u32, kind: u64, ptr_offsets: &[u32]) -> Value {
        self.emit_descriptor_full(size, kind, 0, ptr_offsets, &[])
    }

    /// Emit a full type descriptor blob, including the `@RefCounted`-child
    /// trailer `[n_rc][rcoff…]` (`docs/16` §8.1). The trailer is *always* written
    /// (possibly with `n_rc == 0`) so the runtime can read it unconditionally —
    /// `desc_rc_offsets` reads at `32 + n_ptrs*4` for every object it reclaims.
    /// `rc_offsets` lists the byte offsets of fields holding refcounted strong
    /// references (a subset of `ptr_offsets`); they are released on destruction
    /// by the runtime descriptor trailer.
    pub(crate) fn emit_descriptor_full(
        &mut self,
        size: u32,
        kind: u64,
        type_id: i64,
        ptr_offsets: &[u32],
        rc_offsets: &[u32],
    ) -> Value {
        let mut bytes = Vec::with_capacity(40 + (ptr_offsets.len() + rc_offsets.len()) * 4);
        bytes.extend_from_slice(&(size as u64).to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&(type_id as u64).to_le_bytes());
        bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
        for o in ptr_offsets {
            bytes.extend_from_slice(&o.to_le_bytes());
        }
        bytes.extend_from_slice(&(rc_offsets.len() as u32).to_le_bytes());
        for o in rc_offsets {
            bytes.extend_from_slice(&o.to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes()); // n_ep = 0 (endpoint counts are not aggregate-owned)
        let name = format!("desc.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare descriptor");
        let mut desc = DataDescription::new();
        desc.set_align(8); // the runtime reads `size`/`kind`/`type_id`/`n_ptrs` as `u64`
        desc.define(bytes.into_boxed_slice());
        self.module
            .define_data(data_id, &desc)
            .expect("define descriptor");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Allocate a managed object for `layout`, returning the field-block ptr.
    /// The descriptor carries `layout`'s refcounted-child trailer so the runtime
    /// releases any `@RefCounted` values this object owns when it is reclaimed
    /// (relevant for tuples / internal aggregates holding refcounted elements).
    pub(crate) fn alloc_struct(&mut self, layout: &Layout) -> Value {
        let desc = self.emit_descriptor_full(
            layout.size,
            GC_KIND_PLAIN,
            0,
            &layout.ptr_offsets,
            &layout.rc_offsets,
        );
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Allocate a managed object of nominal type `ty`. If `ty` has a `Drop` impl
    /// (`docs/16` §8), the descriptor carries its type id so the collector (or,
    /// for `@RefCounted`, the release path) can find the finalizer. A
    /// `@RefCounted` type (`docs/16` §8.1) is allocated as a `KIND_REFCOUNTED`
    /// object and its hidden strong-count word is stamped to `1` (the creating
    /// binding owns the one reference).
    pub(crate) fn alloc_struct_typed(&mut self, layout: &Layout, ty: Ty) -> CgResult<Value> {
        let tid = self.drop_type_id(ty)?;
        let refcounted = is_refcounted_ty(
            self.cx.analysis,
            resolve_shallow(self.cx.analysis, ty, &self.subst),
        );
        let kind = if refcounted {
            GC_KIND_REFCOUNTED
        } else {
            GC_KIND_PLAIN
        };
        let desc = self.emit_descriptor_full(
            layout.size,
            kind,
            tid,
            &layout.ptr_offsets,
            &layout.rc_offsets,
        );
        let ptr = self
            .call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        if refcounted {
            // Stamp the initial strong count (offset 0). `lang_alloc` zeroes the
            // object, so the count starts at 0; set it to 1 for the owner.
            let one = self.b.ins().iconst(types::I64, 1);
            self.b.ins().store(MemFlags::trusted(), one, ptr, 0);
        }
        Ok(ptr)
    }

    /// The object-header drop type id to stamp into `ty`'s descriptor (`docs/16`
    /// §8), or `0` when `ty` has no `Drop` impl.
    ///
    /// * No `Drop` ⇒ `0` (the collector skips finalization).
    /// * Non-generic `Drop` ⇒ `1000 + def.index()` — the stable per-`def` id that
    ///   [`Codegen::collect_drops`] registers against the type's `drop` method.
    /// * Generic `Drop` (`extend<T> S<T>: Drop`) ⇒ a per-instance id derived from
    ///   the *monomorphized* `drop` method's `FuncId`. Every `S<int>` shares one
    ///   `def` but needs its own `drop` glue, so the id must distinguish instances.
    ///   Declaring the instance here both pins down its `FuncId` (so the id
    ///   matches what `collect_drops` registers) and enqueues the `drop` body for
    ///   code generation — it is never reached through an ordinary call.
    fn drop_type_id(&mut self, ty: Ty) -> CgResult<i64> {
        let drop_def = self.cx.analysis.program.drop_def;
        if drop_def == DefId(0) {
            return Ok(0);
        }
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        let TyKind::Named { def, args } = self.cx.analysis.tcx.kind(resolved).clone() else {
            return Ok(0);
        };
        let Some(&ext) = self.cx.hir.iface_impls.get(&(def, drop_def)) else {
            return Ok(0);
        };
        if self.cx.analysis.program.def(ext).generics.is_empty() {
            // Non-generic `Drop` impl: stable per-`def` id. Declare the method
            // here as well as in whole-program seeds so root-based codegen can
            // trim unrelated bodies without trimming finalizers used only by an
            // allocation descriptor.
            let method = self.drop_method_of(ext).ok_or_else(|| {
                CodegenError::new(
                    self.cx.analysis.program.def(ext).span,
                    "`Drop` impl has no `drop` method",
                )
            })?;
            if !self.funcs.contains_key(&(method, Vec::new())) {
                declare_instance(
                    self.module,
                    self.funcs,
                    self.worklist,
                    self.cx.analysis,
                    method,
                    Vec::new(),
                )?
                .ok_or_else(|| {
                    CodegenError::new(
                        self.cx.analysis.program.def(method).span,
                        "`Drop` method is not lowerable",
                    )
                })?;
            }
            return Ok(1000 + def.index() as i64);
        }
        // Generic `Drop` impl: resolve the `drop` method and instantiate it for
        // this concrete receiver, keying the id on its `FuncId`.
        let method = self.drop_method_of(ext).ok_or_else(|| {
            CodegenError::new(
                self.cx.analysis.program.def(ext).span,
                "generic `Drop` impl has no `drop` method",
            )
        })?;
        let targs: Vec<Ty> = args
            .iter()
            .map(|a| resolve_shallow(self.cx.analysis, *a, &self.subst))
            .collect();
        let fid = match self.funcs.get(&(method, targs.clone())).copied() {
            Some(f) => f,
            None => declare_instance(
                self.module,
                self.funcs,
                self.worklist,
                self.cx.analysis,
                method,
                targs,
            )?
            .ok_or_else(|| {
                CodegenError::new(
                    self.cx.analysis.program.def(method).span,
                    "generic `Drop` method is not lowerable",
                )
            })?,
        };
        Ok(GENERIC_DROP_TID_BASE + fid.as_u32() as i64)
    }

    /// Find the `drop` method `DefId` declared under `extend_def` (a `Drop` impl).
    pub(crate) fn drop_method_of(&self, extend_def: DefId) -> Option<DefId> {
        let prog = &self.cx.analysis.program;
        (0..prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = prog.def(d);
            def.kind == DefKind::ExtendMethod
                && def.parent == Some(extend_def)
                && def.name == "drop"
        })
    }

    /// Allocate an `extern struct` instance on the stack (`docs/19` §3): a
    /// header-less, C-ABI-laid-out byte block. Returns its address — the
    /// runtime representation of the extern struct value. The GC never sees it
    /// (extern struct fields are scalars / raw pointers), and `&value` is this
    /// same address ("extern stack values: no pin needed", `docs/19` §2).
    pub(crate) fn alloc_extern(&mut self, layout: &Layout) -> Value {
        self.alloc_stack_block(layout)
    }

    /// Allocate a zeroed stack field-block for a non-escaping, heap-layout
    /// ordinary struct. This deliberately has the same field-block pointer
    /// representation as managed structs within the frame, but no object header
    /// or descriptor because the value is proven local and has no traced fields.
    pub(crate) fn alloc_stack_struct(&mut self, layout: &Layout) -> Value {
        self.alloc_stack_block(layout)
    }

    /// A nominal struct whose runtime value carries no field data and no
    /// finalization/ownership obligation can use a null field-block sentinel.
    /// Union/dynamic boxing still records the type id; interface objects still
    /// record the vtable. This only removes the otherwise-empty payload object.
    pub(crate) fn is_zero_sized_final_struct_ty(&self, ty: Ty) -> bool {
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        if is_refcounted_ty(self.cx.analysis, resolved) {
            return false;
        }
        let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(resolved) else {
            return false;
        };
        let d = self.cx.analysis.program.def(*def);
        if d.kind != DefKind::Struct || d.attrs.iter().any(|a| a.name.name == "Transparent") {
            return false;
        }
        let drop_def = self.cx.analysis.program.drop_def;
        if drop_def != DefId(0) && self.cx.hir.iface_impls.contains_key(&(*def, drop_def)) {
            return false;
        }
        match self.cx.hir.structs.get(def) {
            Some(compiler::sema::StructFields::Unit) => true,
            Some(compiler::sema::StructFields::Tuple(fields)) => fields.is_empty(),
            Some(compiler::sema::StructFields::Record(fields)) => fields.is_empty(),
            None => false,
        }
    }

    fn alloc_stack_block(&mut self, layout: &Layout) -> Value {
        let size = layout.size.max(1);
        let align = layout.align.max(1);
        // Cranelift only reliably honors stack-slot alignment up to the stack's
        // natural alignment (16 bytes on the targets we emit). For an over-aligned
        // `@Align(N>16)` extern struct, over-allocate by `align` and round the
        // base address up so the block is genuinely `N`-aligned.
        // Round the slot up to a whole number of 8-byte words so the block can
        // be zeroed word-by-word (an extern struct literal may omit fields,
        // including fixed arrays, which then read back as zero — `docs/19` §4).
        let words = size.div_ceil(8);
        let slot_bytes = words * 8;
        let addr = if align <= 16 {
            let align_shift = align.trailing_zeros() as u8;
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                slot_bytes,
                align_shift,
            ));
            self.b.ins().stack_addr(PTR, slot, 0)
        } else {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                slot_bytes + align,
                4,
            ));
            let base = self.b.ins().stack_addr(PTR, slot, 0);
            let mask = (align - 1) as i64;
            let bumped = self.b.ins().iadd_imm(base, mask);
            self.b.ins().band_imm(bumped, !mask)
        };
        let zero = self.b.ins().iconst(types::I64, 0);
        for w in 0..words {
            self.b
                .ins()
                .store(MemFlags::trusted(), zero, addr, (w * 8) as i32);
        }
        addr
    }

    // -- `@RefCounted` ARC retain/release (`docs/16` §8.1) ------------------

    /// Whether `local` is an *owned* `@RefCounted` local: its type resolves to a
    /// `@RefCounted` struct and it is **not** cell-backed (captured into a
    /// closure). Captured locals escape their frame and are reclaimed by the GC
    /// backstop rather than scope-bound ARC, so they are excluded.
    pub(crate) fn is_rc_local(&self, local: LocalId) -> bool {
        if self.cx.captured_locals.contains(&local) {
            return false;
        }
        match self.cx.analysis.hir.local_ty(local) {
            Some(ty) => is_refcounted_ty(
                self.cx.analysis,
                resolve_shallow(self.cx.analysis, ty, &self.subst),
            ),
            None => false,
        }
    }

    /// Whether `ty` (after this instance's substitution) is a `@RefCounted` struct.
    pub(crate) fn is_rc_ty(&self, ty: Ty) -> bool {
        is_refcounted_ty(
            self.cx.analysis,
            resolve_shallow(self.cx.analysis, ty, &self.subst),
        )
    }

    /// Emit a strong-count retain (`+1`) of a `@RefCounted` field-block pointer.
    pub(crate) fn emit_rc_retain(&mut self, v: Value) {
        self.call_intrinsic("lang_rc_retain", &[PTR], None, &[v]);
    }

    /// Emit a strong-count release (`-1`); at zero this runs the type's `Drop`
    /// immediately as non-waiting cleanup and frees the object (`docs/16` §8.1).
    pub(crate) fn emit_rc_release(&mut self, v: Value) {
        self.call_intrinsic("lang_rc_release", &[PTR], None, &[v]);
    }

    /// Whether HIR expression `e` *produces* an owned `@RefCounted` value (a
    /// fresh `+1`) rather than borrowing an existing reference. Conservative:
    /// only struct constructions and calls are owned — calls return `+1` by the
    /// codegen's return convention (see `emit_return`, which retains a borrowed
    /// return value). Everything else is treated as a borrow, so consuming it
    /// *retains*. This never moves a borrowed reference (which could double
    /// free); at worst it over-retains a genuinely owned value, and the GC
    /// backstop reclaims that.
    pub(crate) fn is_owned_rc_expr(e: &compiler::hir::Expr) -> bool {
        matches!(
            e.kind,
            compiler::hir::ExprKind::Struct { .. } | compiler::hir::ExprKind::Call { .. }
        )
    }

    /// Whether HIR expression `e` produces an owned channel endpoint handle.
    /// Calls follow the function-return convention, `.clone()` is a call, and
    /// list indexing clones the list-owned endpoint before yielding it. Names
    /// and aggregate fields are borrowed handles and must acquire at bind.
    pub(crate) fn is_owned_endpoint_expr(e: &compiler::hir::Expr) -> bool {
        matches!(
            e.kind,
            compiler::hir::ExprKind::Call { .. } | compiler::hir::ExprKind::Index { .. }
        )
    }

    /// Manage a `@RefCounted` local at a `let`/destructure bind. `init_owned` is
    /// whether the bound value carried a fresh `+1` (a move) or was borrowed (so
    /// this binding needs its own retained `+1`). Re-binding a local that was
    /// already bound (a loop body's `var`) releases its prior value first.
    pub(crate) fn rc_manage_bind(&mut self, local: LocalId, v: Value, init_owned: bool) {
        let rebind = self.rc_owned.contains(&local);
        let old = if rebind { self.read_local(local) } else { None };
        if !init_owned {
            self.emit_rc_retain(v);
        }
        if let Some(old) = old {
            self.emit_rc_release(old);
        }
        if !rebind {
            self.rc_owned.push(local);
        }
    }

    /// Prepare a return value for the `+1`-return convention (`docs/16` §8.1):
    /// if the returned expression `e` *borrows* a `@RefCounted` value (rather
    /// than producing a fresh one), retain it so the caller receives an owned
    /// `+1` that survives `emit_return`'s release of this frame's owned locals.
    /// An owned return (a construction/call temporary) is already `+1`.
    pub(crate) fn rc_return_value(
        &mut self,
        e: Option<&compiler::hir::Expr>,
        val: Option<Value>,
    ) -> Option<Value> {
        if let (Some(e), Some(v)) = (e, val) {
            if self.is_rc_ty(e.ty) && !Self::is_owned_rc_expr(e) {
                self.emit_rc_retain(v);
            }
        }
        val
    }

    /// Release every owned `@RefCounted` local (reverse binding order). Emitted
    /// by `emit_return` on every return path; locals not bound on the running
    /// path read back as null (Cranelift's undefined-variable default) and
    /// release is a no-op, so this is safe across conditional binds.
    pub(crate) fn rc_release_owned_locals(&mut self) {
        let owned = self.rc_owned.clone();
        for local in owned.into_iter().rev() {
            if let Some(v) = self.read_local(local) {
                self.emit_rc_release(v);
            }
        }
    }

    /// Manage a channel endpoint local at a `let`/destructure bind. Endpoint
    /// handles own one runtime endpoint reference while the frame is alive.
    /// Rebinding releases the previous endpoint before the new value replaces it.
    pub(crate) fn endpoint_manage_bind(
        &mut self,
        local: LocalId,
        ty: Ty,
        init: Value,
        _init_owned: bool,
        span: Span,
    ) -> CgResult<()> {
        let Some(is_sender) = self.channel_endpoint_kind(ty) else {
            return Ok(());
        };
        let rebind = self.endpoint_owned.iter().any(|(l, _, _)| *l == local);
        if rebind {
            if let Some(old) = self.read_local(local) {
                let chan = self.emit_channel_id(old, ty, span)?;
                let name = if is_sender {
                    "lang_chan_sender_release"
                } else {
                    "lang_chan_receiver_release"
                };
                self.call_intrinsic(name, &[types::I64], None, &[chan]);
                self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[old]);
            }
        } else {
            self.endpoint_owned.push((local, ty, is_sender));
        }
        self.call_intrinsic("lang_gc_pin", &[PTR], None, &[init]);
        Ok(())
    }

    /// Release the old endpoint held by `local` before an assignment. A first
    /// assignment to an endpoint local starts tracking it for function exit.
    pub(crate) fn endpoint_release_assignment_old(
        &mut self,
        local: LocalId,
        ty: Ty,
        span: Span,
    ) -> CgResult<()> {
        let Some(is_sender) = self.channel_endpoint_kind(ty) else {
            return Ok(());
        };
        let tracked = self.endpoint_owned.iter().any(|(l, _, _)| *l == local);
        if let Some(old) = self.read_local(local) {
            let chan = self.emit_channel_id(old, ty, span)?;
            let name = if is_sender {
                "lang_chan_sender_release"
            } else {
                "lang_chan_receiver_release"
            };
            self.call_intrinsic(name, &[types::I64], None, &[chan]);
            self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[old]);
        }
        if !tracked {
            self.endpoint_owned.push((local, ty, is_sender));
        }
        Ok(())
    }

    /// Pin the new endpoint object assigned into an owned endpoint local. The
    /// channel reference count was established by the expression that produced
    /// the endpoint; this pin keeps the tiny handle object itself alive until
    /// the local is reassigned, returned, captured by value, or released.
    pub(crate) fn endpoint_pin_assignment_new(&mut self, ty: Ty, v: Value) {
        if self.channel_endpoint_kind(ty).is_some() {
            self.call_intrinsic("lang_gc_pin", &[PTR], None, &[v]);
        }
    }

    /// Retain a returned endpoint local so ownership transfers to the caller
    /// before this frame releases its own endpoint locals.
    pub(crate) fn endpoint_return_value(
        &mut self,
        e: Option<&compiler::hir::Expr>,
        val: Option<Value>,
    ) -> CgResult<()> {
        let (Some(e), Some(v)) = (e, val) else {
            return Ok(());
        };
        let compiler::hir::ExprKind::Name(compiler::hir::Res::Local(local)) = e.kind else {
            return Ok(());
        };
        let Some((_, ty, is_sender)) = self
            .endpoint_owned
            .iter()
            .find(|(l, _, _)| *l == local)
            .copied()
        else {
            return Ok(());
        };
        let chan = self.emit_channel_id(v, ty, e.span)?;
        let name = if is_sender {
            "lang_chan_sender_acquire"
        } else {
            "lang_chan_receiver_acquire"
        };
        self.call_intrinsic(name, &[types::I64], None, &[chan]);
        Ok(())
    }

    /// Release every endpoint local owned by this frame in reverse binding
    /// order. Locals not bound on the running path read back as null-like zeroes;
    /// the runtime release helpers tolerate inactive channel ids.
    pub(crate) fn endpoint_release_owned_locals(&mut self) -> CgResult<()> {
        let owned = self.endpoint_owned.clone();
        for (local, ty, is_sender) in owned.into_iter().rev() {
            if let Some(v) = self.read_local(local) {
                let chan = self.emit_channel_id(v, ty, Span::dummy())?;
                let name = if is_sender {
                    "lang_chan_sender_release"
                } else {
                    "lang_chan_receiver_release"
                };
                self.call_intrinsic(name, &[types::I64], None, &[chan]);
                self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[v]);
            }
        }
        Ok(())
    }

    /// Whether `def` is an `extern struct` (laid out on the stack, no header).
    pub(crate) fn is_extern_struct_def(&self, def: DefId) -> bool {
        self.cx.analysis.program.def(def).kind == DefKind::ExternStruct
    }

    /// The byte size of `ty` for a foreign allocation (`docs/19` §5): an extern
    /// struct's C layout size, or a scalar / fixed array's size.
    pub(crate) fn sizeof_ty(&self, ty: Ty) -> u32 {
        let r = resolve_shallow(self.cx.analysis, ty, &self.subst);
        if let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(r) {
            if self.is_extern_struct_def(*def) {
                return self.layout_for_ty(r).map(|l| l.size).unwrap_or(0);
            }
        }
        field_size_align(self.cx.analysis, r).0
    }

    /// The field-block layout of a struct named-type, with its generic
    /// arguments (resolved through this instance's substitution).
    pub(crate) fn struct_layout(&self, def: DefId, args: &[Ty]) -> Layout {
        let rargs: Vec<Ty> = args
            .iter()
            .map(|a| resolve_shallow(self.cx.analysis, *a, &self.subst))
            .collect();
        compute_layout(self.cx.analysis, def, &rargs)
    }

    /// The field-block layout of a struct or tuple-typed value.
    pub(crate) fn layout_for_ty(&self, ty: Ty) -> Option<Layout> {
        match self
            .cx
            .analysis
            .tcx
            .kind(resolve_shallow(self.cx.analysis, ty, &self.subst))
            .clone()
        {
            TyKind::Named { def, args } => Some(self.struct_layout(def, &args)),
            TyKind::Tuple(elems) => {
                let re: Vec<Ty> = elems
                    .iter()
                    .map(|e| resolve_shallow(self.cx.analysis, *e, &self.subst))
                    .collect();
                Some(tuple_layout(self.cx.analysis, &re))
            }
            _ => None,
        }
    }

    /// Copy `n` bytes from `src` to `dst + dst_off` in 8/4/2/1-byte chunks
    /// (sizes are compile-time constants). Used to embed a nested extern struct
    /// inline (`docs/19` §3).
    pub(crate) fn copy_bytes(&mut self, dst: Value, dst_off: i32, src: Value, n: u32) {
        let mut i = 0u32;
        for (w, ct) in [
            (8u32, types::I64),
            (4, types::I32),
            (2, types::I16),
            (1, types::I8),
        ] {
            while i + w <= n {
                let v = self.b.ins().load(ct, MemFlags::trusted(), src, i as i32);
                self.b
                    .ins()
                    .store(MemFlags::trusted(), v, dst, dst_off + i as i32);
                i += w;
            }
        }
    }

    /// Copy a by-value `extern struct` (`v` is a pointer to its stack/inline/
    /// foreign field block) into a fresh managed heap block and return the
    /// block's pointer. The copy holds plain C bytes — its own `*T` fields point
    /// into unmanaged memory — so the GC traces references *to* the block but
    /// never scans inside it (`GC_KIND_PLAIN`, empty trace map). This gives an
    /// extern struct a stable home when it escapes the frame that built it:
    /// boxed into a union/`dynamic` (see `box_value`) or returned by value (see
    /// `emit_return`). `ty` must resolve to an extern struct.
    pub(crate) fn heap_copy_extern(&mut self, v: Value, ty: Ty) -> Value {
        let size = self.sizeof_ty(ty);
        let block = (size.div_ceil(8) * 8).max(8);
        let desc = self.emit_descriptor(block, GC_KIND_PLAIN, &[]);
        let copy = self
            .call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        self.copy_bytes(copy, 0, v, size);
        copy
    }
}
