//! Per-function codegen: struct/tuple construction, layout, and field access (`impl FnGen`, split from `lib.rs`).

use super::*;

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    // -- structs -------------------------------------------------------------

    /// Emit a managed-type descriptor blob (`docs/16` §3 — `gc` module) and
    /// return its address: `[size:u64][kind:u64][n_ptrs:u64][off:u32 …]`.
    pub(crate) fn emit_descriptor(&mut self, size: u32, kind: u64, ptr_offsets: &[u32]) -> Value {
        self.emit_descriptor_with(size, kind, 0, ptr_offsets)
    }

    /// Emit a type descriptor blob `[size][kind][type_id][n_ptrs][offsets…]`.
    /// `type_id` is `0` unless the type has a registered `Drop` finalizer
    /// (`docs/16` §8); the collector reads it to find the drop function.
    pub(crate) fn emit_descriptor_with(&mut self, size: u32, kind: u64, type_id: i64, ptr_offsets: &[u32]) -> Value {
        let mut bytes = Vec::with_capacity(32 + ptr_offsets.len() * 4);
        bytes.extend_from_slice(&(size as u64).to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&(type_id as u64).to_le_bytes());
        bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
        for o in ptr_offsets {
            bytes.extend_from_slice(&o.to_le_bytes());
        }
        let name = format!("desc.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare descriptor");
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &desc).expect("define descriptor");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Allocate a managed object for `layout`, returning the field-block ptr.
    pub(crate) fn alloc_struct(&mut self, layout: &Layout) -> Value {
        let desc = self.emit_descriptor(layout.size, GC_KIND_PLAIN, &layout.ptr_offsets);
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Allocate a managed object of nominal type `ty`. If `ty` has a `Drop` impl
    /// (`docs/16` §8), the descriptor carries its type id so the collector can
    /// find the finalizer; otherwise this is identical to [`alloc_struct`].
    pub(crate) fn alloc_struct_typed(&mut self, layout: &Layout, ty: Ty) -> Value {
        let tid = if self.ty_has_drop(ty) { self.type_id_of(ty) } else { 0 };
        let desc = self.emit_descriptor_with(layout.size, GC_KIND_PLAIN, tid, &layout.ptr_offsets);
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Allocate an `extern struct` instance on the stack (`docs/19` §3): a
    /// header-less, C-ABI-laid-out byte block. Returns its address — the
    /// runtime representation of the extern struct value. The GC never sees it
    /// (extern struct fields are scalars / raw pointers), and `&value` is this
    /// same address ("extern stack values: no pin needed", `docs/19` §2).
    pub(crate) fn alloc_extern(&mut self, layout: &Layout) -> Value {
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
            self.b.ins().store(MemFlags::trusted(), zero, addr, (w * 8) as i32);
        }
        addr
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

    /// Whether `ty` (after substitution) has a `Drop` implementation.
    pub(crate) fn ty_has_drop(&self, ty: Ty) -> bool {
        let drop_def = self.cx.analysis.program.drop_def;
        if drop_def == DefId(0) {
            return false;
        }
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        if let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(resolved) {
            return self.cx.analysis.results.iface_impls.contains_key(&(*def, drop_def));
        }
        false
    }

    pub(crate) fn gen_struct_lit(
        &mut self,
        def: DefId,
        args: &[Ty],
        fields: &[FieldInit],
        spread: Option<&Expr>,
        span: Span,
    ) -> CgResult<Value> {
        let layout = self.struct_layout(def, args);
        let sty = self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error);
        let ptr = if self.is_extern_struct_def(def) {
            self.alloc_extern(&layout)
        } else {
            self.alloc_struct_typed(&layout, sty)
        };

        // A spread base fills every field first; explicit fields override.
        if let Some(base) = spread {
            let base_ptr = self.gen_expr(base)?.ok_or_else(|| {
                CodegenError::new(base.span, "spread base has no value")
            })?;
            for i in 0..layout.offsets.len() {
                if let Some(ct) = layout.cltys[i] {
                    let off = layout.offsets[i] as i32;
                    let v = self.b.ins().load(ct, MemFlags::trusted(), base_ptr, off);
                    self.b.ins().store(MemFlags::trusted(), v, ptr, off);
                }
            }
        }

        for fi in fields {
            let Some(idx) = layout.index_of(&fi.name.name) else {
                return Err(CodegenError::new(fi.span, "unknown field in struct literal"));
            };
            let off = layout.offsets[idx] as i32;
            let val = match &fi.value {
                Some(e) => self.gen_expr(e)?,
                None => self.gen_local_use(fi.name.span)?, // field-init shorthand
            };
            // A nested extern struct field embeds its bytes inline.
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

    /// Construct a tuple struct from positional arguments. `span` is the call
    /// expression's span; its recorded type carries the (possibly inferred)
    /// generic arguments, so the layout is computed for the concrete instance.
    pub(crate) fn gen_tuple_ctor(
        &mut self,
        def: DefId,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        // A `@Transparent` newtype is its single field's value — no box.
        let cty = self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error);
        if transparent_inner(self.cx.analysis, cty).is_some() {
            return self.gen_expr(&args[0]);
        }
        // Prefer the call's result type (`S<A, B>`) so a generic tuple struct is
        // laid out for its inferred arguments; fall back to the bare def.
        let layout = match self.cx.analysis.results.expr_ty(span) {
            Some(ty) => self.layout_for_ty(ty).unwrap_or_else(|| self.struct_layout(def, &[])),
            None => self.struct_layout(def, &[]),
        };
        let ptr = if self.is_extern_struct_def(def) {
            self.alloc_extern(&layout)
        } else {
            self.alloc_struct(&layout)
        };
        for (i, a) in args.iter().enumerate() {
            let off = *layout.offsets.get(i).unwrap_or(&0) as i32;
            let v = self.gen_expr(a)?;
            if let (Some(v), Some(Some(_))) = (v, layout.cltys.get(i)) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, off);
            }
        }
        Ok(Some(ptr))
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
        match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)).clone() {
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
        for (w, ct) in [(8u32, types::I64), (4, types::I32), (2, types::I16), (1, types::I8)] {
            while i + w <= n {
                let v = self.b.ins().load(ct, MemFlags::trusted(), src, i as i32);
                self.b.ins().store(MemFlags::trusted(), v, dst, dst_off + i as i32);
                i += w;
            }
        }
    }

    /// Read a field (record name or tuple/struct position) from a pointer.
    pub(crate) fn gen_field_load(&mut self, receiver: &Expr, field: &str) -> CgResult<Option<Value>> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        // A `@Transparent` newtype's value *is* its single field — accessing
        // `.0` is the identity (`docs/19` §3).
        if transparent_inner(self.cx.analysis, rty).is_some() {
            return self.gen_expr(receiver);
        }
        let Some(layout) = self.layout_for_ty(rty) else {
            return Err(CodegenError::new(receiver.span, "field access on non-aggregate"));
        };
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "receiver has no value")
        })?;
        let Some(idx) = layout.index_of(field) else {
            return Err(CodegenError::new(receiver.span, "unknown field"));
        };
        let off = layout.offsets[idx] as i32;
        // A nested extern struct is laid out *inline*; its value is the address
        // of those bytes (`docs/19` §3), not a loaded pointer.
        if is_extern_struct_ty(self.cx.analysis, layout.tys[idx]) {
            return Ok(Some(self.b.ins().iadd_imm(ptr, off as i64)));
        }
        match layout.cltys[idx] {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, off))),
            None => Ok(None),
        }
    }

    /// Store `val` into a field/tuple-position target.
    pub(crate) fn gen_field_store(&mut self, receiver: &Expr, field: &str, val: Option<Value>)
        -> CgResult<()>
    {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let Some(layout) = self.layout_for_ty(rty) else {
            return Err(CodegenError::new(receiver.span, "field assignment on non-aggregate"));
        };
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "receiver has no value")
        })?;
        let Some(idx) = layout.index_of(field) else {
            return Err(CodegenError::new(receiver.span, "unknown field"));
        };
        let off = layout.offsets[idx] as i32;
        // Storing a nested extern struct embeds its bytes inline (`docs/19` §3).
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

    /// Load the local resolved at `span` (used for field-init shorthand and
    /// for `SelfExpr`). Routes through `read_local` so cell-backed locals
    /// (captured by some closure, `docs/09` §7) load through their cell.
    pub(crate) fn gen_local_use(&mut self, span: Span) -> CgResult<Option<Value>> {
        let local = self.resolve_local(span)?;
        self.read_local(local).map(Some).ok_or_else(|| {
            CodegenError::new(span, "use of unbound local")
        })
    }

}
