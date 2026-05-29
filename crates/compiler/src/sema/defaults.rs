//! Interface default-method expansion (`docs/10`).
//!
//! An interface method may carry a default body:
//!
//! ```text
//! interface Named { function name(self): str; function greet(self): str { "Hi " + self.name() } }
//! ```
//!
//! A type that implements the interface but does not override the method uses
//! the default. This pass (run before collection, like `derive`) realises that
//! by copying each un-overridden default body into the implementing `extend`
//! block as an ordinary method. Compiling it then reuses all existing machinery
//! — crucially, `Self` resolves to the `extend`'s target type for free — with no
//! new monomorphisation path.
//!
//! Each copied method is given fresh, unique spans (the checker keys its typed
//! HIR by span; two implementers sharing the interface's body spans would
//! collide — the same rule `derive` follows).
//!
//! Scope: the interface must be **non-generic** and declared in the same module
//! (or an inline submodule) as the `extend`. Generic-interface defaults and
//! cross-module-interface defaults are a follow-up (a type that needs one simply
//! does not receive the default and must spell the method out).

use crate::ast::*;
use crate::span::{BytePos, FileId, Span};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};

const DEFAULTS_FILE: FileId = FileId(u32::MAX - 3);
static SPAN_CTR: AtomicU32 = AtomicU32::new(0);

fn nsp() -> Span {
    let n = SPAN_CTR.fetch_add(1, Ordering::Relaxed);
    Span::new(DEFAULTS_FILE, BytePos(n), BytePos(n + 1))
}

/// Expand interface default methods into implementing `extend` blocks across
/// `module` (recursively through inline submodules).
pub fn expand_default_methods(module: &mut Module) {
    // Index the non-generic interfaces declared in this module by name.
    let mut ifaces: HashMap<String, InterfaceItem> = HashMap::new();
    for item in &module.items {
        if let ItemKind::Interface(i) = &item.kind {
            if i.generics.is_none() {
                ifaces.insert(i.name.name.clone(), i.clone());
            }
        }
    }

    for item in &mut module.items {
        if let ItemKind::Extend(e) = &mut item.kind {
            let mut have: HashSet<String> =
                e.members.iter().map(|m| m.function.name.name.clone()).collect();
            let mut additions: Vec<ExtendMember> = Vec::new();
            for iface_ty in &e.interfaces {
                let TypeKind::Named { name, generics } = &iface_ty.kind else { continue };
                if !generics.is_empty() {
                    continue; // generic interfaces: deferred
                }
                let Some(iface) = ifaces.get(&name.name) else { continue };
                for m in &iface.members {
                    let Some(body) = &m.default_body else { continue };
                    if have.contains(&m.function.name.name) {
                        continue; // overridden by the impl
                    }
                    have.insert(m.function.name.name.clone());
                    additions.push(synth_member(m, body));
                }
            }
            e.members.extend(additions);
        }
    }

    for item in &mut module.items {
        if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) =
            &mut item.kind
        {
            let mut sub = Module { inner_docs: Vec::new(), items: std::mem::take(items), span: item.span };
            expand_default_methods(&mut sub);
            *items = sub.items;
        }
    }
}

/// Build an `extend` member from an interface default method + body, re-spanned.
fn synth_member(m: &InterfaceMember, body: &Block) -> ExtendMember {
    let mut f = FunctionItem {
        name: m.function.name.clone(),
        generics: m.function.generics.clone(),
        params: m.function.params.clone(),
        return_type: m.function.return_type.clone(),
        is_async: m.function.is_async,
        body: Some(body.clone()),
    };
    rs_function(&mut f);
    ExtendMember {
        docs: Vec::new(),
        attrs: Vec::new(),
        visibility: Visibility::Private,
        function: f,
        span: nsp(),
    }
}

// ---------------------------------------------------------------------------
// In-place re-spanning: assign a fresh unique span to every node of the copied
// method so the checker's span-keyed HIR tables never collide between copies.
// ---------------------------------------------------------------------------

fn rs_function(f: &mut FunctionItem) {
    rs_ident(&mut f.name);
    if let Some(g) = &mut f.generics {
        rs_generics(g);
    }
    for p in &mut f.params {
        rs_param(p);
    }
    if let Some(t) = &mut f.return_type {
        rs_type(t);
    }
    if let Some(b) = &mut f.body {
        rs_block(b);
    }
}

fn rs_generics(g: &mut GenericParams) {
    g.span = nsp();
    for p in &mut g.params {
        rs_ident(&mut p.name);
        for b in &mut p.bounds {
            rs_type(b);
        }
    }
}

fn rs_ident(i: &mut Ident) {
    i.span = nsp();
}

fn rs_param(p: &mut Param) {
    p.span = nsp();
    if let ParamKind::Normal { name, ty } = &mut p.kind {
        rs_ident(name);
        rs_type(ty);
    }
}

fn rs_type(t: &mut Type) {
    t.span = nsp();
    match &mut t.kind {
        TypeKind::Named { name, generics } => {
            rs_ident(name);
            for g in generics {
                rs_type(g);
            }
        }
        TypeKind::Tuple(ts) | TypeKind::Union(ts) => {
            for x in ts {
                rs_type(x);
            }
        }
        TypeKind::Function { params, ret } => {
            for p in params {
                rs_type(p);
            }
            rs_type(ret);
        }
        TypeKind::ExternFunction { params, ret } => {
            for p in params {
                rs_type(&mut p.ty);
                p.span = nsp();
            }
            rs_type(ret);
        }
        TypeKind::Pointer(inner) | TypeKind::Paren(inner) => rs_type(inner),
        TypeKind::Array { elem, len } => {
            rs_type(elem);
            rs_expr(len);
        }
        TypeKind::SelfType => {}
    }
}

fn rs_block(b: &mut Block) {
    b.span = nsp();
    for s in &mut b.stmts {
        rs_stmt(s);
    }
    if let Some(t) = &mut b.trailing {
        rs_expr(t);
    }
}

fn rs_stmt(s: &mut Stmt) {
    s.span = nsp();
    match &mut s.kind {
        StmtKind::Var(v) => {
            rs_pattern(&mut v.pattern);
            if let Some(t) = &mut v.ty {
                rs_type(t);
            }
            rs_expr(&mut v.init);
        }
        StmtKind::Assign { target, value } => {
            rs_expr(target);
            rs_expr(value);
        }
        StmtKind::Expr(e) => rs_expr(e),
        // Nested item declarations inside a default body are not re-spanned
        // (they would re-enter collection); defaults in practice do not declare
        // local items.
        StmtKind::Item(_) => {}
    }
}

fn rs_pattern(p: &mut Pattern) {
    p.span = nsp();
    match &mut p.kind {
        PatternKind::Wildcard => {}
        PatternKind::Binding(i) => rs_ident(i),
        PatternKind::Literal(e) => rs_expr(e),
        PatternKind::TypeBinding { ty, binding } => {
            rs_type(ty);
            if let Some(i) = binding {
                rs_ident(i);
            }
        }
        PatternKind::UnitPath(tp) => rs_type_path(tp),
        PatternKind::TupleStruct { path, fields, rest } => {
            rs_type_path(path);
            for f in fields {
                rs_pattern(f);
            }
            if let Some(r) = rest {
                rs_rest(r);
            }
        }
        PatternKind::RecordStruct { path, fields, .. } => {
            rs_type_path(path);
            for fp in fields {
                rs_ident(&mut fp.name);
                fp.span = nsp();
                if let Some(sub) = &mut fp.pattern {
                    rs_pattern(sub);
                }
            }
        }
        PatternKind::Tuple { elems, rest } | PatternKind::List { elems, rest } => {
            for e in elems {
                rs_pattern(e);
            }
            if let Some((_, r)) = rest {
                rs_rest(r);
            }
        }
        PatternKind::Or(ps) => {
            for x in ps {
                rs_pattern(x);
            }
        }
    }
}

fn rs_rest(r: &mut RestPattern) {
    r.span = nsp();
    if let Some(n) = &mut r.name {
        rs_ident(n);
    }
}

fn rs_type_path(tp: &mut TypePath) {
    tp.span = nsp();
    rs_ident(&mut tp.name);
    for g in &mut tp.generics {
        rs_type(g);
    }
}

fn rs_expr(e: &mut Expr) {
    e.span = nsp();
    match &mut e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null
        | ExprKind::Char(_) | ExprKind::SelfExpr | ExprKind::Underscore
        | ExprKind::Continue => {}
        ExprKind::Ident(i) => rs_ident(i),
        ExprKind::Str(s) => {
            s.span = nsp();
            for part in &mut s.parts {
                match part {
                    StringPart::Text { span, .. } => *span = nsp(),
                    StringPart::Ident(i) => rs_ident(i),
                    StringPart::Expr(x) => rs_expr(x),
                }
            }
        }
        ExprKind::Tuple(es) | ExprKind::List(es) => {
            for x in es {
                rs_expr(x);
            }
        }
        ExprKind::Paren(x) => rs_expr(x),
        ExprKind::MapLit(items) => {
            for it in items {
                match it {
                    MapItem::Entry { key, value, span } => {
                        rs_expr(key);
                        rs_expr(value);
                        *span = nsp();
                    }
                    MapItem::Spread(x) => rs_expr(x),
                }
            }
        }
        ExprKind::StructLit { path, fields, spread } => {
            rs_type_path(path);
            for f in fields {
                rs_ident(&mut f.name);
                f.span = nsp();
                if let Some(v) = &mut f.value {
                    rs_expr(v);
                }
            }
            if let Some(s) = spread {
                rs_expr(s);
            }
        }
        ExprKind::Unary { operand, op_span, .. } => {
            *op_span = nsp();
            rs_expr(operand);
        }
        ExprKind::Binary { left, right, op_span, .. } => {
            *op_span = nsp();
            rs_expr(left);
            rs_expr(right);
        }
        ExprKind::Cast { expr, ty, op_span, .. } => {
            *op_span = nsp();
            rs_expr(expr);
            rs_type(ty);
        }
        ExprKind::Field { receiver, name } => {
            rs_expr(receiver);
            rs_ident(name);
        }
        ExprKind::TupleIndex { receiver, index_span, .. } => {
            rs_expr(receiver);
            *index_span = nsp();
        }
        ExprKind::Call { callee, generics, args, trailing_closure } => {
            rs_expr(callee);
            for g in generics {
                rs_type(g);
            }
            for a in args {
                rs_expr(a);
            }
            if let Some(tc) = trailing_closure {
                rs_expr(tc);
            }
        }
        ExprKind::Index { receiver, index } => {
            rs_expr(receiver);
            rs_expr(index);
        }
        ExprKind::Try { expr, q_span } => {
            rs_expr(expr);
            *q_span = nsp();
        }
        ExprKind::Ref { expr, amp_span } => {
            rs_expr(expr);
            *amp_span = nsp();
        }
        ExprKind::Deref { expr, star_span } => {
            rs_expr(expr);
            *star_span = nsp();
        }
        ExprKind::Await { expr, kw_span } => {
            rs_expr(expr);
            *kw_span = nsp();
        }
        ExprKind::Spawn { expr, kw_span } => {
            rs_expr(expr);
            *kw_span = nsp();
        }
        ExprKind::If { cond, then_block, else_branch } => {
            rs_expr(cond);
            rs_block(then_block);
            if let Some(eb) = else_branch {
                match eb {
                    ElseBranch::If(x) => rs_expr(x),
                    ElseBranch::Block(b) => rs_block(b),
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            rs_expr(scrutinee);
            for arm in arms {
                arm.span = nsp();
                rs_pattern(&mut arm.pattern);
                if let Some(g) = &mut arm.guard {
                    rs_expr(g);
                }
                rs_expr(&mut arm.body);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => rs_block(b),
        ExprKind::While { cond, body } => {
            rs_expr(cond);
            rs_block(body);
        }
        ExprKind::For { pattern, iter, body, .. } => {
            rs_pattern(pattern);
            rs_expr(iter);
            rs_block(body);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                rs_expr(x);
            }
        }
        ExprKind::Closure { params, return_type, body, .. } => {
            for p in params {
                p.span = nsp();
                rs_ident(&mut p.name);
                if let Some(t) = &mut p.ty {
                    rs_type(t);
                }
            }
            if let Some(t) = return_type {
                rs_type(t);
            }
            rs_expr(body);
        }
        ExprKind::AnonFn(f) => rs_function(f),
        ExprKind::AsyncBlock(b) => rs_block(b),
    }
}
