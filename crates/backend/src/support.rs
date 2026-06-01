//! Shared codegen support (`docs/22` §4): pure helpers over `Analysis` and the
//! Cranelift `Module` used by both the whole-program driver and the per-function
//! code generator — type lowering, struct/tuple layout, monomorphization
//! instance management, and async-body (state-machine) analysis. Factored out of
//! the per-function generator so each layer stays navigable.

use crate::{CgResult, CodegenError, Instance, PTR};
use compiler::ast::*;
use compiler::ids::{DefId, LocalId};
use compiler::sema::{Analysis, DefKind, StructFields};
use compiler::span::Span;
use compiler::ty::{FloatTy, IntTy, Ty, TyKind};
use cranelift_codegen::ir::{types, AbiParam, Type as ClType};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module};
use std::collections::{HashMap, HashSet};

/// The C calling convention an `extern function` should be called with,
/// selected by its `@CallConv("c"|"system"|"stdcall"|"fastcall")` decorator
/// (`docs/19` §7) or the platform default when absent.
///
/// On every 64-bit target we emit, the four spellings coincide with the
/// platform's single C convention: `"c"` and `"system"` are the platform ABI,
/// and `"stdcall"`/`"fastcall"` are 32-bit x86 conventions that 64-bit ABIs
/// fold into the default — exactly as a C compiler treats them. Reading the
/// (checker-validated) string keeps the policy in one place and would let a
/// future 32-bit x86 port diverge here. `default` should be the module ISA's
/// `default_call_conv()`.
pub(crate) fn extern_call_conv(analysis: &Analysis, def: DefId, default: CallConv) -> CallConv {
    let conv = analysis
        .program
        .def(def)
        .attrs
        .iter()
        .find(|a| a.name.name == "CallConv")
        .and_then(|a| match a.args.as_slice() {
            [AttrArg::Positional(e)] => match &e.kind {
                ExprKind::Str(s) => match s.parts.as_slice() {
                    [StringPart::Text { text, .. }] => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        });
    match conv.as_deref() {
        // All C-ABI spellings resolve to the platform default on 64-bit targets.
        Some("c") | Some("system") | Some("stdcall") | Some("fastcall") | None => default,
        // The checker rejects any other value before codegen reaches here.
        Some(_) => default,
    }
}

/// Whether an `extern function` def is declared `@Variadic` (`docs/19` §13) —
/// i.e. its call must be lowered through `libffi` rather than as an ordinary
/// fixed-arity Cranelift call (see [`crate::gen_hir`]'s `h_variadic_call`).
pub(crate) fn is_variadic_extern(analysis: &Analysis, def: DefId) -> bool {
    analysis.program.def(def).kind == DefKind::ExternFunction
        && analysis.program.def(def).attrs.iter().any(|a| a.name.name == "Variadic")
}

/// The `libffi` type tag (`runtime::variadic::VTAG_*`) for a C-ABI scalar or
/// pointer type, or `None` if the type cannot cross a variadic C call.
///
/// When `promote` is set — the argument is in *variadic* position — the C
/// default argument promotions are applied: `f32` → `f64`, and any integer
/// narrower than 32 bits (including `bool`/`char`) → 32-bit `int`. Named
/// (fixed-prefix) arguments and the return type use `promote = false`, taking
/// their declared C type. `@Transparent` newtypes are seen through to their
/// inner field's ABI (`docs/19` §3).
pub(crate) fn variadic_tag(analysis: &Analysis, ty: Ty, promote: bool) -> Option<u8> {
    use runtime::variadic as v;
    if let Some(inner) = transparent_inner(analysis, ty) {
        return variadic_tag(analysis, inner, promote);
    }
    Some(match analysis.tcx.kind(ty) {
        TyKind::Int(it) => match it {
            IntTy::I8 => if promote { v::VTAG_I32 } else { v::VTAG_I8 },
            IntTy::U8 => if promote { v::VTAG_I32 } else { v::VTAG_U8 },
            IntTy::I16 => if promote { v::VTAG_I32 } else { v::VTAG_I16 },
            IntTy::U16 => if promote { v::VTAG_I32 } else { v::VTAG_U16 },
            IntTy::I32 => v::VTAG_I32,
            IntTy::U32 => v::VTAG_U32,
            IntTy::I64 | IntTy::Isize => v::VTAG_I64,
            IntTy::U64 | IntTy::Usize => v::VTAG_U64,
        },
        // `bool` is a 1-byte `_Bool` when named; it promotes to `int` as a
        // variadic. `char` is a 32-bit Unicode scalar (C `int`-width).
        TyKind::Bool => if promote { v::VTAG_I32 } else { v::VTAG_U8 },
        TyKind::Char => v::VTAG_I32,
        TyKind::Float(FloatTy::F32) => if promote { v::VTAG_F64 } else { v::VTAG_F32 },
        TyKind::Float(FloatTy::F64) => v::VTAG_F64,
        // Raw FFI pointers and extern function pointers are machine pointers.
        // (`str` is deliberately excluded — it is a managed `LangStr`, not a C
        // `char*`; the checker rejects it as a variadic argument.)
        TyKind::Ptr(_) => v::VTAG_PTR,
        TyKind::Func { is_extern: true, .. } => v::VTAG_PTR,
        _ => return None,
    })
}

// -- async body analysis (state-machine lowering, `docs/21`) ----------------

/// The state-struct layout for an async body that suspends: `[state @0][inner
/// future @8][local_i @16 + i*8]` over every body local (the `entry` locals —
/// parameters or captures — come first, since the constructor pre-stores them).
pub(crate) struct AsyncLayout {
    /// Local → byte offset within the state struct.
    pub(crate) slot_off: HashMap<LocalId, i32>,
    /// Locals that carry a runtime value (with offset + Cranelift type) — the
    /// ones the state machine saves and restores.
    pub(crate) live: Vec<(LocalId, i32, ClType)>,
    /// Managed-pointer field offsets for the GC descriptor (includes `inner`).
    pub(crate) ptr_offsets: Vec<u32>,
    /// Total state-struct size in bytes.
    pub(crate) state_size: u32,
    /// For each sync `for` loop (keyed by its `iter.span`) whose body contains an
    /// `await`: the state-struct offsets `(primary, secondary, index)` reserved to
    /// hold its iteration state across suspends. `primary`/`secondary` are managed
    /// (traced) iterable/snapshot pointers — the `Map` driver uses both (the
    /// snapshot keys list + the map), other drivers only `primary`; `index` is a
    /// plain `i64` counter.
    pub(crate) for_slots: HashMap<Span, (i32, i32, i32)>,
}

/// Offset of the suspended-inner-future slot in every async state struct.
pub(crate) const ASYNC_INNER_OFF: i32 = 8;

/// A view of an async/function body's typed HIR block. The async state-machine
/// codegen drives the body walk, the `await`-site scan, and the local collection
/// through this thin wrapper.
#[derive(Clone, Copy)]
pub(crate) struct BodyView<'a>(pub(crate) &'a compiler::hir::Block);

impl<'a> BodyView<'a> {
    pub(crate) fn has_await(&self) -> bool {
        crate::gen_hir::h_block_has_await(self.0)
    }
    pub(crate) fn scan_awaits(&self, out: &mut Vec<Span>) {
        crate::gen_hir::h_scan_stmt_awaits(self.0, out)
    }
}

pub(crate) fn async_state_layout(
    analysis: &Analysis,
    subst: &HashMap<DefId, Ty>,
    entry: &[LocalId],
    body: BodyView,
    captured_locals: &HashSet<LocalId>,
) -> AsyncLayout {
    let mut all_locals = entry.to_vec();
    let mut seen: HashSet<LocalId> = all_locals.iter().copied().collect();
    crate::gen_hir::h_collect_block_locals(body.0, &mut all_locals, &mut seen);
    let mut slot_off = HashMap::new();
    let mut ptr_offsets = vec![ASYNC_INNER_OFF as u32]; // the inner-future slot is managed
    let mut live = Vec::new();
    for (i, l) in all_locals.iter().enumerate() {
        let off = (16 + i * 8) as i32;
        slot_off.insert(*l, off);
        // Captured locals are cell-backed (`docs/09` §7): the Cranelift
        // variable holds a managed cell pointer, regardless of the local's
        // declared (content) type. The async state saves/restores that
        // pointer, so the slot is `PTR` and the descriptor traces it.
        if captured_locals.contains(l) {
            live.push((*l, off, PTR));
            ptr_offsets.push(off as u32);
            continue;
        }
        let ty = analysis.hir.local_ty(*l).unwrap_or(analysis.tcx.error);
        let resolved = resolve_shallow(analysis, ty, subst);
        if let Some(ct) = clty_of(analysis, resolved) {
            live.push((*l, off, ct));
            if is_managed_ptr(analysis, resolved) {
                ptr_offsets.push(off as u32);
            }
        }
    }
    // Reserve two slots — an (managed) iterable pointer and an `i64` index — for
    // each sync `for` loop whose body awaits, so its iteration state survives the
    // `poll` returns (it is otherwise held in non-persistent Cranelift SSA).
    let mut for_spans = Vec::new();
    crate::gen_hir::h_scan_for_state(body.0, &mut for_spans);
    let mut for_slots = HashMap::new();
    let mut next_off = 16 + all_locals.len() * 8;
    for sp in for_spans {
        if for_slots.contains_key(&sp) {
            continue;
        }
        let primary = next_off as i32;
        let secondary = (next_off + 8) as i32;
        let idx_off = (next_off + 16) as i32;
        ptr_offsets.push(primary as u32); // both iterable slots are managed roots
        ptr_offsets.push(secondary as u32);
        for_slots.insert(sp, (primary, secondary, idx_off));
        next_off += 24;
    }
    let state_size = next_off as u32;
    AsyncLayout { slot_off, live, ptr_offsets, state_size, for_slots }
}

/// The generic-parameter → argument substitution for an instance.
pub(crate) fn build_subst(analysis: &Analysis, def: DefId, args: &[Ty]) -> HashMap<DefId, Ty> {
    let prog = &analysis.program;
    // For an `extend`/interface method, the enclosing block's generics (e.g. the
    // `T` of `extend<T> Pair<T>`) come first; the method's own generics follow.
    let mut params: Vec<DefId> = Vec::new();
    if let Some(parent) = prog.def(def).parent {
        if matches!(prog.def(parent).kind, DefKind::Extend) {
            params.extend(prog.def(parent).generics.iter().copied());
        }
    }
    params.extend(prog.def(def).generics.iter().copied());
    params.into_iter().zip(args.iter().copied()).collect()
}

/// A unique Cranelift symbol for an instance: name, def id, and arg type ids.
pub(crate) fn mangle(analysis: &Analysis, def: DefId, args: &[Ty]) -> String {
    let mut s = format!("{}${}", analysis.program.def(def).name, def.index());
    for a in args {
        s.push('_');
        s.push_str(&type_id(analysis, *a).to_string());
    }
    s
}

/// Shallow-substitute a top-level `Param` (sufficient for clty/type_id/layout).
pub(crate) fn resolve_shallow(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Ty {
    if subst.is_empty() {
        return ty;
    }
    match analysis.tcx.kind(ty) {
        TyKind::Param(d) => subst.get(d).copied().unwrap_or(ty),
        _ => ty,
    }
}

pub(crate) fn clty_subst(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Option<ClType> {
    clty_of(analysis, resolve_shallow(analysis, ty, subst))
}

/// Build a function/method instance's Cranelift signature under `subst`, or
/// `None` if a parameter type is not lowerable.
pub(crate) fn signature_of(
    module: &mut impl Module,
    analysis: &Analysis,
    def: DefId,
    subst: &HashMap<DefId, Ty>,
) -> CgResult<Option<cranelift_codegen::ir::Signature>> {
    // The function's HIR signature carries its parameter (LocalId, Ty) pairs and
    // return type (the checker built it; Stage 5 retired `fn_params`/`fn_return`).
    let fsig = analysis.hir.fn_sigs.get(&def);
    let ret = fsig.map(|s| s.ret).unwrap_or(analysis.tcx.null);
    let mut sig = module.make_signature();
    if let Some(fsig) = fsig {
        for (_, ty) in &fsig.params {
            match clty_subst(analysis, *ty, subst) {
                Some(ct) => sig.params.push(AbiParam::new(ct)),
                None => return Ok(None),
            }
        }
    }
    if let Some(ct) = clty_subst(analysis, ret, subst) {
        sig.returns.push(AbiParam::new(ct));
    }
    Ok(Some(sig))
}

/// Declare an instance (idempotently), queuing it for definition. Returns
/// `None` if its signature is not lowerable.
pub(crate) fn declare_instance(
    module: &mut impl Module,
    funcs: &mut HashMap<Instance, FuncId>,
    worklist: &mut Vec<Instance>,
    analysis: &Analysis,
    def: DefId,
    args: Vec<Ty>,
) -> CgResult<Option<FuncId>> {
    let inst = (def, args);
    if let Some(&f) = funcs.get(&inst) {
        return Ok(Some(f));
    }
    let subst = build_subst(analysis, def, &inst.1);
    let Some(sig) = signature_of(module, analysis, def, &subst)? else {
        return Ok(None);
    };
    let name = mangle(analysis, def, &inst.1);
    let fid = module
        .declare_function(&name, Linkage::Export, &sig)
        .map_err(|e| CodegenError::new(analysis.program.def(def).span, format!("declare: {e}")))?;
    funcs.insert(inst.clone(), fid);
    worklist.push(inst);
    Ok(Some(fid))
}

/// Shared, immutable codegen context handed to the per-function generator.

pub(crate) fn clty_of(analysis: &Analysis, ty: Ty) -> Option<ClType> {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => Some(int_clty(*it)),
        TyKind::Float(FloatTy::F32) => Some(types::F32),
        TyKind::Float(FloatTy::F64) => Some(types::F64),
        TyKind::Bool => Some(types::I8),
        TyKind::Char => Some(types::I32),
        // `str` is a managed reference — a pointer (to a runtime `LangStr`).
        TyKind::Str => Some(PTR),
        // A `@Transparent` newtype has its inner field's representation/ABI.
        TyKind::Named { .. } if transparent_inner(analysis, ty).is_some() => {
            clty_of(analysis, transparent_inner(analysis, ty).unwrap())
        }
        // Structs are managed references (a pointer to the field block); an
        // interface object is a pointer to a `{vtable, data}` fat-pointer box.
        TyKind::Named { def, .. }
            if matches!(
                analysis.program.def(*def).kind,
                DefKind::Struct | DefKind::ExternStruct | DefKind::Interface
            ) =>
        {
            Some(PTR)
        }
        // Anonymous tuples are heap-boxed records — a pointer.
        TyKind::Tuple(_) => Some(PTR),
        // A closure value is a pointer to its heap environment.
        TyKind::Func { .. } => Some(PTR),
        // A union/dynamic value is a pointer to a `{type_id, data}` box.
        TyKind::Union(_) | TyKind::Dynamic => Some(PTR),
        // A raw FFI pointer `*T` is a machine pointer (`docs/19`).
        TyKind::Ptr(_) => Some(PTR),
        TyKind::Null | TyKind::Never => None,
        _ => None,
    }
}

/// Base for the per-instance type ids of **generic** `Drop` types (`docs/16`
/// §8). A non-generic `Drop` type uses `1000 + def.index()` for its object-
/// header drop slot, but every monomorphization of a generic `Drop` type shares
/// one `def` while needing its *own* `drop` glue — so each instance is keyed by
/// its compiled `drop` method's `FuncId` instead, offset past this base to keep
/// it disjoint from the `1000 + def` space (def indices never approach `2^40`).
/// These ids live only in the GC drop registry / object descriptors; union and
/// dynamic boxes carry their own separately-written `type_id` words, so this
/// never affects `is`/`as` matching.
pub(crate) const GENERIC_DROP_TID_BASE: i64 = 1 << 40;

/// A stable runtime type id for a (non-union) type, stored in a union/dynamic
/// box so `is`/`as` can identify the inhabited variant. Conceptually the
/// "type pointer" of `docs/16` §3, collapsed to an integer for now.
pub(crate) fn type_id(analysis: &Analysis, ty: Ty) -> i64 {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => match it {
            IntTy::I8 => 1,
            IntTy::I16 => 2,
            IntTy::I32 => 3,
            IntTy::I64 => 4,
            IntTy::U8 => 5,
            IntTy::U16 => 6,
            IntTy::U32 => 7,
            IntTy::U64 => 8,
            IntTy::Isize => 9,
            IntTy::Usize => 10,
        },
        TyKind::Float(FloatTy::F32) => 11,
        TyKind::Float(FloatTy::F64) => 12,
        TyKind::Bool => 13,
        TyKind::Char => 14,
        TyKind::Str => 15,
        TyKind::Null => 16,
        // Nominal types get ids past the primitive range, keyed by def.
        TyKind::Named { def, .. } => 1000 + def.index() as i64,
        // Tuples/functions in unions are not yet supported; -1 never matches.
        _ => -1,
    }
}

/// The `(MIN, MAX)` bit patterns (as `i64` for `iconst`) of an integer type.
pub(crate) fn int_min_max(it: IntTy) -> (i64, i64) {
    let bits = it.bits().unwrap_or(64);
    if it.is_signed() {
        if bits >= 64 {
            (i64::MIN, i64::MAX)
        } else {
            let m = 1i64 << (bits - 1);
            (-m, m - 1)
        }
    } else if bits >= 64 {
        (0, -1) // u64::MAX is all-ones (read back as i64 = -1)
    } else {
        (0, (1i64 << bits) - 1)
    }
}

pub(crate) fn int_clty(it: IntTy) -> ClType {
    match it {
        IntTy::I8 | IntTy::U8 => types::I8,
        IntTy::I16 | IntTy::U16 => types::I16,
        IntTy::I32 | IntTy::U32 => types::I32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => types::I64,
    }
}

pub(crate) fn float_clty(ft: FloatTy) -> ClType {
    match ft {
        FloatTy::F32 => types::F32,
        FloatTy::F64 => types::F64,
    }
}

/// The in-memory layout of a struct's field block: per-field byte offset and
/// Cranelift type (`None` for zero-sized `null` fields), plus the total size.
pub(crate) struct Layout {
    pub(crate) names: Vec<String>,
    pub(crate) offsets: Vec<u32>,
    pub(crate) cltys: Vec<Option<ClType>>,
    /// The (lowered) type of each field — lets field access distinguish an
    /// inline aggregate (a nested extern struct) from a scalar.
    pub(crate) tys: Vec<Ty>,
    pub(crate) size: u32,
    /// The aggregate's alignment (a power of two), for stack-slot allocation.
    pub(crate) align: u32,
    /// Byte offsets of fields that hold managed pointers (the GC trace map).
    pub(crate) ptr_offsets: Vec<u32>,
    /// Byte offsets of fields that hold `@RefCounted` strong references (a subset
    /// of `ptr_offsets`). Emitted into the descriptor trailer so the runtime
    /// releases these owned references when the object is destroyed — by
    /// `lang_rc_release` reaching zero or by a GC sweep (`docs/16` §8.1).
    pub(crate) rc_offsets: Vec<u32>,
}

impl Layout {
    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }
}

pub(crate) fn align_up(x: u32, align: u32) -> u32 {
    x.div_ceil(align) * align
}

/// Is `ty` an immutable, intrinsically-cloneable value? Primitives + `str` +
/// `null` qualify (sharing them is observationally a deep copy, `docs/15` §8).
/// Mirrors `Checker::is_immutable_value` for the codegen-side deep-clone
/// recursion.
pub(crate) fn is_immutable_value_codegen(analysis: &Analysis, ty: Ty) -> bool {
    matches!(
        analysis.tcx.kind(ty),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
    )
}

/// Whether nominal type `def` is a `@RefCounted` struct (`docs/16` §8.1): an
/// opt-in deterministic reference-counted object kind. Read off the struct's
/// decorators; checked exactly like the other layout decorators (`@Packed`, …).
pub(crate) fn is_refcounted_def(analysis: &Analysis, def: DefId) -> bool {
    let d = analysis.program.def(def);
    d.kind == DefKind::Struct && d.attrs.iter().any(|a| a.name.name == "RefCounted")
}

/// Whether `ty` resolves to a `@RefCounted` struct. `ty` should already be
/// resolved through the active substitution.
pub(crate) fn is_refcounted_ty(analysis: &Analysis, ty: Ty) -> bool {
    matches!(analysis.tcx.kind(ty), TyKind::Named { def, .. } if is_refcounted_def(analysis, *def))
}

/// Bytes a `@RefCounted` object prepends to its field block: the hidden atomic
/// strong-count word at offset 0 (user fields shift up by this much).
pub(crate) const RC_HEADER: u32 = 8;

/// Is a value of `ty` a managed-heap pointer (so the collector must trace it)?
/// Primitives are not; `str`, tuples, unions/`dynamic`, and managed structs
/// (including `List`) are. Foreign (`extern`) structs are not managed.
pub(crate) fn is_managed_ptr(analysis: &Analysis, ty: Ty) -> bool {
    match analysis.tcx.kind(ty) {
        TyKind::Str | TyKind::Tuple(_) | TyKind::Dynamic => true,
        // A `*T | null` union is laid out as a *raw* nullable pointer (NPO,
        // `docs/19` §2), not a managed `{type_id, data}` box — the collector
        // must NOT trace it (it points into foreign/unmanaged memory or is null).
        // Every other union is a managed box.
        TyKind::Union(_) => npo_union(analysis, ty).is_none(),
        // A closure value is a pointer to a managed environment.
        TyKind::Func { is_extern: false, .. } => true,
        // A `@Transparent` newtype is managed iff its inner field is.
        TyKind::Named { .. } if transparent_inner(analysis, ty).is_some() => {
            is_managed_ptr(analysis, transparent_inner(analysis, ty).unwrap())
        }
        TyKind::Named { def, .. } => {
            matches!(analysis.program.def(*def).kind, DefKind::Struct | DefKind::Interface)
        }
        _ => false,
    }
}

/// If `ty` is a null-pointer-optimized union — exactly `{ *T, null }` — return
/// its pointer variant `*T`. Such a union is represented at runtime as a single
/// raw nullable pointer (`null` == `0x0`), with no `{type_id, data}` box
/// (`docs/19` §2). Returns `None` for every other union.
pub(crate) fn npo_union(analysis: &Analysis, ty: Ty) -> Option<Ty> {
    if let TyKind::Union(members) = analysis.tcx.kind(ty) {
        if members.len() == 2 {
            let has_null = members.iter().any(|m| matches!(analysis.tcx.kind(*m), TyKind::Null));
            let ptr = members.iter().find(|m| matches!(analysis.tcx.kind(**m), TyKind::Ptr(_)));
            if has_null {
                return ptr.copied();
            }
        }
    }
    None
}

/// Compute a field-block layout from named, lowered field types. Field offsets
/// respect each field's natural alignment; the total size is rounded up to the
/// aggregate's alignment (`docs/02` §9). Records which fields are managed
/// pointers for the GC trace map.
pub(crate) fn layout_of_fields(analysis: &Analysis, fields: &[(String, Ty)]) -> Layout {
    let mut offset = 0u32;
    let mut offsets = Vec::new();
    let mut cltys = Vec::new();
    let mut names = Vec::new();
    let mut tys = Vec::new();
    let mut ptr_offsets = Vec::new();
    let mut rc_offsets = Vec::new();
    let mut max_align = 1u32;
    for (name, ty) in fields {
        let ct = clty_of(analysis, *ty);
        let (size, align) = match ct {
            Some(c) => (c.bytes(), c.bytes().max(1)),
            None => (0, 1),
        };
        offset = align_up(offset, align);
        offsets.push(offset);
        cltys.push(ct);
        names.push(name.clone());
        tys.push(*ty);
        if is_managed_ptr(analysis, *ty) {
            ptr_offsets.push(offset);
            if is_refcounted_ty(analysis, *ty) {
                rc_offsets.push(offset);
            }
        }
        offset += size;
        max_align = max_align.max(align);
    }
    Layout {
        names,
        offsets,
        cltys,
        tys,
        size: align_up(offset, max_align).max(1),
        align: max_align,
        ptr_offsets,
        rc_offsets,
    }
}

/// If `ty` is a `@Transparent` single-field struct (`docs/19` §3), return its
/// inner field type (with the struct's generic args substituted). A transparent
/// struct has the same runtime representation and ABI as its one field.
pub(crate) fn transparent_inner(analysis: &Analysis, ty: Ty) -> Option<Ty> {
    let TyKind::Named { def, args } = analysis.tcx.kind(ty) else { return None };
    let d = analysis.program.def(*def);
    if !matches!(d.kind, DefKind::Struct | DefKind::ExternStruct) {
        return None;
    }
    if !d.attrs.iter().any(|a| a.name.name == "Transparent") {
        return None;
    }
    let inner = match analysis.hir.structs.get(def)? {
        StructFields::Tuple(ts) if ts.len() == 1 => ts[0],
        StructFields::Record(fs) if fs.len() == 1 => fs[0].1,
        _ => return None,
    };
    // Substitute the struct's generic params (usually none for a newtype).
    let ssubst: HashMap<DefId, Ty> =
        d.generics.iter().copied().zip(args.iter().copied()).collect();
    Some(resolve_shallow(analysis, inner, &ssubst))
}

/// Whether `ty` is a nested `extern struct` (laid out *inline* inside another
/// extern struct, not as a pointer) — `docs/19` §3.
pub(crate) fn is_extern_struct_ty(analysis: &Analysis, ty: Ty) -> bool {
    matches!(
        analysis.tcx.kind(ty),
        TyKind::Named { def, .. } if analysis.program.def(*def).kind == DefKind::ExternStruct
    )
}

/// The C-ABI layout decorators on an `extern struct` def (`docs/19` §3),
/// read off its `attrs`.
#[derive(Default)]
pub(crate) struct ExternRepr {
    /// `@Packed(N)` — cap each field's alignment at `N` (bare `@Packed` = 1).
    pub(crate) packed: Option<u32>,
    /// `@Align(N)` — a minimum alignment for the whole struct.
    pub(crate) min_align: Option<u32>,
    /// `@Union` — all fields share offset 0; size = max field size.
    pub(crate) is_union: bool,
}

/// Read the first positive integer literal among an attribute's positional
/// arguments (e.g. the `8` in `@Packed(8)`), if any.
fn attr_int_arg(attr: &Attribute) -> Option<u32> {
    for a in &attr.args {
        if let AttrArg::Positional(e) = a {
            if let ExprKind::Int(lit) = &e.kind {
                let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Collect the FFI layout decorators declared on an `extern struct` def.
pub(crate) fn extern_repr(analysis: &Analysis, def: DefId) -> ExternRepr {
    let mut repr = ExternRepr::default();
    for attr in &analysis.program.def(def).attrs {
        match attr.name.name.as_str() {
            "Packed" => repr.packed = Some(attr_int_arg(attr).unwrap_or(1)),
            "Align" => repr.min_align = attr_int_arg(attr),
            "Union" => repr.is_union = true,
            _ => {}
        }
    }
    repr
}

/// The byte size and alignment of an extern-struct field type (`docs/19`):
/// a scalar / raw pointer (via its Cranelift type), or a fixed array
/// `[T; N]` (`N * size(T)`, aligned like `T`).
pub(crate) fn field_size_align(analysis: &Analysis, ty: Ty) -> (u32, u32) {
    match analysis.tcx.kind(ty) {
        TyKind::Array { elem, len } => {
            let (es, ea) = field_size_align(analysis, *elem);
            (es.saturating_mul(*len as u32), ea)
        }
        // A nested `extern struct` is embedded *inline*, occupying its own C
        // layout's bytes (not a pointer) — `docs/19` §3.
        TyKind::Named { def, args }
            if analysis.program.def(*def).kind == DefKind::ExternStruct =>
        {
            let l = compute_layout(analysis, *def, args);
            (l.size, l.align)
        }
        _ => match clty_of(analysis, ty) {
            Some(c) => (c.bytes(), c.bytes().max(1)),
            None => (0, 1),
        },
    }
}

/// Compute the C-ABI layout of an extern struct's flat field block (no object
/// header, no GC trace map), honoring `@Packed`/`@Align`/`@Union` (`docs/19`
/// §3). Fields are scalars, raw pointers, or fixed arrays (validated by the
/// checker), so the trace map is always empty. An array field's `clty` is
/// `None` (it is not loaded as a whole value — only indexed).
pub(crate) fn extern_layout_of_fields(
    analysis: &Analysis,
    fields: &[(String, Ty)],
    repr: &ExternRepr,
) -> Layout {
    let mut offsets = Vec::new();
    let mut cltys = Vec::new();
    let mut names = Vec::new();
    let mut tys = Vec::new();
    let mut offset = 0u32;
    let mut max_align = 1u32;
    let mut union_size = 0u32;
    for (name, fty) in fields.iter() {
        // A nested extern struct (and a fixed array) is an inline aggregate, not
        // a scalar — it has no whole-value Cranelift type (accessed by address /
        // index, never loaded as one value).
        let ct = if is_extern_struct_ty(analysis, *fty)
            || matches!(analysis.tcx.kind(*fty), TyKind::Array { .. })
        {
            None
        } else {
            clty_of(analysis, *fty)
        };
        let (size, mut align) = field_size_align(analysis, *fty);
        if let Some(p) = repr.packed {
            align = align.min(p.max(1));
        }
        let off = if repr.is_union { 0 } else { align_up(offset, align) };
        offsets.push(off);
        cltys.push(ct);
        names.push(name.clone());
        tys.push(*fty);
        if repr.is_union {
            union_size = union_size.max(size);
        } else {
            offset = off + size;
        }
        max_align = max_align.max(align);
    }
    if let Some(m) = repr.min_align {
        max_align = max_align.max(m.max(1));
    }
    let raw = if repr.is_union { union_size } else { offset };
    Layout {
        names,
        offsets,
        cltys,
        tys,
        size: align_up(raw, max_align).max(1),
        align: max_align,
        ptr_offsets: Vec::new(),
        rc_offsets: Vec::new(),
    }
}

/// The field-block layout of a (non-generic) struct, by its recorded fields.
pub(crate) fn compute_layout(analysis: &Analysis, def: DefId, args: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> = match analysis.hir.structs.get(&def) {
        Some(StructFields::Record(fs)) => fs.clone(),
        Some(StructFields::Tuple(ts)) => {
            ts.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect()
        }
        _ => Vec::new(),
    };
    // For a generic struct, the field types reference the struct's own
    // parameters; substitute the instantiation's arguments.
    let ssubst: HashMap<DefId, Ty> =
        analysis.program.def(def).generics.iter().copied().zip(args.iter().copied()).collect();
    let resolved: Vec<(String, Ty)> = fields
        .into_iter()
        .map(|(n, t)| (n, resolve_shallow(analysis, t, &ssubst)))
        .collect();
    // An `extern struct` uses the C ABI: a flat, header-less field block whose
    // offsets honor the layout decorators (`docs/19` §3).
    if analysis.program.def(def).kind == DefKind::ExternStruct {
        let repr = extern_repr(analysis, def);
        return extern_layout_of_fields(analysis, &resolved, &repr);
    }
    let mut layout = layout_of_fields(analysis, &resolved);
    if is_refcounted_def(analysis, def) {
        // A `@RefCounted` object reserves a hidden atomic strong-count word at
        // field-block offset 0; every user field shifts up by `RC_HEADER`. Field
        // access, construction, clone, pattern-matching, etc. all read offsets
        // from this `Layout`, so the shift is transparent to them — only the
        // allocator (stamps the initial count) and the descriptor (kind +
        // rc-child trailer) treat refcounted objects specially.
        for o in layout.offsets.iter_mut() {
            *o += RC_HEADER;
        }
        for o in layout.ptr_offsets.iter_mut() {
            *o += RC_HEADER;
        }
        for o in layout.rc_offsets.iter_mut() {
            *o += RC_HEADER;
        }
        layout.size += RC_HEADER;
        layout.align = layout.align.max(RC_HEADER);
    }
    layout
}

/// The layout of an anonymous tuple, positions named "0", "1", ….
pub(crate) fn tuple_layout(analysis: &Analysis, elems: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> =
        elems.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect();
    layout_of_fields(analysis, &fields)
}

/// Process the supported backslash escapes of a string-literal text run.
pub(crate) fn unescape_into(text: &str, out: &mut Vec<u8>) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('\'') => out.push(b'\''),
            Some('"') => out.push(b'"'),
            Some('$') => out.push(b'$'),
            Some('0') => out.push(0),
            Some('u') => {
                // \u{H..} — consume the brace-delimited hex.
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    while let Some(&h) = chars.peek() {
                        if h == '}' { chars.next(); break; }
                        hex.push(h);
                        chars.next();
                    }
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => {}
        }
    }
}

