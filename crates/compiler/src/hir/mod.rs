//! The **High-level Intermediate Representation** — the typed, resolved,
//! desugared tree that the type checker produces and that code generation and
//! the LSP consume.
//!
//! # Why a typed tree, not span side-tables
//!
//! Historically the checker walked the AST and recorded everything it learned
//! in ~40 [`crate::sema::results::CheckResults`] hashmaps keyed by source
//! [`Span`]; downstream phases re-walked the *same AST* and looked each fact
//! back up by span. That contract was implicit (a span typo silently produced
//! wrong code) and fragile (two nodes sharing a span collided).
//!
//! The HIR makes the contract a typed Rust structure the compiler verifies:
//!
//! - every [`Expr`] carries its resolved [`Ty`] inline (was `expr_types`);
//! - every name carries its resolution as a [`Res`] (was `resolutions`);
//! - every call carries its dispatch kind as an explicit [`CallKind`]
//!   (`Direct` / `Method` / `Builtin` / `Closure` / `Extern`) plus its solved
//!   generic arguments (was `resolutions` + `call_type_args` + `static_calls` +
//!   `static_recv`);
//! - coercions are explicit [`ExprKind::Adjust`] wrapper nodes (was
//!   `adjustments`);
//! - builtins/foreign/numeric/collection/concurrency operations are explicit
//!   [`ExprKind::Intrinsic`] variants (was the dozen `num_intrinsics`,
//!   `foreign_*`, `*_news`, `clone_kinds`, … sets);
//! - desugaring information (`?` residuals, `for` iterator protocol, operator
//!   overloading, string-interpolation `to_str`, async/await output types)
//!   lives on the relevant node.
//!
//! # Provenance
//!
//! Every node — [`Expr`], [`Stmt`], [`Pattern`], [`Block`] — preserves its
//! source [`Span`], so any value, diagnostic, or lowering traces back to the
//! exact bytes it came from. This invariant holds through every lowering and is
//! exercised by the HIR tests.
//!
//! # Relationship to `CheckResults` during the migration
//!
//! The payload structs the HIR embeds ([`Builtin`], [`NumIntrinsic`],
//! [`CloneKind`], [`Adjust`], [`ForIter`], [`ForAsyncIter`], [`TryBranch`]) are
//! currently shared with [`crate::sema::results`]; they migrate fully into this
//! module in the final stage when the span-keyed tables are deleted. The
//! *span-keyed* tables are what the HIR retires; the genuinely def-keyed
//! program facts (struct layouts, extern signatures, interface impls, link
//! libraries) live here as program-level maps, which is their natural home.

use crate::ids::{DefId, LocalId};
use crate::span::Span;
use crate::ty::Ty;
use std::collections::HashMap;

// Payload structs reused from the checker's side tables during the migration.
// (These definitions move into `hir` wholesale in Stage 5.)
pub use crate::sema::results::{
    Adjust, Builtin, CloneKind, ForAsyncIter, ForIter, NumIntrinsic, StructFields, TryBranch,
    ValueRes,
};

// ===========================================================================
// Program container
// ===========================================================================

/// The whole program in HIR form: per-definition bodies and signatures plus the
/// genuinely def-keyed program facts. This is what codegen and the LSP consume
/// instead of `(ast, CheckResults)`.
#[derive(Clone, Debug, Default)]
pub struct Hir {
    /// The lowered body of every function / `extend` method that has one, by its
    /// [`DefId`]. Bodyless defs (extern functions, abstract interface methods)
    /// have no entry here — only a [`Hir::fn_sigs`] / [`Hir::extern_sigs`] one.
    pub bodies: HashMap<DefId, Body>,
    /// The signature of every free function and `extend` method, including those
    /// without a body, so signature building never needs the AST.
    pub fn_sigs: HashMap<DefId, FnSig>,
    /// Each `extern function`'s C-ABI signature (was `extern_sigs`).
    pub extern_sigs: HashMap<DefId, ExternSig>,
    /// Each struct's lowered field-type layout template (was `struct_fields`).
    pub structs: HashMap<DefId, StructFields>,
    /// `(implementing type def, interface def) → extend block def` — lets
    /// codegen monomorphize an interface-method call to a concrete impl (was
    /// `iface_impls`).
    pub iface_impls: HashMap<(DefId, DefId), DefId>,
    /// Libraries named by `@Link(lib = "…")`, first-seen order (was `link_libs`).
    pub link_libs: Vec<String>,
    /// The declaration (binding occurrence) span of every local — its `var`
    /// name, parameter name, or pattern binding (was `local_decls`). Consumed by
    /// the LSP for go-to-definition / find-references on locals.
    pub local_decls: HashMap<LocalId, Span>,
    /// The type of every local in the program, by its (globally-unique)
    /// [`LocalId`] (was `CheckResults::local_types`). A program-wide map the
    /// backend reads directly; each [`Body::locals`] is the per-body slice of it.
    pub local_types: HashMap<LocalId, Ty>,
}

impl Hir {
    /// The type of local `id` anywhere in the program.
    pub fn local_ty(&self, id: LocalId) -> Option<Ty> {
        self.local_types.get(&id).copied()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// The body of a function/method definition, if it has one.
    pub fn body(&self, def: DefId) -> Option<&Body> {
        self.bodies.get(&def)
    }

    /// The signature of a function/method definition.
    pub fn sig(&self, def: DefId) -> Option<&FnSig> {
        self.fn_sigs.get(&def)
    }

    /// Every local captured by some closure or `async` block anywhere in the
    /// program. Codegen cell-backs these wherever they are bound (`docs/09` §7).
    /// Walks the HIR bodies' closure/async-block nodes (was derived from the
    /// `closures` / `async_blocks` side tables).
    pub fn captured_locals(&self) -> std::collections::HashSet<LocalId> {
        let mut set = std::collections::HashSet::new();
        for body in self.bodies.values() {
            collect_captures_block(&body.block, &mut set);
        }
        set
    }

    /// The checked type of the expression whose source span is `span`, found by
    /// scanning the HIR bodies for a node at that span. A baked `Adjust` carries
    /// the post-coercion type on its wrapper, so its inner (pre-coercion) type is
    /// returned to match what the checker recorded for the original node.
    pub fn expr_ty(&self, span: crate::span::Span) -> Option<Ty> {
        self.find_expr(span).map(|e| match &e.kind {
            ExprKind::Adjust { expr, .. } => expr.ty,
            _ => e.ty,
        })
    }

    /// What the value-position name at `span` resolves to, read off the `Name`
    /// HIR node there (unwrapping a baked `Adjust` wrapper).
    pub fn resolution(&self, span: crate::span::Span) -> Option<crate::sema::results::ValueRes> {
        match &self.find_expr(span)?.kind {
            ExprKind::Name(res) => Some(*res),
            ExprKind::Adjust { expr, .. } => match &expr.kind {
                ExprKind::Name(res) => Some(*res),
                _ => None,
            },
            _ => None,
        }
    }

    /// Find the HIR expression node whose span exactly matches `span`, scanning
    /// every function body. Returns the first match in body iteration order.
    pub fn find_expr(&self, span: crate::span::Span) -> Option<&Expr> {
        for body in self.bodies.values() {
            if let Some(e) = find_expr_block(&body.block, span) {
                return Some(e);
            }
        }
        None
    }
}

fn find_expr_block(b: &Block, span: crate::span::Span) -> Option<&Expr> {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                if let Some(e) = find_expr_pattern(pattern, span) {
                    return Some(e);
                }
                if let Some(e) = find_expr(init, span) {
                    return Some(e);
                }
            }
            StmtKind::Assign { target, value } => {
                if let Some(e) = find_expr(target, span) {
                    return Some(e);
                }
                if let Some(e) = find_expr(value, span) {
                    return Some(e);
                }
            }
            StmtKind::Expr(e) => {
                if let Some(found) = find_expr(e, span) {
                    return Some(found);
                }
            }
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &b.trailing {
        if let Some(e) = find_expr(t, span) {
            return Some(e);
        }
    }
    None
}

fn find_expr_pattern(p: &Pattern, span: crate::span::Span) -> Option<&Expr> {
    match &p.kind {
        PatternKind::Literal(e) => find_expr(e, span),
        PatternKind::TupleStruct { fields, .. } => {
            fields.iter().find_map(|f| find_expr_pattern(f, span))
        }
        PatternKind::RecordStruct { fields, .. } => {
            fields.iter().find_map(|f| find_expr_pattern(&f.pattern, span))
        }
        PatternKind::Tuple { elems, .. } | PatternKind::List { elems, .. } => {
            elems.iter().find_map(|e| find_expr_pattern(e, span))
        }
        PatternKind::Or(ps) => ps.iter().find_map(|p| find_expr_pattern(p, span)),
        _ => None,
    }
}

fn find_expr(e: &Expr, span: crate::span::Span) -> Option<&Expr> {
    if e.span == span {
        return Some(e);
    }
    use ExprKind as K;
    match &e.kind {
        K::Tuple(xs) | K::List(xs) => xs.iter().find_map(|x| find_expr(x, span)),
        K::Unary { operand, .. } => find_expr(operand, span),
        K::Binary { left, right, .. } => {
            find_expr(left, span).or_else(|| find_expr(right, span))
        }
        K::Cast { expr, .. }
        | K::Ref(expr)
        | K::Deref(expr)
        | K::Adjust { expr, .. }
        | K::Try { expr, .. }
        | K::Await { expr, .. }
        | K::Spawn { expr, .. }
        | K::Field { receiver: expr, .. }
        | K::TupleIndex { receiver: expr, .. } => find_expr(expr, span),
        K::Index { receiver, index } => {
            find_expr(receiver, span).or_else(|| find_expr(index, span))
        }
        K::Return(v) | K::Break(v) => v.as_deref().and_then(|x| find_expr(x, span)),
        K::Call { args, kind, .. } => {
            if let CallKind::Closure { callee } = kind {
                if let Some(found) = find_expr(callee, span) {
                    return Some(found);
                }
            }
            args.iter().find_map(|a| find_expr(a, span))
        }
        K::Intrinsic { args, .. } => args.iter().find_map(|a| find_expr(a, span)),
        K::Struct { fields, spread, .. } => fields
            .iter()
            .find_map(|f| find_expr(&f.value, span))
            .or_else(|| spread.as_deref().and_then(|x| find_expr(x, span))),
        K::Str(parts) => parts.iter().find_map(|p| match p {
            StrPart::Interp { expr, .. } => find_expr(expr, span),
            _ => None,
        }),
        K::Map(items) => items.iter().find_map(|it| match it {
            MapEntry::Kv { key, value } => {
                find_expr(key, span).or_else(|| find_expr(value, span))
            }
            MapEntry::Spread(e) => find_expr(e, span),
        }),
        K::If { cond, then_block, else_branch } => find_expr(cond, span)
            .or_else(|| find_expr_block(then_block, span))
            .or_else(|| else_branch.as_deref().and_then(|x| find_expr(x, span))),
        K::Match { scrutinee, arms } => find_expr(scrutinee, span).or_else(|| {
            arms.iter().find_map(|a| {
                find_expr_pattern(&a.pattern, span)
                    .or_else(|| a.guard.as_ref().and_then(|x| find_expr(x, span)))
                    .or_else(|| find_expr(&a.body, span))
            })
        }),
        K::Block(b) | K::Loop(b) => find_expr_block(b, span),
        K::While { cond, body } => {
            find_expr(cond, span).or_else(|| find_expr_block(body, span))
        }
        K::For { pattern, iter, body, .. } => find_expr_pattern(pattern, span)
            .or_else(|| find_expr(iter, span))
            .or_else(|| find_expr_block(body, span)),
        K::Closure { body, .. } => find_expr(body, span),
        K::AsyncBlock { body, .. } => find_expr_block(body, span),
        K::Int(_) | K::Float(_) | K::Bool(_) | K::Null | K::Char(_) | K::Name(_)
        | K::Discard | K::Continue | K::Error => None,
    }
}

fn collect_captures_block(b: &Block, out: &mut std::collections::HashSet<LocalId>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { init, .. } => collect_captures_expr(init, out),
            StmtKind::Assign { target, value } => {
                collect_captures_expr(target, out);
                collect_captures_expr(value, out);
            }
            StmtKind::Expr(e) => collect_captures_expr(e, out),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &b.trailing {
        collect_captures_expr(t, out);
    }
}

fn collect_captures_expr(e: &Expr, out: &mut std::collections::HashSet<LocalId>) {
    use ExprKind as K;
    match &e.kind {
        K::Closure { captures, body, .. } => {
            for (id, _) in captures {
                out.insert(*id);
            }
            collect_captures_expr(body, out);
        }
        K::AsyncBlock { captures, body, .. } => {
            for (id, _) in captures {
                out.insert(*id);
            }
            collect_captures_block(body, out);
        }
        K::Tuple(xs) | K::List(xs) => xs.iter().for_each(|x| collect_captures_expr(x, out)),
        K::Unary { operand, .. } => collect_captures_expr(operand, out),
        K::Binary { left, right, .. } => {
            collect_captures_expr(left, out);
            collect_captures_expr(right, out);
        }
        K::Cast { expr, .. }
        | K::Ref(expr)
        | K::Deref(expr)
        | K::Adjust { expr, .. }
        | K::Try { expr, .. }
        | K::Await { expr, .. }
        | K::Spawn { expr, .. }
        | K::Field { receiver: expr, .. }
        | K::TupleIndex { receiver: expr, .. } => collect_captures_expr(expr, out),
        K::Index { receiver, index } => {
            collect_captures_expr(receiver, out);
            collect_captures_expr(index, out);
        }
        K::Return(v) | K::Break(v) => {
            if let Some(e) = v {
                collect_captures_expr(e, out);
            }
        }
        K::Call { args, kind, .. } => {
            if let CallKind::Closure { callee } = kind {
                collect_captures_expr(callee, out);
            }
            args.iter().for_each(|a| collect_captures_expr(a, out));
        }
        K::Intrinsic { args, .. } => args.iter().for_each(|a| collect_captures_expr(a, out)),
        K::Struct { fields, spread, .. } => {
            fields.iter().for_each(|f| collect_captures_expr(&f.value, out));
            if let Some(s) = spread {
                collect_captures_expr(s, out);
            }
        }
        K::Str(parts) => parts.iter().for_each(|p| {
            if let StrPart::Interp { expr, .. } = p {
                collect_captures_expr(expr, out);
            }
        }),
        K::Map(items) => items.iter().for_each(|it| match it {
            MapEntry::Kv { key, value } => {
                collect_captures_expr(key, out);
                collect_captures_expr(value, out);
            }
            MapEntry::Spread(e) => collect_captures_expr(e, out),
        }),
        K::If { cond, then_block, else_branch } => {
            collect_captures_expr(cond, out);
            collect_captures_block(then_block, out);
            if let Some(e) = else_branch {
                collect_captures_expr(e, out);
            }
        }
        K::Match { scrutinee, arms } => {
            collect_captures_expr(scrutinee, out);
            for a in arms {
                if let Some(g) = &a.guard {
                    collect_captures_expr(g, out);
                }
                collect_captures_expr(&a.body, out);
            }
        }
        K::Block(b) | K::Loop(b) => collect_captures_block(b, out),
        K::While { cond, body } => {
            collect_captures_expr(cond, out);
            collect_captures_block(body, out);
        }
        K::For { iter, body, .. } => {
            collect_captures_expr(iter, out);
            collect_captures_block(body, out);
        }
        // Leaves with no sub-expressions.
        K::Int(_) | K::Float(_) | K::Bool(_) | K::Null | K::Char(_) | K::Name(_)
        | K::Discard | K::Continue | K::Error => {}
    }
}

/// A function / `extend` method signature in resolved form.
#[derive(Clone, Debug)]
pub struct FnSig {
    /// Parameter locals, in declaration order, with their resolved types.
    /// Includes the synthetic `self` local for methods.
    pub params: Vec<(LocalId, Ty)>,
    /// The declared (or inferred) return type.
    pub ret: Ty,
    /// `Some(output)` when this is an `async function` — its public symbol
    /// constructs a `Future<output>` state machine rather than running the body
    /// (was `async_fns`).
    pub async_output: Option<Ty>,
}

/// An `extern function`'s C-ABI signature: it has no body, so codegen reads its
/// parameter and return types here (was `extern_sigs`).
#[derive(Clone, Debug)]
pub struct ExternSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// The lowered body of one function or `extend` method.
#[derive(Clone, Debug)]
pub struct Body {
    /// The definition this body belongs to.
    pub def: DefId,
    /// Parameter locals, in order (the same ids as [`FnSig::params`]).
    pub params: Vec<LocalId>,
    /// Every local binding in the body (parameters included) and its type
    /// (was `local_types`).
    pub locals: HashMap<LocalId, Ty>,
    /// The declared/inferred return type.
    pub ret: Ty,
    /// `Some(output)` for an async body (mirrors [`FnSig::async_output`]).
    pub async_output: Option<Ty>,
    /// The function body.
    pub block: Block,
    /// The body's source span (the brace-delimited block).
    pub span: Span,
}

impl Body {
    /// The resolved type of a local binding.
    pub fn local_ty(&self, id: LocalId) -> Option<Ty> {
        self.locals.get(&id).copied()
    }
}

// ===========================================================================
// Blocks & statements
// ===========================================================================

/// A brace-delimited block: a sequence of statements and an optional trailing
/// expression that the block evaluates to.
#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub trailing: Option<Box<Expr>>,
    /// The block's value type (`null` when there is no trailing expression).
    pub ty: Ty,
    pub span: Span,
}

/// A statement inside a block.
#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// `var pat [: T] = init;` — a local binding (or destructuring), whose bound
    /// names have already been resolved to [`LocalId`]s inside `pattern`.
    Let { pattern: Pattern, init: Expr },
    /// `lvalue = value;`
    Assign { target: Expr, value: Expr },
    /// An expression evaluated for its effect (terminated by `;`).
    Expr(Expr),
    /// A block-level item declaration (`function`, `struct`, …). Its body, if
    /// any, lives in [`Hir::bodies`] under `def`; this marker keeps provenance
    /// and lets the LSP walk nested scopes.
    Item(DefId),
}

// ===========================================================================
// Expressions
// ===========================================================================

/// A resolved, typed expression. `ty` is the type the checker assigned (was the
/// `expr_types` side table); `span` is the exact source range.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    // -- literals -----------------------------------------------------------
    /// An integer literal, parsed to its bit pattern. Signedness/width come from
    /// the node's [`Expr::ty`]. Literals are non-negative; negation is a
    /// [`ExprKind::Unary`].
    Int(u128),
    /// A floating-point literal, parsed. Width comes from [`Expr::ty`].
    Float(f64),
    Bool(bool),
    Null,
    /// A character literal as its Unicode scalar value, with escapes already
    /// processed (stored as the raw code point, matching codegen's `i32` const).
    Char(u32),
    /// A string literal, possibly interpolated.
    Str(Vec<StrPart>),

    // -- names --------------------------------------------------------------
    /// A resolved value-position name (was an `Ident`/`self` + the `resolutions`
    /// table). `self` lowers to `Name(Res::Local(self_local))`.
    Name(Res),

    // -- composites ---------------------------------------------------------
    /// `(a, b, …)` — always ≥ 2 elements.
    Tuple(Vec<Expr>),
    /// `[a, b, …]`. The element type is read from [`Expr::ty`].
    List(Vec<Expr>),
    /// `{ k: v, …, ..base }` — a map literal.
    Map(Vec<MapEntry>),
    /// `Foo { field: v, …, ..base }` — a struct literal. Generic arguments are
    /// solved (was `call_type_args` at the constructor span).
    Struct {
        def: DefId,
        type_args: Vec<Ty>,
        fields: Vec<FieldInit>,
        spread: Option<Box<Expr>>,
    },

    // -- accesses -----------------------------------------------------------
    /// `recv.field` — a struct field access, resolved to a field index.
    Field { receiver: Box<Expr>, field: FieldRef },
    /// `recv.0` — a tuple element access.
    TupleIndex { receiver: Box<Expr>, index: u32 },
    /// `recv[index]` — a builtin index into a `List`/`Map`/`str`/pointer
    /// (resolved by the receiver's type in codegen).
    Index { receiver: Box<Expr>, index: Box<Expr> },

    // -- calls --------------------------------------------------------------
    /// A call. The dispatch [`CallKind`] carries the resolved target and any
    /// solved generic arguments; `args` are the (already coerced) operands. For
    /// method calls the receiver is `args[0]` unless the call is static.
    ///
    /// `callee_span` is the source span of the callee name (the `f` in `f(x)`,
    /// the `m` in `recv.m(..)`) and `callee_ty` its type (the function type, or
    /// the receiver type for a builtin method) — provenance the IDE needs for
    /// go-to-definition and hover on the call name, which the desugared dispatch
    /// would otherwise drop. Codegen ignores both.
    Call { kind: CallKind, args: Vec<Expr>, callee_span: Span, callee_ty: Ty },

    /// A compiler intrinsic: numeric-namespace ops, collection constructors,
    /// `clone`, foreign memory, channels/threads/`Shared`, async builtins —
    /// everything that was a dedicated marker set keyed by span. `args` are the
    /// operands (empty for a constant like `i32.MAX`).
    Intrinsic { intrinsic: Intrinsic, args: Vec<Expr> },

    // -- operators ----------------------------------------------------------
    /// A unary operator. `overload` is `Some(..)` when the operand is a user
    /// type whose `extend` provides the operator (was `operator_methods`).
    Unary { op: UnaryOp, operand: Box<Expr>, overload: Option<OpOverload> },
    /// A binary operator. `overload` as in [`ExprKind::Unary`]. Short-circuit
    /// `&&`/`||` keep their structure here and are lowered to branches in codegen.
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr>, overload: Option<OpOverload> },
    /// `expr as T` or `expr is T`. `target` is the lowered cast target (was
    /// `cast_targets`); the result type (`T` for `as`, `bool` for `is`) is on
    /// [`Expr::ty`].
    Cast { op: CastOp, expr: Box<Expr>, target: Ty },

    // -- pointers / FFI -----------------------------------------------------
    /// `&expr` — address-of.
    Ref(Box<Expr>),
    /// `*expr` — pointer dereference.
    Deref(Box<Expr>),

    // -- effectful / async --------------------------------------------------
    /// `expr?` — error propagation. `branch` is set when the operand is a
    /// user `Try` wrapper (was `try_branches`); `residual_conversions` lists the
    /// failure variants routed through `FromResidual` (was `residual_conversions`).
    Try {
        expr: Box<Expr>,
        branch: Option<TryBranch>,
        residual_conversions: Vec<(Ty, DefId, Ty)>,
    },
    /// `await expr` — `output` is the awaited future's `Output` (was `awaits`).
    Await { expr: Box<Expr>, output: Ty },
    /// `spawn expr` — schedule a future-producing expression; `output` is the
    /// inner future's `Output` (was `async_spawns`).
    Spawn { expr: Box<Expr>, output: Ty },

    // -- control flow -------------------------------------------------------
    If { cond: Box<Expr>, then_block: Block, else_branch: Option<Box<Expr>> },
    Match { scrutinee: Box<Expr>, arms: Vec<MatchArm> },
    Block(Block),
    Loop(Block),
    While { cond: Box<Expr>, body: Block },
    /// `for pat in iter { … }`. `driver` records how the loop is driven (List
    /// fast path, `Iterator`, `Map`, or `AsyncIterator`) — was the `for_iters` /
    /// `for_maps` / `for_async_iters` tables.
    For {
        pattern: Pattern,
        iter: Box<Expr>,
        body: Block,
        driver: ForDriver,
        in_async: bool,
    },
    Return(Option<Box<Expr>>),
    Break(Option<Box<Expr>>),
    Continue,

    // -- function-shaped ----------------------------------------------------
    /// A closure (`(x) => body` / `function(…) {…}` / trailing closure). Carries
    /// the capture analysis codegen needs (was `closures`).
    Closure {
        params: Vec<(LocalId, Ty)>,
        captures: Vec<(LocalId, Ty)>,
        ret: Ty,
        is_async: bool,
        body: Box<Expr>,
    },
    /// `async { … }` — a zero-arg inline future literal (was `async_blocks`).
    AsyncBlock {
        output: Ty,
        params: Vec<(LocalId, Ty)>,
        captures: Vec<(LocalId, Ty)>,
        body: Block,
    },

    // -- coercions ----------------------------------------------------------
    /// An implicit coercion the checker inserted (was `adjustments`). The target
    /// type is [`Expr::ty`]; `adjust` says how (widen into a union/`dynamic`,
    /// unbox a known variant, or wrap into an interface object).
    Adjust { adjust: Adjust, expr: Box<Expr> },

    /// `_` in lvalue position — a discard target for assignment.
    Discard,

    /// A placeholder for an expression that failed to check (keeps lowering
    /// total). Never reaches a successful codegen.
    Error,
}

/// What a value-position name resolves to. Mirrors [`ValueRes`] but is the HIR's
/// own spelling so the migration can retire the side-table import later.
pub type Res = ValueRes;

/// A resolved struct-field reference: the owning struct def, the field index
/// within the layout, and the field name (kept for diagnostics / the LSP).
#[derive(Clone, Debug)]
pub struct FieldRef {
    pub struct_def: DefId,
    pub index: u32,
    pub name: String,
}

/// A resolved operator-overload target for [`ExprKind::Unary`] /
/// [`ExprKind::Binary`]: the `extend` method to call, plus the extend's solved
/// type arguments (was `operator_methods` + the type-args table keyed by the
/// operator span). Carrying the type args here keeps codegen free of any
/// span-keyed `CheckResults` lookups.
#[derive(Clone, Debug)]
pub struct OpOverload {
    pub method: DefId,
    pub type_args: Vec<Ty>,
}

/// One initializer in a struct literal: the resolved field index and its value.
#[derive(Clone, Debug)]
pub struct FieldInit {
    pub index: u32,
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

/// One item in a map literal.
#[derive(Clone, Debug)]
pub enum MapEntry {
    /// `key: value`
    Kv { key: Expr, value: Expr },
    /// `..base`
    Spread(Expr),
}

/// One part of a (possibly interpolated) string literal.
#[derive(Clone, Debug)]
pub enum StrPart {
    /// A run of literal text, with escapes already processed.
    Text(String),
    /// An interpolation hole `$x` / `${e}`. `stringify` is `Some(to_str)` when
    /// the hole has a user type whose `ToStr` impl must be called (was
    /// `stringify_methods`); `None` for builtin-typed holes codegen formats
    /// directly. `stringify_targs` are the (unresolved) generic arguments to
    /// monomorphize that `to_str` (was `call_type_args` at the hole span).
    Interp { expr: Box<Expr>, stringify: Option<DefId>, stringify_targs: Vec<Ty> },
}

/// The dispatch kind of a [`ExprKind::Call`].
#[derive(Clone, Debug)]
pub enum CallKind {
    /// A direct call to a free function. `type_args` are the solved generics
    /// (in the caller's parameters; may contain `Param`s).
    Direct { def: DefId, type_args: Vec<Ty> },
    /// A method call `recv.m(..)` or static `Type.m(..)` / `T.m(..)`. `def` is
    /// the resolved (interface or `extend`) method. `recv_static` is the
    /// receiver type for a static call (a `Named`/`Param`), so codegen can
    /// resolve an interface static method to its concrete impl; `is_static`
    /// tells codegen not to prepend a `self` receiver (was `static_calls` +
    /// `static_recv`).
    Method {
        def: DefId,
        type_args: Vec<Ty>,
        recv_static: Option<Ty>,
        is_static: bool,
    },
    /// A compiler-provided prelude builtin (`print`, `panic`, `exit`, …).
    Builtin(Builtin),
    /// A call through a closure / function-pointer value. The callee value is
    /// `args`-external — it is carried as the first boxed operand here.
    Closure { callee: Box<Expr> },
    /// A call to an `extern function` (C ABI). Reads its signature from
    /// [`Hir::extern_sigs`].
    Extern { def: DefId },
    /// A builtin method on a `List`/`Map`/`str`/`Sender`/`Receiver`/`Shared`
    /// receiver (`xs.push(x)`, `s.trim()`, `tx.send(v)`, …). These have no name
    /// resolution in the checker — codegen dispatches by the receiver's type and
    /// the method `name`. The receiver is `args[0]`.
    BuiltinMethod { name: String },
    /// A positional tuple-struct / unit-struct constructor used in call position
    /// (`Pair(1, 2)`). `def` is the struct definition (was the `StructCtor`
    /// resolution reaching `gen_tuple_ctor`).
    TupleCtor { def: DefId, type_args: Vec<Ty> },
}

/// A compiler intrinsic operation (was the dozen builtin marker sets). The
/// result type and any element/payload types are read from the node's
/// [`Expr::ty`] except where an explicit field is needed.
#[derive(Clone, Debug)]
pub enum Intrinsic {
    /// A numeric-namespace constant or operation (`i32.MAX`, `f64.is_nan(x)`,
    /// `i32.wrapping_add(a, b)`, …). Was `num_intrinsics`.
    Num(NumIntrinsic),
    /// An empty collection constructor (`List<T>()`, `Map<K,V>()`, `.new`
    /// forms). The collection type is [`Expr::ty`]. Was `builtin_ctors`.
    CollectionCtor,
    /// A builtin `.clone()` on a builtin-typed receiver. Was `clone_kinds`.
    Clone(CloneKind),
    /// `Shared.new(v)`. Was `shared_news`.
    SharedNew,
    /// `channel<T>()`. Was `channel_news`.
    ChannelNew,
    /// `Thread.spawn { … }` — `output` is the worker's result type `R`. When the
    /// closure is **async** (`() => Future<R>`, `is_async` set), the worker drives
    /// the future to completion and the handle joins on the awaited `R` (`docs/20`
    /// §1); `output` is then the awaited `R`, not `Future<R>`. Was `thread_spawns`.
    ThreadSpawn { output: Ty, is_async: bool },
    /// `JoinHandle<R>.join()` — `output` is `R`. Was `thread_joins`.
    ThreadJoin { output: Ty },
    /// `JoinHandle<R>.detach()` — relinquish the worker, fire-and-forget
    /// (`docs/20` §1). Yields `null`.
    ThreadDetach,
    /// `yield_now()`. Was `yield_nows`.
    YieldNow,
    /// `sleep(ms)`. Was `async_sleeps`.
    AsyncSleep,
    /// `timeout(fut, ms): Future<T | TimedOut>` — `output` is the success type
    /// `T` (so codegen can pass its type id + pointer-ness to the runtime).
    AsyncTimeout { output: Ty },
    /// `fut.cancel()` — a no-op for compute-only futures. Was `future_cancels`.
    FutureCancel,
    /// `Foreign.alloc<T>()` / `alloc_zeroed<T>()`. Was `foreign_allocs`.
    ForeignAlloc { ty: Ty, zeroed: bool },
    /// `Foreign.free(p)`. Was `foreign_frees`.
    ForeignFree,
    /// `Foreign.realloc<T>(p, n)`. Was `foreign_reallocs`.
    ForeignRealloc,
    /// `Foreign.alloc_flex<T, E>(extra)`. Was `foreign_flex`.
    ForeignFlex { ty: Ty, elem: Ty },
}

/// How a `for` loop is driven (was `for_iters` / `for_maps` / `for_async_iters`,
/// with the default — no entry in any table — being the `List` fast path).
#[derive(Clone, Debug)]
pub enum ForDriver {
    /// The `List<T>` fast path: index the backing buffer directly. `elem` is `T`.
    ListFast { elem: Ty },
    /// The synchronous `Iterator` protocol.
    Iter(ForIter),
    /// `for entry in map` — `(key, value, Entry<K,V>)` types.
    Map { key: Ty, value: Ty, entry: Ty },
    /// `for await x in stream` — the `AsyncIterator` protocol.
    AsyncIter(ForAsyncIter),
    /// `for ch in s` over a `str` — desugars to iterating `s.chars()`
    /// (`docs/18` §4); codegen snapshots the scalars and index-loops them.
    StrChars,
    /// `for n in rx` over a `Receiver<T>` (`docs/20` §2): blocking-recv each
    /// message, terminating (`Done`) once the channel is closed and drained.
    /// `elem` is `T`.
    Channel { elem: Ty },
}

/// One arm of a `match`.
#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

// ===========================================================================
// Operators (re-spelled here so HIR consumers need not import the AST)
// ===========================================================================

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum BinaryOp {
    Add, Sub, Mul, Div, Rem,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
    BitAnd, BitOr, BitXor,
    Shl, Shr,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum CastOp {
    As,
    Is,
}

// ===========================================================================
// Patterns
// ===========================================================================

/// A resolved pattern. Bound names are already [`LocalId`]s; type-test patterns
/// carry the lowered variant type they test for (was `pattern_types`).
#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    /// `_`
    Wildcard,
    /// A binding `name` — resolved to its local id.
    Bind(LocalId),
    /// A literal pattern `42` / `"x"` / `'c'` / `true` / `null` / `-1`.
    Literal(Box<Expr>),
    /// `T x` / `T` — a type-test pattern. `test_ty` is the lowered variant type
    /// it matches; `bind` is the bound local if any.
    TypeBind { test_ty: Ty, bind: Option<LocalId> },
    /// `Red` — a unit-struct / unit-variant pattern. `test_ty` is the variant.
    UnitPath { def: DefId, test_ty: Ty },
    /// `Some(a, b)` — positional destructuring of a tuple struct.
    TupleStruct { def: DefId, fields: Vec<Pattern>, rest: Option<RestPattern> },
    /// `Person { name, age, .. }` — record destructuring; each field is resolved
    /// to its index in the struct layout.
    RecordStruct { def: DefId, fields: Vec<FieldPattern>, has_rest: bool },
    /// `(a, b)` / `(a, ..rest, b)` — tuple destructuring.
    Tuple { elems: Vec<Pattern>, rest: Option<(usize, RestPattern)> },
    /// `[a, b]` / `[head, ..tail]` — list destructuring.
    List { elems: Vec<Pattern>, rest: Option<(usize, RestPattern)> },
    /// `P1 | P2 | …`
    Or(Vec<Pattern>),
}

/// A `..name` / `..` rest binding. `bind` is the bound local when the rest is
/// named and captures a sub-collection.
#[derive(Clone, Debug)]
pub struct RestPattern {
    pub bind: Option<LocalId>,
    pub span: Span,
}

/// One field in a record-struct pattern.
#[derive(Clone, Debug)]
pub struct FieldPattern {
    pub index: u32,
    pub name: String,
    pub pattern: Pattern,
    pub span: Span,
}

// ===========================================================================
// HIR build utilities (used by the type-checker as it emits the HIR directly)
// ===========================================================================

/// The libraries named by `@Link(lib = "…")` / `@Link("…")` on `extern function`
/// declarations (`docs/19` §13), de-duplicated in first-seen order. Consumed as
/// [`Hir::link_libs`] (JIT `dlopen`) and by the CLI's native linker (`-l`).
pub fn collect_link_libs(prog: &crate::sema::symbols::Program) -> Vec<String> {
    use crate::ast;
    let mut libs: Vec<String> = Vec::new();
    for def in &prog.defs {
        if !matches!(def.item, Some(ast::ItemKind::Extern(ast::ExternItem::Function(_)))) {
            continue;
        }
        for attr in &def.attrs {
            if attr.name.name != "Link" {
                continue;
            }
            for a in &attr.args {
                let value = match a {
                    ast::AttrArg::Named { name, value, .. } if name.name == "lib" => value,
                    ast::AttrArg::Positional(e) => e,
                    _ => continue,
                };
                if let ast::ExprKind::Str(s) = &value.kind {
                    let lib = match s.parts.as_slice() {
                        [] => Some(String::new()),
                        [ast::StringPart::Text { text, .. }] => Some(text.clone()),
                        _ => None,
                    };
                    if let Some(lib) = lib {
                        if !lib.is_empty() && !libs.contains(&lib) {
                            libs.push(lib);
                        }
                    }
                }
            }
        }
    }
    libs
}

pub(crate) fn parse_int_lit(lit: &crate::ast::IntLit) -> u128 {
    let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
    let radix = match lit.base {
        crate::token::IntBase::Dec => 10,
        crate::token::IntBase::Hex => 16,
        crate::token::IntBase::Oct => 8,
        crate::token::IntBase::Bin => 2,
    };
    u128::from_str_radix(&digits, radix).unwrap_or(0)
}

pub(crate) fn parse_float_lit(lit: &crate::ast::FloatLit) -> f64 {
    let raw: String = lit.raw.chars().filter(|c| *c != '_').collect();
    raw.parse().unwrap_or(0.0)
}

/// Parse a char literal to its Unicode scalar value, mirroring the backend's
/// `parse_char` so HIR and codegen agree byte-for-byte.
pub(crate) fn parse_char_lit(raw: &str) -> Option<u32> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        return if chars.next().is_none() { Some(first as u32) } else { None };
    }
    let esc = chars.next()?;
    let val = match esc {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '\\' => '\\' as u32,
        '\'' => '\'' as u32,
        '"' => '"' as u32,
        '0' => 0,
        'u' => {
            let rest: String = chars.collect();
            let hex = rest.strip_prefix('{')?.strip_suffix('}')?;
            return u32::from_str_radix(hex, 16).ok();
        }
        _ => return None,
    };
    if chars.next().is_none() { Some(val) } else { None }
}

pub(crate) fn lower_unop(op: crate::ast::UnaryOp) -> UnaryOp {
    use crate::ast::UnaryOp as A;
    match op {
        A::Neg => UnaryOp::Neg,
        A::Not | A::BitNot => UnaryOp::Not,
    }
}

pub(crate) fn lower_binop(op: crate::ast::BinaryOp) -> BinaryOp {
    use crate::ast::BinaryOp as A;
    match op {
        A::Add => BinaryOp::Add,
        A::Sub => BinaryOp::Sub,
        A::Mul => BinaryOp::Mul,
        A::Div => BinaryOp::Div,
        A::Rem => BinaryOp::Rem,
        A::Eq => BinaryOp::Eq,
        A::Ne => BinaryOp::Ne,
        A::Lt => BinaryOp::Lt,
        A::Le => BinaryOp::Le,
        A::Gt => BinaryOp::Gt,
        A::Ge => BinaryOp::Ge,
        A::And => BinaryOp::And,
        A::Or => BinaryOp::Or,
        A::BitAnd => BinaryOp::BitAnd,
        A::BitOr => BinaryOp::BitOr,
        A::BitXor => BinaryOp::BitXor,
        A::Shl => BinaryOp::Shl,
        A::Shr => BinaryOp::Shr,
    }
}

pub(crate) fn lower_castop(op: crate::ast::CastOp) -> CastOp {
    match op {
        crate::ast::CastOp::As => CastOp::As,
        crate::ast::CastOp::Is => CastOp::Is,
    }
}

pub mod pretty;
pub use pretty::print_program;

// (The former `hir::lower` pass is gone: the type-checker emits the HIR directly
// and assembles it in `Checker::finish`. The small build utilities above —
// `collect_link_libs`, the literal parsers, and operator lowering — are all that
// remains, and the checker calls them as it builds nodes.)

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lower_tests;
