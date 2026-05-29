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
            return self.cx.hir.iface_impls.contains_key(&(*def, drop_def));
        }
        false
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

}
