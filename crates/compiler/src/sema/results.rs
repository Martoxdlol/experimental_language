//! The output of type checking that downstream phases (monomorphization, code
//! generation, the LSP) consume.
//!
//! The checker walks the AST and, rather than building a separate typed tree,
//! records side tables keyed by source [`Span`] (every AST node has a unique
//! span) and by [`DefId`]/[`LocalId`]. Codegen re-walks the same AST and looks
//! up each node's type and each name's resolution here.

use crate::ids::{DefId, LocalId};
use crate::span::Span;
use crate::ty::Ty;
use std::collections::HashMap;

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

/// Everything the checker learns about a program.
#[derive(Default)]
pub struct CheckResults {
    /// The resolved type of every expression node, by its span.
    pub expr_types: HashMap<Span, Ty>,
    /// What each value-position name resolves to, by the name's span.
    pub resolutions: HashMap<Span, ValueRes>,
    /// The type of every local binding.
    pub local_types: HashMap<LocalId, Ty>,
    /// The declaration (binding occurrence) span of every local — its `var`
    /// name, parameter name, or pattern binding. Consumed by the LSP for
    /// go-to-definition / find-references on locals.
    pub local_decls: HashMap<LocalId, Span>,
    /// Per function: its parameter locals in order, and its return type.
    pub fn_params: HashMap<DefId, Vec<LocalId>>,
    pub fn_return: HashMap<DefId, Ty>,
    /// Per `extern function`: its lowered parameter types and return type. An
    /// extern function has no body (no param locals), so codegen reads its C-ABI
    /// signature from here instead of `fn_params`/`fn_return`.
    pub extern_sigs: HashMap<DefId, (Vec<Ty>, Ty)>,
    /// Per struct def: its lowered field-type layout template.
    pub struct_fields: HashMap<DefId, StructFields>,
    /// Implicit coercions the checker inserted, keyed by the coerced
    /// expression's span (the code generator applies them after evaluating the
    /// expression).
    pub adjustments: HashMap<Span, Adjust>,
    /// The lowered target type of each `as`/`is` cast, keyed by the cast
    /// expression's span (needed for `is`, whose result type is `bool`).
    pub cast_targets: HashMap<Span, Ty>,
    /// For type-matching patterns (`i64 x`, unit-struct names), the lowered
    /// variant type the pattern tests for, keyed by the pattern's span.
    pub pattern_types: HashMap<Span, Ty>,
    /// The generic type arguments at each generic call/constructor site, keyed
    /// by the callee's span. These are expressed in the *caller's* type
    /// parameters (so they may contain `Param`s); the monomorphizer substitutes
    /// the caller's instantiation to get concrete arguments.
    pub call_type_args: HashMap<Span, Vec<Ty>>,
    /// For an overloaded operator (one whose operands are user types), the
    /// `extend` method it resolves to, keyed by the operator's span. Codegen
    /// emits a method call instead of a primitive instruction.
    pub operator_methods: HashMap<Span, DefId>,
    /// For a string-interpolation hole of a user type, the `to_str(self): str`
    /// method to call, keyed by the hole expression's span (`docs/01` §8).
    pub stringify_methods: HashMap<Span, DefId>,
    /// `?` residual conversions (`docs/13` §4): for each `?` (keyed by its
    /// span), the failure variants that propagate via a `FromResidual` impl
    /// rather than directly — `(residual variant, from_residual method, target
    /// type)`. On a match the value is unboxed, converted, and re-boxed.
    pub residual_conversions: HashMap<Span, Vec<(Ty, DefId, Ty)>>,
    /// `?` on a non-union wrapper type (`docs/13` §3): for each such `?`
    /// (keyed by the `?` span), the `branch` method to call on the operand
    /// to obtain its `Output | Residual` union, the monomorphization arguments
    /// (the wrapper extend's solved generics), and the resulting union type.
    /// Codegen emits the branch call before the existing union partition.
    pub try_branches: HashMap<Span, TryBranch>,
    /// Builtin collection constructor calls (`Map<K,V>()`, `List<T>()`, and
    /// their `.new` forms), keyed by the call expression's span. The value is
    /// the lowered collection type to allocate (empty).
    pub builtin_ctors: HashMap<Span, Ty>,
    /// `for` loops driven by the `Iterator` protocol (rather than the `List`
    /// fast path), keyed by the iterable expression's span.
    pub for_iters: HashMap<Span, ForIter>,
    /// `for entry in map` loops, keyed by the iterable expression's span. The
    /// values are `(key type, value type, Entry<K,V> type)`.
    pub for_maps: HashMap<Span, (Ty, Ty, Ty)>,
    /// Interface implementations: `(implementing type def, interface def) →
    /// extend block def`. Lets codegen monomorphize an interface-method call on
    /// a generic type parameter to the concrete `extend` impl.
    pub iface_impls: HashMap<(DefId, DefId), DefId>,
    /// Closure expressions, keyed by the closure's span. Carries the analysis
    /// codegen needs to lower it to a heap environment + lifted function.
    pub closures: HashMap<Span, ClosureInfo>,
    /// Builtin `.clone()` calls (`docs/10`/`docs/15`), keyed by the call's
    /// `callee` span. User/derived `clone` methods go through normal method
    /// resolution instead and are *not* recorded here.
    pub clone_kinds: HashMap<Span, CloneKind>,
    /// Static method calls `Type.method(...)` / `T.method(...)` (`docs/09` §6,
    /// `docs/10`), keyed by the call's `callee` span. The resolution table holds
    /// the (interface or extend) method; this set tells codegen the call takes
    /// **no receiver** (do not prepend `self`).
    pub static_calls: std::collections::HashSet<Span>,
    /// For each static call, the receiver type it was made on (a `Param` for
    /// `T.method()`, a `Named` for `Type.method()`), so codegen can resolve an
    /// interface static method to the concrete impl after substitution.
    pub static_recv: HashMap<Span, Ty>,
    /// Numeric-namespace intrinsics (`docs/18` §10, `docs/14` §5): `i32.MAX`,
    /// `i32.wrapping_add(a,b)`, `f64.NAN`, `f64.is_nan(x)`, … keyed by the field
    /// or call expression's span. Codegen emits the constant/operation directly.
    pub num_intrinsics: HashMap<Span, NumIntrinsic>,
    /// `Shared.new(v)` calls (`docs/20` §4), keyed by the call span. Codegen
    /// allocates a runtime mutex cell and wraps it in a `Shared<T>` handle.
    pub shared_news: std::collections::HashSet<Span>,
    /// `Foreign.alloc<T>()` / `alloc_zeroed<T>()` calls (`docs/19` §5), keyed by
    /// the call span → (`T`, `zeroed`). Codegen emits a `lang_foreign_alloc`
    /// of `sizeof(T)` bytes; the result is a raw `*T | null` (NPO).
    pub foreign_allocs: HashMap<Span, (Ty, bool)>,
    /// `Foreign.free(p)` calls (`docs/19` §5), keyed by the call span. Codegen
    /// emits `lang_foreign_free` on the raw pointer argument.
    pub foreign_frees: std::collections::HashSet<Span>,
    /// `Foreign.realloc<T>(p, new_size)` calls (`docs/19` §5). Codegen emits
    /// `lang_foreign_realloc(p, new_size)`.
    pub foreign_reallocs: std::collections::HashSet<Span>,
    /// `Foreign.alloc_flex<T, E>(extra_count)` calls (`docs/19` §5), keyed by
    /// span → (`T`, `E`). Codegen allocates `sizeof(T) + extra_count*sizeof(E)`.
    pub foreign_flex: HashMap<Span, (Ty, Ty)>,
    /// `CString.from_str(s)` calls (`docs/19` §6): marshal a `str` into a
    /// NUL-terminated C string on the foreign heap (returns `*u8`).
    pub cstring_from_strs: std::collections::HashSet<Span>,
    /// `CStr.to_str(p)` calls (`docs/19` §6): copy a C string into a `str`.
    pub cstr_to_strs: std::collections::HashSet<Span>,
    /// Libraries named by `@Link(lib = "…")` on extern functions (`docs/19`
    /// §13), in first-seen order. Native builds pass `-l<lib>`; the JIT
    /// `dlopen`s each so the symbols resolve.
    pub link_libs: Vec<String>,
    /// `channel<T>()` calls (`docs/20` §2), keyed by the call span. Codegen
    /// allocates a runtime channel and builds the `(Sender<T>, Receiver<T>)`
    /// tuple; the element type is read from the recorded result type.
    pub channel_news: std::collections::HashSet<Span>,
    /// `Thread.spawn { … }` calls (`docs/20`), keyed by the call's span. The
    /// value is the closure's result type `R` (so codegen builds `JoinHandle<R>`
    /// and, at `join`, `Joined<R>`).
    pub thread_spawns: HashMap<Span, Ty>,
    /// `JoinHandle<R>.join()` calls (`docs/20`), keyed by the call's span. The
    /// value is `R` (codegen builds the `Joined<R> | Panicked` result).
    pub thread_joins: HashMap<Span, Ty>,
    /// Async functions/methods (`docs/21` §3): the future `Output` type, keyed
    /// by the function def. Such a function's body is lowered to a `Future`
    /// state machine whose `poll` runs the body; calling it constructs the
    /// machine instead of running the body.
    pub async_fns: HashMap<DefId, Ty>,
    /// `await e` expressions (`docs/21` §4), keyed by the `await` keyword span.
    /// The value is the awaited future's `Output` (the type `await` yields).
    pub awaits: HashMap<Span, Ty>,
    /// `yield_now()` calls (`docs/21`): a builtin returning a `Future<null>`
    /// that suspends once. Keyed by the call span.
    pub yield_nows: std::collections::HashSet<Span>,
    /// `spawn EXPR` expressions (`docs/21` §6), keyed by the `spawn` keyword
    /// span. The value is the inner future's `Output` `T`. `spawn EXPR`
    /// evaluates to a `Future<T>` whose poll either delivers the result of the
    /// scheduled task or registers a waker.
    pub async_spawns: HashMap<Span, Ty>,
    /// `sleep(ms)` calls (`docs/21`): a `Future<null>` completing after a delay.
    pub async_sleeps: std::collections::HashSet<Span>,
    /// `fut.cancel()` calls (`docs/21` §8), keyed by the call's callee span — a
    /// no-op for the compute-only futures we generate (no I/O to release).
    pub future_cancels: std::collections::HashSet<Span>,
    /// `for await x in stream` loops (`docs/21` §10), keyed by the iterable
    /// expression's span. Carries the `AsyncIterator` protocol resolution.
    pub for_async_iters: HashMap<Span, ForAsyncIter>,
    /// Bare `async { … }` blocks and `async` closures (`docs/21` §6–7), keyed by
    /// the expression's span. Carries the future `Output`, captured locals, and
    /// (for closures) parameters — everything codegen needs to lower the block
    /// to a `Future` state machine over a captured environment.
    pub async_blocks: HashMap<Span, AsyncInfo>,
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

impl CheckResults {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn expr_ty(&self, span: Span) -> Option<Ty> {
        self.expr_types.get(&span).copied()
    }

    pub fn resolution(&self, span: Span) -> Option<ValueRes> {
        self.resolutions.get(&span).copied()
    }

    pub fn local_ty(&self, id: LocalId) -> Option<Ty> {
        self.local_types.get(&id).copied()
    }

    pub fn adjustment(&self, span: Span) -> Option<Adjust> {
        self.adjustments.get(&span).copied()
    }

    pub fn type_args(&self, span: Span) -> Option<&[Ty]> {
        self.call_type_args.get(&span).map(|v| v.as_slice())
    }
}
