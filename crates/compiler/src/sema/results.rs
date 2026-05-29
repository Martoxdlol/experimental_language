//! Shared semantic vocabulary the checker bakes onto HIR nodes and that
//! downstream phases (monomorphization, code generation, the LSP) consume:
//! value resolutions, type adjustments, builtin/intrinsic descriptors, and the
//! codegen-shape records (`TryBranch`, `ForIter`, `CloneKind`, …).
//!
//! The checker no longer keeps span-keyed side tables — it emits a fully typed
//! [`crate::hir::Hir`] directly. These types are the leaf data those HIR nodes
//! carry.

use crate::ids::{DefId, LocalId};
use crate::ty::Ty;

/// What a value-position name (an identifier or call target) resolves to.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ValueRes {
    /// A parameter or `var` binding, by its unique id.
    Local(LocalId),
    /// A free or extern function — referenced as a function value / call target.
    Function(DefId),
    /// An inherent/interface method resolved at a `recv.m(..)` call site; the
    /// `DefId` is the `extend` method definition.
    Method(DefId),
    /// A module-level `var` or `extern var`.
    Global(DefId),
    /// A unit/tuple struct used as a value constructor (the bare name).
    StructCtor(DefId),
    /// A compiler-provided builtin function (temporary prelude until the real
    /// `std:io`/`core:prelude` modules are wired in).
    Builtin(Builtin),
}

/// Compiler-provided builtin functions available without import (for now).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Builtin {
    /// `print(str): null` — write to stdout, no newline.
    Print,
    /// `println(str): null` — write to stdout with a trailing newline.
    Println,
    /// `panic(str): never` — terminate the thread with a message (`docs/14`).
    Panic,
    /// `panic_with(value: dynamic): never` — terminate the thread, attaching a
    /// structured value the language never inspects (`docs/14` §1).
    PanicWith,
    /// `exit(i32): never` — terminate the process with a code (`docs/24`).
    Exit,
    /// `abort(): never` — terminate the process immediately (`docs/24`).
    Abort,
}

impl Builtin {
    pub fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "print" => Some(Builtin::Print),
            "println" => Some(Builtin::Println),
            "panic" => Some(Builtin::Panic),
            "panic_with" => Some(Builtin::PanicWith),
            "exit" => Some(Builtin::Exit),
            "abort" => Some(Builtin::Abort),
            _ => None,
        }
    }
}

/// An implicit coercion applied to an expression's value at runtime.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Adjust {
    /// Box the value into a union/`dynamic` of the given type (widening).
    Widen(Ty),
    /// Unbox a union/`dynamic` value to the given concrete variant type
    /// (flow narrowing — the variant is already known, no tag check).
    Unbox(Ty),
    /// Wrap a concrete value into an interface object (a fat pointer of
    /// `{vtable, data}`) for the given interface type (dynamic dispatch).
    WidenDyn(Ty),
}

/// The lowered field types of a struct, as a layout template. For generic
/// structs the types may contain `Param`s; codegen substitutes per instance.
#[derive(Clone, Debug)]
pub enum StructFields {
    Unit,
    Tuple(Vec<Ty>),
    Record(Vec<(String, Ty)>),
}

/// What codegen needs to lower a bare `async { … }` block or an `async`
/// closure to a `Future` state machine (`docs/21` §6–7).
#[derive(Clone, Debug)]
pub struct AsyncInfo {
    /// The future's `Output` type (the block's trailing-expression type).
    pub output: Ty,
    /// Parameter locals (empty for a bare `async {}` block; populated for an
    /// `async` closure), in order, with their types.
    pub params: Vec<(LocalId, Ty)>,
    /// Captured enclosing locals, in order, with their types.
    pub captures: Vec<(LocalId, Ty)>,
}

/// A numeric-namespace intrinsic on a primitive type (`docs/18` §10, `docs/14`
/// §5): `i32.MIN`, `f64.is_nan(x)`, `i32.wrapping_add(a, b)`, … The embedded
/// `Ty` is the primitive operand/result type.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum NumIntrinsic {
    /// `T.MIN` (`max == false`) / `T.MAX` for an integer type.
    IntBound { ty: Ty, max: bool },
    /// `f*.INFINITY` (0), `f*.NEG_INFINITY` (1), `f*.NAN` (2).
    FloatConst { ty: Ty, kind: u8 },
    /// `f*.is_nan` (0) / `is_infinite` (1) / `is_finite` (2) — one float arg.
    FloatPred { ty: Ty, kind: u8 },
    /// `T.{wrapping,saturating,checked,overflowing}_{add,sub,mul,div,rem,neg,shl,shr}(args)`.
    /// `family`: 0 wrapping, 1 saturating, 2 checked, 3 overflowing.
    /// `op`: 0 add, 1 sub, 2 mul, 3 div, 4 rem, 5 neg, 6 shl, 7 shr.
    /// Arities: add/sub/mul/div/rem take `(T, T)`; neg takes `(T)`;
    /// shl/shr take `(T, u32)`. Result by family: 0/1 → `T`, 2 → `T | null`,
    /// 3 → `(T, bool)`.
    IntArith { ty: Ty, family: u8, op: u8 },
}

/// The numeric-namespace *constant* named by `Type.NAME` (`i32.MIN`, `f64.NAN`,
/// …), or `None` if `tyname`/`name` is not such a constant. Shared by the type
/// checker (for typing) and HIR lowering (to build the `Intrinsic::Num` node),
/// so the recognition lives in exactly one place. The operand/result type is
/// embedded in the returned [`NumIntrinsic`].
pub fn num_constant_of(tcx: &crate::ty::TyCtxt, tyname: &str, name: &str) -> Option<NumIntrinsic> {
    use crate::ty::{FloatTy, IntTy};
    if let Some(it) = IntTy::from_name(tyname) {
        let ty = tcx.int(it);
        return match name {
            "MIN" => Some(NumIntrinsic::IntBound { ty, max: false }),
            "MAX" => Some(NumIntrinsic::IntBound { ty, max: true }),
            _ => None,
        };
    }
    if let Some(ft) = FloatTy::from_name(tyname) {
        let ty = tcx.float(ft);
        let kind = match name {
            "INFINITY" => 0u8,
            "NEG_INFINITY" => 1,
            "NAN" => 2,
            _ => return None,
        };
        return Some(NumIntrinsic::FloatConst { ty, kind });
    }
    None
}

/// The numeric-namespace *method* named by `Type.name(..)` — the float
/// predicates (`is_nan`/`is_infinite`/`is_finite`) and the integer
/// `{wrapping,saturating,checked,overflowing}_{add,sub,mul,div,rem,neg,shl,shr}`
/// families — or `None`. Shared by the checker and HIR lowering (see
/// [`num_constant_of`]). The operand type is embedded in the result.
pub fn num_method_of(tcx: &crate::ty::TyCtxt, tyname: &str, name: &str) -> Option<NumIntrinsic> {
    use crate::ty::{FloatTy, IntTy};
    if let Some(ft) = FloatTy::from_name(tyname) {
        let ty = tcx.float(ft);
        let kind = match name {
            "is_nan" => 0u8,
            "is_infinite" => 1,
            "is_finite" => 2,
            _ => return None,
        };
        return Some(NumIntrinsic::FloatPred { ty, kind });
    }
    let it = IntTy::from_name(tyname)?;
    let ty = tcx.int(it);
    let (family, base) = if let Some(b) = name.strip_prefix("wrapping_") {
        (0u8, b)
    } else if let Some(b) = name.strip_prefix("saturating_") {
        (1, b)
    } else if let Some(b) = name.strip_prefix("checked_") {
        (2, b)
    } else if let Some(b) = name.strip_prefix("overflowing_") {
        (3, b)
    } else {
        return None;
    };
    let op = match base {
        "add" => 0u8,
        "sub" => 1,
        "mul" => 2,
        "div" => 3,
        "rem" => 4,
        "neg" => 5,
        "shl" => 6,
        "shr" => 7,
        _ => return None,
    };
    Some(NumIntrinsic::IntArith { ty, family, op })
}

/// How codegen should clone a builtin-typed receiver (`docs/15` §8).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum CloneKind {
    /// `List` of a mutable element type whose elements implement `Clone`:
    /// codegen emits an element-by-element deep clone (`docs/10`).
    ListDeep,
    /// `Map` whose value type is mutable but implements `Clone` (the key
    /// type stays immutable since rehashing would change the layout).
    MapDeep,
    /// Immutable scalars and `str`: a clone is the same value (sharing an
    /// immutable value is observationally identical to a deep copy).
    Identity,
    /// `List<T>` of an immutable element type: copy the backing buffer into a
    /// fresh list (elements are shared, which is sound because they're
    /// immutable).
    List,
    /// `Map<K, V>` of immutable key/value types: copy into a fresh map.
    Map,
}

/// What codegen needs to lower a closure: its parameters, the enclosing locals
/// it captures (by value), and its return type.
#[derive(Clone, Debug)]
pub struct ClosureInfo {
    /// Parameter locals, in order, with their types.
    pub params: Vec<(LocalId, Ty)>,
    /// Captured enclosing locals, in order, with their types. Stored in the
    /// closure's heap environment after the function pointer.
    pub captures: Vec<(LocalId, Ty)>,
    /// The closure's return type.
    pub ret: Ty,
}

/// How codegen should drive a `for await x in stream` loop (`docs/21` §10): the
/// async-iterator protocol resolution. Each step `await`s `next_async()` and
/// matches `Item<T>` / `Done`.
#[derive(Clone, Debug)]
pub struct ForAsyncIter {
    /// The element type `T` (`Item<T>`'s argument).
    pub elem: Ty,
    /// The resolved `next_async(self): Future<Item<T> | Done>` method.
    pub next_async: DefId,
    /// Type arguments to monomorphize `next_async` (enclosing `extend` generics).
    pub next_targs: Vec<Ty>,
    /// The concrete stream type.
    pub iter_ty: Ty,
    /// The `Item<T>` variant type (its type id tags the `Item` arm).
    pub item_ty: Ty,
    /// The `Done` variant type (its type id ends iteration).
    pub done_ty: Ty,
    /// The `Item<T> | Done` union (the awaited `Output`).
    pub union_ty: Ty,
}

/// Lowering info for `?` on a wrapper type that implements
/// `Try<Output, Residual>` (`docs/13` §3). Codegen calls `branch` to get the
/// `Output | Residual` union; the checker has pre-split it into the success
/// variants (`output`) and failure variants (`residual`).
#[derive(Clone, Debug)]
pub struct TryBranch {
    /// The `branch(self): Output | Residual` method to invoke.
    pub method: DefId,
    /// The monomorphization arguments for `method` (the wrapper extend's
    /// solved generics), in declaration order.
    pub targs: Vec<Ty>,
    /// The `Output | Residual` union returned by `branch` — the runtime tag
    /// dispatch happens on this.
    pub union_ty: Ty,
    /// The `Output` type from the `Try<Output, Residual>` impl. Variants of
    /// this are the *success* path of `?` (unboxed as the expression's value).
    pub output: Ty,
    /// The `Residual` type. Its variants are the *failure* path — early-
    /// returned directly when in `R`, or via `FromResidual` otherwise.
    pub residual: Ty,
}

/// How codegen should drive an `Iterator`-protocol `for` loop.
#[derive(Clone, Debug)]
pub struct ForIter {
    /// The element type `U` yielded (the `T` of `Item<T>`).
    pub elem: Ty,
    /// The resolved `next(self): Item<U> | Done` method.
    pub next: DefId,
    /// The type arguments to monomorphize `next` with (the enclosing `extend`'s
    /// generics; empty for a concrete iterator).
    pub next_targs: Vec<Ty>,
    /// The concrete iterator type (the loop variable's receiver type).
    pub iter_ty: Ty,
    /// The `Done` variant type (for the loop's end-of-iteration tag check).
    pub done_ty: Ty,
    /// The `Item<U>` variant type (the boxed payload to unwrap each step).
    pub item_ty: Ty,
}

