//! Async source-level lowering: `await`-hoisting normalization (ANF for
//! suspending expressions) and async-closure desugaring.
//!
//! Async closures `(p) async => E` are desugared here into a plain closure
//! returning an async block — `(p) => async { E }` — so they reuse the existing
//! closure-environment + async-block state-machine codegen with no special case
//! (`docs/21` §7). Calling the closure builds the future (capturing `p` and the
//! outer environment) without running `E`; `await` then drives it.
//!
//! The async state machine (`docs/21`) can only suspend at *statement-level*
//! positions — a `var`/assign right-hand side, a bare expression statement, a
//! block's trailing expression, or a `return`/`break` operand. An `await` that
//! appears deeper inside an expression (`f(await x)`, `a + await b`,
//! `xs[await i]`) would otherwise leave intermediate temporaries live across the
//! suspend, which the state machine does not save.
//!
//! This source-level pass (run before collection, like `derive`) rewrites such
//! nested `await`s into preceding `var` bindings, preserving left-to-right
//! evaluation order:
//!
//! ```text
//! var y = f(g(), await a) + 1;
//! // becomes
//! var __await_0 = g();
//! var __await_1 = await a;
//! var y = f(__await_0, __await_1) + 1;
//! ```
//!
//! **Correctness — conditional positions are NOT hoisted.** Hoisting an `await`
//! out of the right operand of `&&`/`||`, or out of a `while` condition, would
//! change *when* (or whether) the future is awaited. Those cases are left
//! untouched: if they truly contain an `await` in a non-statement position the
//! backend reports a clear "await in this position is not yet supported" error
//! rather than miscompiling. `if`/`match` *branches* and `match` *arm bodies*
//! are blocks, so an `await` there is already statement/trailing-level and works
//! — this pass simply recurses into them. The transformation never reorders
//! observable side effects and never lifts an `await` past a conditional guard.

use crate::ast::*;
use crate::span::{BytePos, FileId, Span};
use std::sync::atomic::{AtomicU32, Ordering};

/// Synthesised nodes live in a dedicated virtual file so their spans are unique
/// and never collide with real source (the checker keys HIR nodes by span).
const ANF_FILE: FileId = FileId(u32::MAX - 2);
static SPAN_CTR: AtomicU32 = AtomicU32::new(0);
static NAME_CTR: AtomicU32 = AtomicU32::new(0);

fn nsp() -> Span {
    let n = SPAN_CTR.fetch_add(1, Ordering::Relaxed);
    Span::new(ANF_FILE, BytePos(n), BytePos(n + 1))
}

/// A fresh, program-unique hoist-temporary name.
fn fresh_name() -> String {
    let n = NAME_CTR.fetch_add(1, Ordering::Relaxed);
    format!("__await_{n}")
}

/// Hoist nested `await`s in every function/method/closure body of `module`
/// (recursively through inline submodules), so all surviving `await`s sit at a
/// statement-level position the async state machine can suspend at.
pub fn hoist_awaits(module: &mut Module) {
    for item in &mut module.items {
        hoist_item(item);
    }
}

fn hoist_item(item: &mut Item) {
    match &mut item.kind {
        ItemKind::Function(f) => {
            if let Some(body) = &mut f.body {
                process_block(body);
            }
        }
        ItemKind::Extend(e) => {
            for m in &mut e.members {
                if let Some(body) = &mut m.function.body {
                    process_block(body);
                }
            }
        }
        ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) => {
            for it in items {
                hoist_item(it);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Block / statement processing
// ---------------------------------------------------------------------------

/// Rewrite every statement of `block` in place, inserting hoisted `var` bindings
/// before each statement that needs them.
fn process_block(block: &mut Block) {
    let mut out: Vec<Stmt> = Vec::with_capacity(block.stmts.len());
    for stmt in std::mem::take(&mut block.stmts) {
        let mut pre = Vec::new();
        let kind = match stmt.kind {
            StmtKind::Var(mut v) => {
                v.init = rewrite_tail(v.init, &mut pre);
                StmtKind::Var(v)
            }
            StmtKind::Assign { target, value } => {
                // The assignment target is an lvalue (place); hoist any await in
                // an index/receiver sub-expression, then the value as a tail.
                let target = rewrite(target, &mut pre);
                let value = rewrite_tail(value, &mut pre);
                StmtKind::Assign { target, value }
            }
            StmtKind::Expr(e) => StmtKind::Expr(rewrite_tail(e, &mut pre)),
            StmtKind::Item(mut it) => {
                hoist_item(&mut it);
                StmtKind::Item(it)
            }
        };
        out.append(&mut pre);
        out.push(Stmt { kind, span: stmt.span });
    }
    if let Some(t) = block.trailing.take() {
        let mut pre = Vec::new();
        let t = rewrite_tail(*t, &mut pre);
        out.append(&mut pre);
        block.trailing = Some(Box::new(t));
    }
    block.stmts = out;
}

fn process_else(e: ElseBranch) -> ElseBranch {
    match e {
        ElseBranch::If(inner) => {
            // `else if` — recurse into the boxed `If` as a tail expression
            // (its condition is hoisted into a fresh wrapper if needed).
            let mut pre = Vec::new();
            let rewritten = rewrite(*inner, &mut pre);
            ElseBranch::If(Box::new(wrap_with_pre(rewritten, pre)))
        }
        ElseBranch::Block(mut b) => {
            process_block(&mut b);
            ElseBranch::Block(b)
        }
    }
}

// ---------------------------------------------------------------------------
// Expression rewriting
// ---------------------------------------------------------------------------

/// Rewrite an expression in a *tail* position (statement value, block trailing,
/// closure body, `return`/`break` operand, match-arm body), where a *top-level*
/// `await` may legally remain. Nested operand `await`s are hoisted into `pre`.
fn rewrite_tail(e: Expr, pre: &mut Vec<Stmt>) -> Expr {
    rewrite(e, pre)
}

/// Core rewrite: returns an expression equivalent to `e`, appending any hoisted
/// bindings to `pre` in evaluation order. A top-level `await` is preserved;
/// `await`s in operand positions are atomized (hoisted to temporaries).
fn rewrite(e: Expr, pre: &mut Vec<Stmt>) -> Expr {
    let span = e.span;
    let kind = match e.kind {
        // Leaves — nothing to do.
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::SelfExpr
        | ExprKind::Underscore
        | ExprKind::Continue => e.kind,

        // `await E` — the canonical statement-level form. Keep the wrapper and
        // rewrite the (unconditional) inner; a parent operand position will
        // atomize the whole `await` if needed.
        ExprKind::Await { expr, kw_span } => ExprKind::Await {
            expr: Box::new(rewrite(*expr, pre)),
            kw_span,
        },

        // `spawn E` evaluates `E` (a call) to a future in the current frame, so
        // operand `await`s inside `E` are unconditional — rewrite through.
        ExprKind::Spawn { expr, kw_span } => ExprKind::Spawn {
            expr: Box::new(rewrite(*expr, pre)),
            kw_span,
        },

        ExprKind::Paren(inner) => ExprKind::Paren(Box::new(rewrite(*inner, pre))),

        // `E?` — `await` in the operand must move out first.
        ExprKind::Try { expr, q_span } => ExprKind::Try {
            expr: Box::new(atomize(*expr, pre)),
            q_span,
        },

        ExprKind::Cast { op, op_span, expr, ty } => ExprKind::Cast {
            op,
            op_span,
            expr: Box::new(atomize(*expr, pre)),
            ty,
        },

        ExprKind::Unary { op, op_span, operand } => ExprKind::Unary {
            op,
            op_span,
            operand: Box::new(atomize(*operand, pre)),
        },

        ExprKind::Ref { expr, amp_span } => ExprKind::Ref {
            expr: Box::new(atomize(*expr, pre)),
            amp_span,
        },
        ExprKind::Deref { expr, star_span } => ExprKind::Deref {
            expr: Box::new(atomize(*expr, pre)),
            star_span,
        },

        ExprKind::Field { receiver, name } => ExprKind::Field {
            receiver: Box::new(atomize(*receiver, pre)),
            name,
        },
        ExprKind::TupleIndex { receiver, index, index_span } => ExprKind::TupleIndex {
            receiver: Box::new(atomize(*receiver, pre)),
            index,
            index_span,
        },

        ExprKind::Binary { op, op_span, left, right } => {
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                // Short-circuit: the left operand is unconditional (atomize it);
                // the right runs only conditionally, so it must NOT be hoisted
                // out. Recurse into the right in isolation — if that would hoist
                // anything (a conditional `await`), keep the original so the
                // backend reports the unsupported position rather than us
                // changing semantics.
                let left = atomize(*left, pre);
                let right = rewrite_isolated(*right);
                ExprKind::Binary { op, op_span, left: Box::new(left), right: Box::new(right) }
            } else {
                let mut ops = atomize_seq(vec![*left, *right], pre);
                let right = ops.pop().unwrap();
                let left = ops.pop().unwrap();
                ExprKind::Binary { op, op_span, left: Box::new(left), right: Box::new(right) }
            }
        }

        ExprKind::Index { receiver, index } => {
            let mut ops = atomize_seq(vec![*receiver, *index], pre);
            let index = ops.pop().unwrap();
            let receiver = ops.pop().unwrap();
            ExprKind::Index { receiver: Box::new(receiver), index: Box::new(index) }
        }

        ExprKind::Call { callee, generics, args, trailing_closure } => {
            // For a method call the callee is `Field { receiver, name }`: the
            // receiver is the value operand (atomized); `name` stays. For a free
            // call the callee is usually an atom (`Ident`), but atomize handles
            // a computed callee too. Trailing closures are separate scopes.
            let tc = trailing_closure.map(|c| Box::new(rewrite_scope(*c)));
            match callee.kind {
                ExprKind::Field { receiver, name } => {
                    let mut seq = vec![*receiver];
                    seq.extend(args);
                    let mut atomized = atomize_seq(seq, pre);
                    let new_args = atomized.split_off(1);
                    let recv = atomized.pop().unwrap();
                    let callee = Expr {
                        kind: ExprKind::Field { receiver: Box::new(recv), name },
                        span: callee.span,
                    };
                    ExprKind::Call { callee: Box::new(callee), generics, args: new_args, trailing_closure: tc }
                }
                _ => {
                    let callee = atomize(*callee, pre);
                    let new_args = atomize_seq(args, pre);
                    ExprKind::Call { callee: Box::new(callee), generics, args: new_args, trailing_closure: tc }
                }
            }
        }

        ExprKind::Tuple(elems) => ExprKind::Tuple(atomize_seq(elems, pre)),
        ExprKind::List(elems) => ExprKind::List(atomize_seq(elems, pre)),

        ExprKind::StructLit { path, fields, spread } => {
            // Evaluate field initializers (then the spread) in source order.
            let mut vals = Vec::new();
            let mut idx = Vec::new();
            for (i, f) in fields.iter().enumerate() {
                if let Some(v) = &f.value {
                    vals.push(v.clone());
                    idx.push(i);
                }
            }
            if let Some(s) = &spread {
                vals.push((**s).clone());
            }
            let mut atomized = atomize_seq(vals, pre);
            let new_spread = if spread.is_some() { Some(Box::new(atomized.pop().unwrap())) } else { None };
            let mut new_fields = fields;
            for (slot, i) in idx.into_iter().enumerate() {
                new_fields[i].value = Some(atomized[slot].clone());
            }
            ExprKind::StructLit { path, fields: new_fields, spread: new_spread }
        }

        ExprKind::MapLit(items) => {
            // Flatten key/value exprs in order, atomize, then re-thread.
            let mut vals = Vec::new();
            let mut shapes: Vec<bool> = Vec::new(); // true = Entry (key+value), false = Spread
            for it in &items {
                match it {
                    MapItem::Entry { key, value, .. } => {
                        vals.push((**key).clone());
                        vals.push((**value).clone());
                        shapes.push(true);
                    }
                    MapItem::Spread(e) => {
                        vals.push((**e).clone());
                        shapes.push(false);
                    }
                }
            }
            let atomized = atomize_seq(vals, pre);
            let mut it = atomized.into_iter();
            let mut new_items = Vec::with_capacity(items.len());
            for (orig, is_entry) in items.into_iter().zip(shapes) {
                match orig {
                    MapItem::Entry { span, .. } if is_entry => {
                        let key = it.next().unwrap();
                        let value = it.next().unwrap();
                        new_items.push(MapItem::Entry { key: Box::new(key), value: Box::new(value), span });
                    }
                    MapItem::Spread(_) => {
                        new_items.push(MapItem::Spread(Box::new(it.next().unwrap())));
                    }
                    other => new_items.push(other),
                }
            }
            ExprKind::MapLit(new_items)
        }

        ExprKind::Str(lit) => {
            // Interpolation holes are evaluated in order; atomize the `${expr}`
            // holes (bare `$ident` holes are atoms and stay).
            let mut hole_vals = Vec::new();
            let mut hole_idx = Vec::new();
            for (i, part) in lit.parts.iter().enumerate() {
                if let StringPart::Expr(e) = part {
                    hole_vals.push(e.clone());
                    hole_idx.push(i);
                }
            }
            if hole_vals.is_empty() {
                ExprKind::Str(lit)
            } else {
                let atomized = atomize_seq(hole_vals, pre);
                let mut parts = lit.parts;
                for (slot, i) in hole_idx.into_iter().enumerate() {
                    parts[i] = StringPart::Expr(atomized[slot].clone());
                }
                ExprKind::Str(StringLit { parts, span: lit.span })
            }
        }

        // Control flow as a (possibly value-producing) expression. Conditions /
        // scrutinees are unconditional → atomize into `pre`; branch blocks and
        // arm bodies are recursed (their `await`s are already statement-level).
        ExprKind::If { cond, then_block, else_branch } => {
            let cond = atomize(*cond, pre);
            let mut then_block = then_block;
            process_block(&mut then_block);
            let else_branch = else_branch.map(process_else);
            ExprKind::If { cond: Box::new(cond), then_block, else_branch }
        }
        ExprKind::Match { scrutinee, arms } => {
            let scrutinee = atomize(*scrutinee, pre);
            let arms = arms.into_iter().map(rewrite_arm).collect();
            ExprKind::Match { scrutinee: Box::new(scrutinee), arms }
        }
        ExprKind::Block(mut b) => {
            process_block(&mut b);
            ExprKind::Block(b)
        }
        ExprKind::Loop(mut b) => {
            process_block(&mut b);
            ExprKind::Loop(b)
        }
        ExprKind::While { cond, mut body } => {
            // The condition runs every iteration, so an `await` there cannot be
            // hoisted out — leave it (the backend rejects it). Recurse the body.
            process_block(&mut body);
            ExprKind::While { cond, body }
        }
        ExprKind::For { pattern, in_async, iter, mut body } => {
            process_block(&mut body);
            // `for await x in EXPR`: the async backend re-loads the stream each
            // iteration (across suspends), so it must be a simple variable.
            // Hoist any other stream expression into a preceding `var` (which
            // gets a slot in the state machine and survives suspends), making
            // `for await x in make_stream()` work (`docs/21` §10).
            let iter = if in_async && !is_simple_place(&iter) {
                let hoisted = rewrite(*iter, pre);
                Box::new(hoist(hoisted, pre))
            } else {
                Box::new(rewrite(*iter, pre))
            };
            ExprKind::For { pattern, in_async, iter, body }
        }

        ExprKind::Return(v) => {
            ExprKind::Return(v.map(|e| Box::new(rewrite_tail(*e, pre))))
        }
        ExprKind::Break(v) => {
            ExprKind::Break(v.map(|e| Box::new(rewrite_tail(*e, pre))))
        }

        // Independent async scopes: process their bodies on their own; they
        // never contribute to the enclosing `pre`.
        ExprKind::Closure { params, return_type, is_async, body } => {
            // Desugar an async closure `(p) async => E` into a plain closure
            // returning an async block — `(p) => async { E }` (`docs/21` §7).
            // Calling it constructs the future (capturing `p` + the outer
            // environment) without running `E`; `await` then drives it. This
            // reuses the closure-environment + async-block state-machine codegen
            // verbatim — there is no separate "async closure" lowering.
            let body = if is_async {
                let block = Block { stmts: Vec::new(), trailing: Some(body), span: nsp() };
                let ab = Expr { kind: ExprKind::AsyncBlock(block), span: nsp() };
                rewrite_scope(ab)
            } else {
                rewrite_scope(*body)
            };
            ExprKind::Closure { params, return_type, is_async: false, body: Box::new(body) }
        }
        ExprKind::AnonFn(f) => {
            // `function(params): Ret [async] { body }` is the same kind of value
            // as an arrow closure (`docs/09` §4) — desugar to one so it reuses
            // the closure (and, when `async`, the async-closure) lowering. Only
            // the non-generic form maps to a closure; a generic anonymous
            // function is left as-is (and reported by the checker).
            let f = *f;
            if f.generics.is_none() {
                if let Some(block) = f.body {
                    let params = f
                        .params
                        .into_iter()
                        .filter_map(|p| match p.kind {
                            ParamKind::Normal { name, ty } => {
                                Some(ClosureParam { name, ty: Some(ty), span: p.span })
                            }
                            ParamKind::SelfParam => None,
                        })
                        .collect();
                    let body = Expr { kind: ExprKind::Block(block), span: nsp() };
                    let closure = Expr {
                        kind: ExprKind::Closure {
                            params,
                            return_type: f.return_type,
                            is_async: f.is_async,
                            body: Box::new(body),
                        },
                        span,
                    };
                    return rewrite(closure, pre);
                }
            }
            let mut f = f;
            if let Some(body) = &mut f.body {
                process_block(body);
            }
            ExprKind::AnonFn(Box::new(f))
        }
        ExprKind::AsyncBlock(mut b) => {
            process_block(&mut b);
            ExprKind::AsyncBlock(b)
        }
    };
    Expr { kind, span }
}

/// Rewrite a match-arm body (a conditional tail position). Hoisted bindings stay
/// *inside* the arm (wrapped in a block) so they only run when the arm matches.
fn rewrite_arm(arm: MatchArm) -> MatchArm {
    let mut pre = Vec::new();
    let body = rewrite(arm.body, &mut pre);
    MatchArm { pattern: arm.pattern, guard: arm.guard, body: wrap_with_pre(body, pre), span: arm.span }
}

/// Rewrite a closure/`=>` body as its own scope. The body is a tail expression;
/// any hoisted bindings are wrapped into a block so they run inside the closure.
fn rewrite_scope(body: Expr) -> Expr {
    let mut pre = Vec::new();
    let body = rewrite(body, &mut pre);
    wrap_with_pre(body, pre)
}

/// Process an expression that must NOT lift `await`s into the enclosing `pre`
/// (a conditional position: `&&`/`||` right operand). If rewriting it would
/// hoist anything, keep the original so the backend reports the unsupported
/// nested-`await` position instead of us changing evaluation semantics.
fn rewrite_isolated(e: Expr) -> Expr {
    let orig = e.clone();
    let mut pre = Vec::new();
    let rewritten = rewrite(e, &mut pre);
    if pre.is_empty() {
        rewritten
    } else {
        orig
    }
}

/// If `pre` is non-empty, wrap `value` in a block `{ pre…; value }` so the
/// hoisted bindings execute before (and in the same scope as) the value.
fn wrap_with_pre(value: Expr, pre: Vec<Stmt>) -> Expr {
    if pre.is_empty() {
        return value;
    }
    let span = value.span;
    Expr {
        kind: ExprKind::Block(Block { stmts: pre, trailing: Some(Box::new(value)), span: nsp() }),
        span,
    }
}

/// Reduce `e` to something usable as an operand: rewrite it, then — if it still
/// carries an `await` in the current scope — hoist it into a fresh `var` and
/// return a reference to that variable.
fn atomize(e: Expr, pre: &mut Vec<Stmt>) -> Expr {
    let rewritten = rewrite(e, pre);
    if contains_await(&rewritten) {
        hoist(rewritten, pre)
    } else {
        rewritten
    }
}

/// Atomize a sequence of operands in left-to-right order. If any operand carries
/// an `await`, every *effectful* (non-atom) operand is hoisted to a temporary so
/// the original evaluation order is preserved; pure atoms are left in place.
fn atomize_seq(items: Vec<Expr>, pre: &mut Vec<Stmt>) -> Vec<Expr> {
    let any_await = items.iter().any(contains_await);
    if !any_await {
        // No suspension here; still recurse for nested scopes, no hoisting.
        return items.into_iter().map(|e| rewrite(e, pre)).collect();
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let rewritten = rewrite(item, pre);
        if is_atom(&rewritten) {
            out.push(rewritten);
        } else {
            out.push(hoist(rewritten, pre));
        }
    }
    out
}

/// Bind `value` to a fresh hoist temporary appended to `pre`, returning an
/// identifier expression that reads it.
fn hoist(value: Expr, pre: &mut Vec<Stmt>) -> Expr {
    let name = fresh_name();
    let bind_ident = Ident { name: name.clone(), span: nsp() };
    let use_ident = Ident { name, span: nsp() };
    let var = LocalVar {
        pattern: Pattern { kind: PatternKind::Binding(bind_ident), span: nsp() },
        ty: None,
        init: value,
    };
    pre.push(Stmt { kind: StmtKind::Var(var), span: nsp() });
    Expr { kind: ExprKind::Ident(use_ident), span: nsp() }
}

/// Whether `e` is a simple variable place (`x` or `self`) — already safe to
/// re-load each loop iteration without re-evaluating side effects, so a
/// `for await` over it needs no hoisting.
fn is_simple_place(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Ident(_) | ExprKind::SelfExpr)
}

/// Whether `e` is a trivial, side-effect-free, value-stable atom that needs no
/// temporary when other operands are hoisted.
fn is_atom(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Char(_)
            | ExprKind::Ident(_)
            | ExprKind::SelfExpr
    )
}

/// Whether `e` contains an `await` in the *current* async scope — recursing
/// through ordinary expressions but stopping at nested closures / `async {}`
/// blocks / anonymous functions, whose `await`s belong to their own state
/// machine.
fn contains_await(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Await { .. } => true,

        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::SelfExpr
        | ExprKind::Underscore
        | ExprKind::Continue => false,

        // Nested scopes — not the current state machine's awaits.
        ExprKind::Closure { .. } | ExprKind::AnonFn(_) | ExprKind::AsyncBlock(_) => false,

        ExprKind::Paren(e)
        | ExprKind::Spawn { expr: e, .. }
        | ExprKind::Try { expr: e, .. }
        | ExprKind::Cast { expr: e, .. }
        | ExprKind::Unary { operand: e, .. }
        | ExprKind::Ref { expr: e, .. }
        | ExprKind::Deref { expr: e, .. }
        | ExprKind::Field { receiver: e, .. }
        | ExprKind::TupleIndex { receiver: e, .. } => contains_await(e),

        ExprKind::Binary { left, right, .. } => contains_await(left) || contains_await(right),
        ExprKind::Index { receiver, index } => contains_await(receiver) || contains_await(index),
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            contains_await(callee)
                || args.iter().any(contains_await)
                // A trailing closure is its own scope — ignore its awaits.
                || trailing_closure.as_ref().is_some_and(|_c| false)
        }
        ExprKind::Tuple(es) | ExprKind::List(es) => es.iter().any(contains_await),
        ExprKind::StructLit { fields, spread, .. } => {
            fields.iter().any(|f| f.value.as_ref().is_some_and(contains_await))
                || spread.as_ref().is_some_and(|s| contains_await(s))
        }
        ExprKind::MapLit(items) => items.iter().any(|it| match it {
            MapItem::Entry { key, value, .. } => contains_await(key) || contains_await(value),
            MapItem::Spread(e) => contains_await(e),
        }),
        ExprKind::Str(lit) => lit.parts.iter().any(|p| match p {
            StringPart::Expr(e) => contains_await(e),
            _ => false,
        }),
        ExprKind::Return(v) | ExprKind::Break(v) => v.as_ref().is_some_and(|e| contains_await(e)),

        // Control-flow blocks/conditions: only the unconditional condition /
        // scrutinee is relevant to *this* node's operand decisions; branch
        // bodies are handled by recursing into them during rewriting. We report
        // `true` if the condition itself awaits so the node is processed.
        ExprKind::If { cond, .. } => contains_await(cond),
        ExprKind::Match { scrutinee, .. } => contains_await(scrutinee),
        ExprKind::While { cond, .. } => contains_await(cond),
        ExprKind::For { .. }
        | ExprKind::Loop(_)
        | ExprKind::Block(_) => false,
    }
}
