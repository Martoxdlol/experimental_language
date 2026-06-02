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
//! ## Scope
//!
//! Three forms are realised, all through the same body-copy machinery:
//!
//! * **Same-module, non-generic** — `extend Foo: Named` where `Named` is
//!   declared in this module. The default body is copied verbatim.
//! * **Generic interfaces** — `extend Foo: Bounded<i32>` where
//!   `interface Bounded<T> { … function max(self): T { … } }`. The interface's
//!   type parameters are substituted with the `extend`'s interface arguments
//!   (`T` → `i32`) in the copied signature *and* body before re-spanning, so the
//!   resulting method is fully concrete and reuses the ordinary checker path.
//! * **Cross-module interfaces** — `extend Foo: Named` where `Named` is a
//!   `pub` interface imported from another module. A program-wide index of every
//!   `pub` interface (built by [`crate::sema::analyze_multi_ctx`]) is consulted
//!   when a name is not declared locally. A name that resolves ambiguously across
//!   modules is left for normal resolution to diagnose (no default is copied).
//!
//! `Self` always resolves to the `extend`'s target type for free (the copied
//! body is checked in the implementer's context), so no `Self` substitution is
//! needed here.

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

/// A program-wide index of `pub` interfaces by name, used to resolve a
/// cross-module interface reference in an `extend` to its declaration so its
/// default bodies can be copied. A name that maps to two *structurally distinct*
/// `pub` interfaces is **ambiguous**: it is recorded as `None` and never used
/// (the conflict is left for name resolution to report), so a wrong default is
/// never silently copied.
#[derive(Default, Clone)]
pub struct ForeignIfaces {
    by_name: HashMap<String, Option<InterfaceItem>>,
}

impl ForeignIfaces {
    /// Record a `pub` interface, marking the name ambiguous if a different
    /// interface already claimed it.
    pub fn insert(&mut self, iface: InterfaceItem) {
        match self.by_name.get(&iface.name.name) {
            None => {
                self.by_name.insert(iface.name.name.clone(), Some(iface));
            }
            Some(Some(existing)) if *existing == iface => {} // identical re-export
            Some(_) => {
                // A conflicting (or already-ambiguous) name: poison it.
                self.by_name.insert(iface.name.name.clone(), None);
            }
        }
    }

    fn get(&self, name: &str) -> Option<&InterfaceItem> {
        self.by_name.get(name).and_then(|o| o.as_ref())
    }

    /// Merge every entry of `self` into `other`, preserving ambiguity: an
    /// already-ambiguous name (or one that conflicts on merge) stays ambiguous.
    pub fn extend_into(&self, other: &mut ForeignIfaces) {
        for (name, entry) in &self.by_name {
            match entry {
                Some(iface) => other.insert(iface.clone()),
                None => {
                    other.by_name.insert(name.clone(), None);
                }
            }
        }
    }
}

/// Collect every `pub` interface declared in `module` (recursively through
/// inline submodules) into `out`. The basis for the cross-module index.
pub fn collect_pub_interfaces(module: &Module, out: &mut ForeignIfaces) {
    for item in &module.items {
        match &item.kind {
            ItemKind::Interface(i) if item.visibility.is_public() => out.insert(i.clone()),
            ItemKind::Module(ModuleItem {
                kind: ModuleKind::Inline { items, .. },
                ..
            }) => {
                let sub = Module {
                    inner_docs: Vec::new(),
                    items: items.clone(),
                    span: item.span,
                };
                collect_pub_interfaces(&sub, out);
            }
            _ => {}
        }
    }
}

/// Expand interface default methods into implementing `extend` blocks across
/// `module` (recursively through inline submodules). `foreign` supplies `pub`
/// interfaces declared in *other* modules so cross-module defaults resolve; an
/// interface declared locally always shadows a foreign one of the same name.
pub fn expand_default_methods(module: &mut Module, foreign: &ForeignIfaces) {
    // Index every interface declared in this module by name (generic or not).
    let mut ifaces: HashMap<String, InterfaceItem> = HashMap::new();
    for item in &module.items {
        if let ItemKind::Interface(i) = &item.kind {
            ifaces.insert(i.name.name.clone(), i.clone());
        }
    }

    for item in &mut module.items {
        if let ItemKind::Extend(e) = &mut item.kind {
            let mut have: HashSet<String> = e
                .members
                .iter()
                .map(|m| m.function.name.name.clone())
                .collect();
            let mut additions: Vec<ExtendMember> = Vec::new();
            for iface_ty in &e.interfaces {
                let TypeKind::Named { name, generics } = &iface_ty.kind else {
                    continue;
                };
                // Local declarations shadow imported ones (`docs/17`).
                let Some(iface) = ifaces.get(&name.name).or_else(|| foreign.get(&name.name)) else {
                    continue;
                };
                // Map the interface's type parameters to this `extend`'s
                // interface arguments (`Bounded<T>` impl'd as `Bounded<i32>`
                // ⇒ `T` → `i32`). A non-generic interface yields an empty map.
                let iface_subst = iface_param_subst(iface, generics);
                for m in &iface.members {
                    let Some(body) = &m.default_body else {
                        continue;
                    };
                    if have.contains(&m.function.name.name) {
                        continue; // overridden by the impl
                    }
                    have.insert(m.function.name.name.clone());
                    additions.push(synth_member(m, body, &iface_subst));
                }
            }
            e.members.extend(additions);
        }
    }

    for item in &mut module.items {
        if let ItemKind::Module(ModuleItem {
            kind: ModuleKind::Inline { items, .. },
            ..
        }) = &mut item.kind
        {
            let mut sub = Module {
                inner_docs: Vec::new(),
                items: std::mem::take(items),
                span: item.span,
            };
            expand_default_methods(&mut sub, foreign);
            *items = sub.items;
        }
    }
}

/// Build the interface-type-parameter substitution for a particular `extend`
/// reference: each declared interface generic name mapped to the corresponding
/// argument written in `extend Foo: Iface<Arg0, …>`. Arity mismatches (which the
/// checker reports) zip to the shorter length and leave the rest unsubstituted.
fn iface_param_subst(iface: &InterfaceItem, args: &[Type]) -> HashMap<String, Type> {
    let mut subst = HashMap::new();
    if let Some(generics) = &iface.generics {
        for (p, a) in generics.params.iter().zip(args.iter()) {
            subst.insert(p.name.name.clone(), a.clone());
        }
    }
    subst
}

/// Build an `extend` member from an interface default method + body, applying
/// the interface's type-parameter substitution, then re-spanning.
fn synth_member(
    m: &InterfaceMember,
    body: &Block,
    iface_subst: &HashMap<String, Type>,
) -> ExtendMember {
    let mut f = FunctionItem {
        name: m.function.name.clone(),
        generics: m.function.generics.clone(),
        params: m.function.params.clone(),
        return_type: m.function.return_type.clone(),
        is_async: m.function.is_async,
        body: Some(body.clone()),
    };
    if !iface_subst.is_empty() {
        // The method's own generics shadow interface parameters of the same
        // name and must not be substituted.
        let mut shadow: HashSet<String> = HashSet::new();
        if let Some(g) = &f.generics {
            for p in &g.params {
                shadow.insert(p.name.name.clone());
            }
        }
        subst_function_types(&mut f, iface_subst, &shadow);
    }
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
// Interface type-parameter substitution: replace each bare `Named { name, [] }`
// whose name is an interface parameter (and not shadowed by a method-local
// generic) with the concrete argument type. Runs before re-spanning; the new
// nodes pick up fresh spans in the `rs_*` pass.
// ---------------------------------------------------------------------------

fn subst_function_types(
    f: &mut FunctionItem,
    subst: &HashMap<String, Type>,
    shadow: &HashSet<String>,
) {
    for p in &mut f.params {
        if let ParamKind::Normal { ty, .. } = &mut p.kind {
            subst_type(ty, subst, shadow);
        }
    }
    if let Some(t) = &mut f.return_type {
        subst_type(t, subst, shadow);
    }
    if let Some(b) = &mut f.body {
        subst_block(b, subst, shadow);
    }
}

fn subst_type(t: &mut Type, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    match &mut t.kind {
        TypeKind::Named { name, generics } => {
            if generics.is_empty() && !shadow.contains(&name.name) {
                if let Some(repl) = subst.get(&name.name) {
                    // Replace the whole node; do not recurse into the
                    // substitute (it may legitimately reference the `extend`'s
                    // own generics, resolved later by monomorphization).
                    *t = repl.clone();
                    return;
                }
            }
            for g in generics {
                subst_type(g, subst, shadow);
            }
        }
        TypeKind::Tuple(ts) | TypeKind::Union(ts) => {
            for x in ts {
                subst_type(x, subst, shadow);
            }
        }
        TypeKind::Function { params, ret } => {
            for p in params {
                subst_type(p, subst, shadow);
            }
            subst_type(ret, subst, shadow);
        }
        TypeKind::ExternFunction { params, ret } => {
            for p in params {
                subst_type(&mut p.ty, subst, shadow);
            }
            subst_type(ret, subst, shadow);
        }
        TypeKind::Pointer(inner) | TypeKind::Paren(inner) => subst_type(inner, subst, shadow),
        TypeKind::Array { elem, len } => {
            subst_type(elem, subst, shadow);
            subst_expr(len, subst, shadow);
        }
        TypeKind::SelfType => {}
    }
}

fn subst_block(b: &mut Block, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    for s in &mut b.stmts {
        subst_stmt(s, subst, shadow);
    }
    if let Some(t) = &mut b.trailing {
        subst_expr(t, subst, shadow);
    }
}

fn subst_stmt(s: &mut Stmt, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    match &mut s.kind {
        StmtKind::Var(v) => {
            subst_pattern(&mut v.pattern, subst, shadow);
            if let Some(t) = &mut v.ty {
                subst_type(t, subst, shadow);
            }
            subst_expr(&mut v.init, subst, shadow);
        }
        StmtKind::Assign { target, value } => {
            subst_expr(target, subst, shadow);
            subst_expr(value, subst, shadow);
        }
        StmtKind::Expr(e) => subst_expr(e, subst, shadow),
        StmtKind::Item(_) => {}
    }
}

fn subst_pattern(p: &mut Pattern, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    match &mut p.kind {
        PatternKind::TypeBinding { ty, .. } => subst_type(ty, subst, shadow),
        PatternKind::UnitPath(tp) => subst_type_path(tp, subst, shadow),
        PatternKind::TupleStruct { path, fields, .. } => {
            subst_type_path(path, subst, shadow);
            for f in fields {
                subst_pattern(f, subst, shadow);
            }
        }
        PatternKind::RecordStruct { path, fields, .. } => {
            subst_type_path(path, subst, shadow);
            for fp in fields {
                if let Some(sub) = &mut fp.pattern {
                    subst_pattern(sub, subst, shadow);
                }
            }
        }
        PatternKind::Tuple { elems, .. } | PatternKind::List { elems, .. } => {
            for e in elems {
                subst_pattern(e, subst, shadow);
            }
        }
        PatternKind::Or(ps) => {
            for x in ps {
                subst_pattern(x, subst, shadow);
            }
        }
        PatternKind::Literal(e) => subst_expr(e, subst, shadow),
        PatternKind::Wildcard | PatternKind::Binding(_) => {}
    }
}

fn subst_type_path(tp: &mut TypePath, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    // A bare unit-path `T` whose name is an interface parameter cannot become a
    // type path (paths name nominal items); only its generic arguments carry
    // substitutable types.
    for g in &mut tp.generics {
        subst_type(g, subst, shadow);
    }
}

fn subst_expr(e: &mut Expr, subst: &HashMap<String, Type>, shadow: &HashSet<String>) {
    match &mut e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Char(_)
        | ExprKind::SelfExpr
        | ExprKind::Underscore
        | ExprKind::Continue
        | ExprKind::Ident(_) => {}
        ExprKind::Str(s) => {
            for part in &mut s.parts {
                if let StringPart::Expr(x) = part {
                    subst_expr(x, subst, shadow);
                }
            }
        }
        ExprKind::Tuple(es) | ExprKind::List(es) => {
            for x in es {
                subst_expr(x, subst, shadow);
            }
        }
        ExprKind::Paren(x) => subst_expr(x, subst, shadow),
        ExprKind::MapLit(items) => {
            for it in items {
                match it {
                    MapItem::Entry { key, value, .. } => {
                        subst_expr(key, subst, shadow);
                        subst_expr(value, subst, shadow);
                    }
                    MapItem::Spread(x) => subst_expr(x, subst, shadow),
                }
            }
        }
        ExprKind::StructLit {
            path,
            fields,
            spread,
        } => {
            subst_type_path(path, subst, shadow);
            for f in fields {
                if let Some(v) = &mut f.value {
                    subst_expr(v, subst, shadow);
                }
            }
            if let Some(s) = spread {
                subst_expr(s, subst, shadow);
            }
        }
        ExprKind::Unary { operand, .. } => subst_expr(operand, subst, shadow),
        ExprKind::Binary { left, right, .. } => {
            subst_expr(left, subst, shadow);
            subst_expr(right, subst, shadow);
        }
        ExprKind::Cast { expr, ty, .. } => {
            subst_expr(expr, subst, shadow);
            subst_type(ty, subst, shadow);
        }
        ExprKind::Field { receiver, .. } => subst_expr(receiver, subst, shadow),
        ExprKind::TupleIndex { receiver, .. } => subst_expr(receiver, subst, shadow),
        ExprKind::Call {
            callee,
            generics,
            args,
            trailing_closure,
        } => {
            subst_expr(callee, subst, shadow);
            for g in generics {
                subst_type(g, subst, shadow);
            }
            for a in args {
                subst_expr(a, subst, shadow);
            }
            if let Some(tc) = trailing_closure {
                subst_expr(tc, subst, shadow);
            }
        }
        ExprKind::Index { receiver, index } => {
            subst_expr(receiver, subst, shadow);
            subst_expr(index, subst, shadow);
        }
        ExprKind::Try { expr, .. }
        | ExprKind::Ref { expr, .. }
        | ExprKind::Deref { expr, .. }
        | ExprKind::Await { expr, .. }
        | ExprKind::Spawn { expr, .. } => subst_expr(expr, subst, shadow),
        ExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            subst_expr(cond, subst, shadow);
            subst_block(then_block, subst, shadow);
            if let Some(eb) = else_branch {
                match eb {
                    ElseBranch::If(x) => subst_expr(x, subst, shadow),
                    ElseBranch::Block(b) => subst_block(b, subst, shadow),
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            subst_expr(scrutinee, subst, shadow);
            for arm in arms {
                subst_pattern(&mut arm.pattern, subst, shadow);
                if let Some(g) = &mut arm.guard {
                    subst_expr(g, subst, shadow);
                }
                subst_expr(&mut arm.body, subst, shadow);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => subst_block(b, subst, shadow),
        ExprKind::While { cond, body } => {
            subst_expr(cond, subst, shadow);
            subst_block(body, subst, shadow);
        }
        ExprKind::For {
            pattern,
            iter,
            body,
            ..
        } => {
            subst_pattern(pattern, subst, shadow);
            subst_expr(iter, subst, shadow);
            subst_block(body, subst, shadow);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                subst_expr(x, subst, shadow);
            }
        }
        ExprKind::Closure {
            params,
            return_type,
            body,
            ..
        } => {
            for p in params {
                if let Some(t) = &mut p.ty {
                    subst_type(t, subst, shadow);
                }
            }
            if let Some(t) = return_type {
                subst_type(t, subst, shadow);
            }
            subst_expr(body, subst, shadow);
        }
        ExprKind::AnonFn(f) => subst_function_types(f, subst, shadow),
        ExprKind::AsyncBlock(b) => subst_block(b, subst, shadow),
        ExprKind::MacroCall { args, block, .. } => {
            for arg in args {
                match arg {
                    AttrArg::Positional(e) => subst_expr(e, subst, shadow),
                    AttrArg::Named { value, .. } => subst_expr(value, subst, shadow),
                }
            }
            if let Some(b) = block {
                subst_block(b, subst, shadow);
            }
        }
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
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::Char(_)
        | ExprKind::SelfExpr
        | ExprKind::Underscore
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
        ExprKind::StructLit {
            path,
            fields,
            spread,
        } => {
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
        ExprKind::Unary {
            operand, op_span, ..
        } => {
            *op_span = nsp();
            rs_expr(operand);
        }
        ExprKind::Binary {
            left,
            right,
            op_span,
            ..
        } => {
            *op_span = nsp();
            rs_expr(left);
            rs_expr(right);
        }
        ExprKind::Cast {
            expr, ty, op_span, ..
        } => {
            *op_span = nsp();
            rs_expr(expr);
            rs_type(ty);
        }
        ExprKind::Field { receiver, name } => {
            rs_expr(receiver);
            rs_ident(name);
        }
        ExprKind::TupleIndex {
            receiver,
            index_span,
            ..
        } => {
            rs_expr(receiver);
            *index_span = nsp();
        }
        ExprKind::Call {
            callee,
            generics,
            args,
            trailing_closure,
        } => {
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
        ExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
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
        ExprKind::For {
            pattern,
            iter,
            body,
            ..
        } => {
            rs_pattern(pattern);
            rs_expr(iter);
            rs_block(body);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                rs_expr(x);
            }
        }
        ExprKind::Closure {
            params,
            return_type,
            body,
            ..
        } => {
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
        // Normally eliminated by macro expansion before this pass; recurse into
        // arguments/block so a survivor still gets consistent unique spans.
        ExprKind::MacroCall { args, block, .. } => {
            for arg in args {
                match arg {
                    AttrArg::Positional(e) => rs_expr(e),
                    AttrArg::Named { value, .. } => rs_expr(value),
                }
            }
            if let Some(b) = block {
                rs_block(b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn parse_module(src: &str) -> Module {
        let (tokens, lerrs) = lex(src, FileId(0));
        assert!(lerrs.is_empty(), "lex errors: {lerrs:?}");
        let (module, perrs) = parse(src, &tokens);
        assert!(perrs.is_empty(), "parse errors: {perrs:?}");
        module
    }

    /// The members of the first `extend` block in `module`, by method name.
    fn extend_members(module: &Module) -> std::collections::HashMap<String, FunctionItem> {
        for item in &module.items {
            if let ItemKind::Extend(e) = &item.kind {
                return e
                    .members
                    .iter()
                    .map(|m| (m.function.name.name.clone(), m.function.clone()))
                    .collect();
            }
        }
        panic!("no extend block");
    }

    fn type_name(t: &Type) -> &str {
        match &t.kind {
            TypeKind::Named { name, .. } => &name.name,
            _ => panic!("not a named type"),
        }
    }

    #[test]
    fn nongeneric_same_module_default_is_copied() {
        let mut m = parse_module(
            "interface Named { function name(self): str; function greet(self): str { self.name() } }\n\
             struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }",
        );
        expand_default_methods(&mut m, &ForeignIfaces::default());
        let members = extend_members(&m);
        assert!(
            members.contains_key("greet"),
            "default `greet` not copied: {:?}",
            members.keys()
        );
        assert!(members.contains_key("name"));
    }

    #[test]
    fn overridden_default_is_not_copied() {
        let mut m = parse_module(
            "interface Named { function name(self): str; function greet(self): str { self.name() } }\n\
             struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } function greet(self): str { \"hi\" } }",
        );
        expand_default_methods(&mut m, &ForeignIfaces::default());
        let members = extend_members(&m);
        // `greet` exists exactly once — the user's, not a duplicate synthesized one.
        let greet_count = {
            let mut n = 0;
            for item in &m.items {
                if let ItemKind::Extend(e) = &item.kind {
                    n += e
                        .members
                        .iter()
                        .filter(|x| x.function.name.name == "greet")
                        .count();
                }
            }
            n
        };
        assert_eq!(greet_count, 1, "override must not be shadowed by a copy");
        assert!(members.contains_key("greet"));
    }

    #[test]
    fn generic_interface_default_substitutes_parameter() {
        let mut m = parse_module(
            "interface Boxed<T> { function get(self): T; function dup(self): T { self.get() } }\n\
             struct Cell { v: i64 }\n\
             extend Cell: Boxed<i64> { function get(self): i64 { self.v } }",
        );
        expand_default_methods(&mut m, &ForeignIfaces::default());
        let members = extend_members(&m);
        let dup = members.get("dup").expect("default `dup` not copied");
        // The return type `T` must have been substituted to `i64`.
        assert_eq!(type_name(dup.return_type.as_ref().unwrap()), "i64");
    }

    #[test]
    fn generic_default_substitutes_in_local_binding() {
        let mut m = parse_module(
            "interface Wrap<T> { function u(self): T; function e(self): T { var x: T = self.u(); x } }\n\
             struct N { n: i64 }\n\
             extend N: Wrap<i64> { function u(self): i64 { self.n } }",
        );
        expand_default_methods(&mut m, &ForeignIfaces::default());
        let members = extend_members(&m);
        let e = members.get("e").expect("default `e` not copied");
        // The `var x: T` annotation inside the copied body must be substituted.
        let body = e.body.as_ref().unwrap();
        let var_ty = body.stmts.iter().find_map(|s| match &s.kind {
            StmtKind::Var(v) => v.ty.clone(),
            _ => None,
        });
        assert_eq!(type_name(&var_ty.expect("no typed var")), "i64");
    }

    #[test]
    fn cross_module_default_resolved_via_foreign_index() {
        // The interface is *not* declared in this module; it comes from `foreign`.
        let mut m = parse_module(
            "struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }",
        );
        let iface = parse_module(
            "pub interface Named { function name(self): str; function greet(self): str { self.name() } }",
        );
        let mut foreign = ForeignIfaces::default();
        collect_pub_interfaces(&iface, &mut foreign);
        expand_default_methods(&mut m, &foreign);
        let members = extend_members(&m);
        assert!(
            members.contains_key("greet"),
            "cross-module default not copied"
        );
    }

    #[test]
    fn local_interface_shadows_foreign_of_same_name() {
        // A locally-declared interface wins over a foreign one with the same name.
        let mut m = parse_module(
            "interface Named { function name(self): str; function greet(self): str { \"local\" } }\n\
             struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }",
        );
        let other = parse_module(
            "pub interface Named { function name(self): str; function greet(self): str { \"foreign\" } }",
        );
        let mut foreign = ForeignIfaces::default();
        collect_pub_interfaces(&other, &mut foreign);
        expand_default_methods(&mut m, &foreign);
        let members = extend_members(&m);
        let greet = members.get("greet").expect("default not copied");
        // The local default body (a `"local"` string literal trailing expr) was used.
        let trailing = greet.body.as_ref().unwrap().trailing.as_ref().unwrap();
        match &trailing.kind {
            ExprKind::Str(s) => match &s.parts[0] {
                StringPart::Text { text, .. } => assert_eq!(text, "local"),
                _ => panic!("unexpected string part"),
            },
            _ => panic!("expected a string literal trailing expr"),
        }
    }

    #[test]
    fn ambiguous_foreign_name_is_not_copied() {
        // Two structurally-distinct `pub` interfaces share a name across modules:
        // the name is poisoned, so no default is copied (left for resolution to
        // diagnose) — never a silently-wrong body.
        let mut foreign = ForeignIfaces::default();
        let a = parse_module(
            "pub interface Named { function name(self): str; function greet(self): str { \"a\" } }",
        );
        let b = parse_module(
            "pub interface Named { function name(self): str; function greet(self): str { \"b\" } }",
        );
        collect_pub_interfaces(&a, &mut foreign);
        collect_pub_interfaces(&b, &mut foreign);
        assert!(
            foreign.get("Named").is_none(),
            "conflicting name must be ambiguous"
        );

        let mut m = parse_module(
            "struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }",
        );
        expand_default_methods(&mut m, &foreign);
        let members = extend_members(&m);
        assert!(
            !members.contains_key("greet"),
            "ambiguous default must not be copied"
        );
    }

    #[test]
    fn identical_foreign_reexport_is_not_ambiguous() {
        // The same interface seen twice (e.g. via two import paths) is fine.
        let mut foreign = ForeignIfaces::default();
        let a = parse_module(
            "pub interface Named { function name(self): str; function greet(self): str { self.name() } }",
        );
        let b = a.clone();
        collect_pub_interfaces(&a, &mut foreign);
        collect_pub_interfaces(&b, &mut foreign);
        assert!(
            foreign.get("Named").is_some(),
            "identical re-export must resolve"
        );
    }
}
