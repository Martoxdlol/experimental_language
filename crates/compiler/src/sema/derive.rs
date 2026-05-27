//! `@Derive(...)` expansion (`docs/22` §11).
//!
//! A built-in derive is a source-level desugaring: for a `struct` annotated
//! with `@Derive(Eq)` (etc.) the compiler synthesises an `extend` block
//! implementing the corresponding methods field-by-field, then feeds the
//! augmented module through the normal collect/check/codegen pipeline. Nothing
//! downstream knows the impl was generated — `==` resolves to the synthesised
//! `eq` exactly as it would a hand-written one.
//!
//! Currently supported: `Eq` (field-by-field equality; `!=` comes free via the
//! operator machinery negating `eq`). `Ord`/`Hash`/`Clone`/`ToStr` are staged.

use crate::ast::*;
use crate::span::{BytePos, FileId, Span};
use std::sync::atomic::{AtomicU32, Ordering};

/// Synthesised nodes live in a dedicated virtual file so their spans never
/// collide with real source — and, crucially, so each generated node gets a
/// *unique* span (the checker keys its side tables, like expression types, by
/// span; duplicate spans would clobber each other).
const DERIVE_FILE: FileId = FileId(u32::MAX - 1);
static SPAN_CTR: AtomicU32 = AtomicU32::new(0);

/// A fresh, unique span in the synthetic derive file.
fn nsp() -> Span {
    let n = SPAN_CTR.fetch_add(1, Ordering::Relaxed);
    Span::new(DERIVE_FILE, BytePos(n), BytePos(n + 1))
}

/// Expand every `@Derive(...)` on a struct in `module` (recursively through
/// inline submodules) into synthesised `extend` items appended to that module.
pub fn expand_derives(module: &mut Module) {
    let mut generated = Vec::new();
    for item in &module.items {
        if let ItemKind::Struct(s) = &item.kind {
            let derives = derived_interfaces(&item.attrs);
            if !derives.is_empty() {
                if let Some(ext) = synth_impl(s, &derives) {
                    generated.push(ext);
                }
            }
        }
    }
    module.items.extend(generated);
    // Inline submodules are expanded in place.
    for item in &mut module.items {
        if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) =
            &mut item.kind
        {
            let mut sub = Module { inner_docs: Vec::new(), items: std::mem::take(items), span: item.span };
            expand_derives(&mut sub);
            *items = sub.items;
        }
    }
}

/// The set of built-in derivable interfaces named in a struct's attributes.
fn derived_interfaces(attrs: &[Attribute]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for a in attrs {
        // Accept `@Derive(..)` (the canonical UpperCamelCase macro name) and the
        // `@derive(..)` spelling used in some docs.
        if a.name.name != "Derive" && a.name.name != "derive" {
            continue;
        }
        for arg in &a.args {
            if let AttrArg::Positional(Expr { kind: ExprKind::Ident(id), .. }) = arg {
                if let Some(known) = known_derive(&id.name) {
                    out.push(known);
                }
            }
        }
    }
    out
}

fn known_derive(name: &str) -> Option<&'static str> {
    match name {
        "Eq" => Some("Eq"),
        "Ord" => Some("Ord"),
        "ToStr" => Some("ToStr"),
        "Clone" => Some("Clone"),
        _ => None, // Hash: staged.
    }
}

/// Synthesise one `extend` block implementing all `derives` for struct `s`,
/// deduping methods (e.g. `Ord` implies `Eq`, so `eq` is emitted once even with
/// `@Derive(Eq, Ord)`). Returns `None` for cases not yet handled (generic
/// structs).
fn synth_impl(s: &StructItem, derives: &[&str]) -> Option<Item> {
    // Generic structs need a generic `extend` with bounds; staged.
    if s.generics.is_some() {
        return None;
    }
    let want_eq = derives.contains(&"Eq") || derives.contains(&"Ord");
    let want_ord = derives.contains(&"Ord");
    let want_to_str = derives.contains(&"ToStr");
    let want_clone = derives.contains(&"Clone");

    let mut functions: Vec<FunctionItem> = Vec::new();
    if want_eq {
        functions.push(synth_eq(s));
    }
    if want_ord {
        // `<`/`<=`/`>`/`>=` resolve to distinct methods; synthesise all four.
        let name = &s.name.name;
        functions.push(synth_lt(s));
        functions.push(synth_le(name));
        functions.push(synth_gt(name));
        functions.push(synth_ge(name));
    }
    if want_to_str {
        functions.push(synth_to_str(s));
    }
    if want_clone {
        functions.push(synth_clone(s));
    }
    if functions.is_empty() {
        return None;
    }

    let members = functions
        .into_iter()
        .map(|function| ExtendMember {
            docs: Vec::new(),
            attrs: Vec::new(),
            visibility: Visibility::Public(nsp()),
            function,
            span: nsp(),
        })
        .collect();
    // `Eq`/`Ord`/`ToStr` resolve by operator/name, so they need no interface
    // declaration. `Clone`, however, is dispatched through `T: Clone` bounds and
    // interface objects, so the synthesised impl must declare it (populating the
    // `(type, Clone)` impl table the monomorphizer consults).
    let interfaces = if want_clone { vec![named_ty("Clone")] } else { Vec::new() };
    Some(Item {
        docs: Vec::new(),
        attrs: Vec::new(),
        visibility: Visibility::Private,
        kind: ItemKind::Extend(ExtendItem {
            generics: None,
            target: named_ty(&s.name.name),
            interfaces,
            members,
        }),
        span: nsp(),
    })
}

/// A `function <name>(self, other: S): bool { <body> }` comparison method.
fn cmp_method(name: &str, struct_name: &str, body: Expr) -> FunctionItem {
    FunctionItem {
        name: ident(name),
        generics: None,
        params: vec![
            Param { kind: ParamKind::SelfParam, span: nsp() },
            Param {
                kind: ParamKind::Normal { name: ident("other"), ty: named_ty(struct_name) },
                span: nsp(),
            },
        ],
        return_type: Some(named_ty("bool")),
        is_async: false,
        body: Some(Block { stmts: Vec::new(), trailing: Some(Box::new(body)), span: nsp() }),
    }
}

/// `eq`: `self.f0 == other.f0 && …` (field-by-field). Unit/empty → `true`.
fn synth_eq(s: &StructItem) -> FunctionItem {
    let body = conjoin(field_cmps(s, BinaryOp::Eq).into_iter());
    cmp_method("eq", &s.name.name, body)
}

/// `lt`: lexicographic less-than by field declaration order (`docs/22` §11).
fn synth_lt(s: &StructItem) -> FunctionItem {
    let n = field_count(s);
    let body = lex_lt(s, 0, n);
    cmp_method("lt", &s.name.name, body)
}

/// `le`: `self.lt(other) || self.eq(other)`.
fn synth_le(struct_name: &str) -> FunctionItem {
    let body = binary(
        BinaryOp::Or,
        method_call(self_expr(), "lt", ident_expr("other")),
        method_call(self_expr(), "eq", ident_expr("other")),
    );
    cmp_method("le", struct_name, body)
}

/// `gt`: `!self.le(other)`.
fn synth_gt(struct_name: &str) -> FunctionItem {
    let body = not(method_call(self_expr(), "le", ident_expr("other")));
    cmp_method("gt", struct_name, body)
}

/// `ge`: `!self.lt(other)`.
fn synth_ge(struct_name: &str) -> FunctionItem {
    let body = not(method_call(self_expr(), "lt", ident_expr("other")));
    cmp_method("ge", struct_name, body)
}

/// Number of fields (0 for a unit struct).
fn field_count(s: &StructItem) -> usize {
    match &s.kind {
        StructKind::Unit => 0,
        StructKind::Record(f) => f.len(),
        StructKind::Tuple(f) => f.len(),
    }
}

/// `self.fi` for field `i` (record name or tuple index), freshly built.
fn self_field(s: &StructItem, i: usize) -> Expr {
    field_at(s, self_expr(), i)
}
/// `other.fi`.
fn other_field(s: &StructItem, i: usize) -> Expr {
    field_at(s, ident_expr("other"), i)
}
fn field_at(s: &StructItem, base: Expr, i: usize) -> Expr {
    match &s.kind {
        StructKind::Record(fields) => field_access(base, &fields[i].name.name),
        StructKind::Tuple(_) => tuple_index(base, i as u32),
        StructKind::Unit => unreachable!("unit struct has no fields"),
    }
}

/// All field comparisons `self.fi <op> other.fi`.
fn field_cmps(s: &StructItem, op: BinaryOp) -> Vec<Expr> {
    (0..field_count(s)).map(|i| binary(op, self_field(s, i), other_field(s, i))).collect()
}

/// Lexicographic `<` over fields `[i, n)`:
/// `self.fi < other.fi || (self.fi == other.fi && <rest>)`. Empty → `false`.
fn lex_lt(s: &StructItem, i: usize, n: usize) -> Expr {
    if i >= n {
        return bool_lit(false);
    }
    let lt_i = binary(BinaryOp::Lt, self_field(s, i), other_field(s, i));
    if i + 1 == n {
        return lt_i;
    }
    let eq_i = binary(BinaryOp::Eq, self_field(s, i), other_field(s, i));
    let rest = lex_lt(s, i + 1, n);
    binary(BinaryOp::Or, lt_i, binary(BinaryOp::And, eq_i, rest))
}

/// `function to_str(self): str { "S { f: " + (self.f as str) + … }` — a
/// debug-style rendering, concatenating literal text with each field cast to
/// `str` (`docs/22` §11). Fields must themselves be `as str`-stringifiable
/// (primitives and `str`); nested-struct fields await interpolation of derived
/// `to_str`, a follow-up.
fn synth_to_str(s: &StructItem) -> FunctionItem {
    let name = &s.name.name;
    let body = match &s.kind {
        StructKind::Unit => str_lit(name),
        StructKind::Record(fields) => {
            let mut pieces = vec![str_lit(&format!("{name} {{ "))];
            for (i, f) in fields.iter().enumerate() {
                let sep = if i == 0 { String::new() } else { ", ".to_string() };
                pieces.push(str_lit(&format!("{sep}{}: ", f.name.name)));
                pieces.push(cast_to_str(field_access(self_expr(), &f.name.name)));
            }
            pieces.push(str_lit(" }"));
            concat(pieces)
        }
        StructKind::Tuple(fields) => {
            let mut pieces = vec![str_lit(&format!("{name}("))];
            for i in 0..fields.len() as u32 {
                if i > 0 {
                    pieces.push(str_lit(", "));
                }
                pieces.push(cast_to_str(tuple_index(self_expr(), i)));
            }
            pieces.push(str_lit(")"));
            concat(pieces)
        }
    };
    FunctionItem {
        name: ident("to_str"),
        generics: None,
        params: vec![Param { kind: ParamKind::SelfParam, span: nsp() }],
        return_type: Some(named_ty("str")),
        is_async: false,
        body: Some(Block { stmts: Vec::new(), trailing: Some(Box::new(body)), span: nsp() }),
    }
}

/// `clone(self): Self` — a field-by-field deep copy (`docs/15` §8). Each field
/// is cloned via its own `.clone()` (primitives/`str` clone trivially; nested
/// structs recurse through their own `Clone` impl). The result is a freshly
/// constructed value of the same struct shape.
fn synth_clone(s: &StructItem) -> FunctionItem {
    let name = &s.name.name;
    let body = match &s.kind {
        StructKind::Unit => ident_expr(name),
        StructKind::Record(fields) => {
            let inits = fields
                .iter()
                .map(|f| FieldInit {
                    name: ident(&f.name.name),
                    value: Some(clone_call(field_access(self_expr(), &f.name.name))),
                    span: nsp(),
                })
                .collect();
            Expr {
                kind: ExprKind::StructLit {
                    path: TypePath { name: ident(name), generics: Vec::new(), span: nsp() },
                    fields: inits,
                    spread: None,
                },
                span: nsp(),
            }
        }
        StructKind::Tuple(fields) => {
            // A tuple struct is constructed by calling its name positionally.
            let args = (0..fields.len() as u32)
                .map(|i| clone_call(tuple_index(self_expr(), i)))
                .collect();
            Expr {
                kind: ExprKind::Call {
                    callee: Box::new(ident_expr(name)),
                    generics: Vec::new(),
                    args,
                    trailing_closure: None,
                },
                span: nsp(),
            }
        }
    };
    FunctionItem {
        name: ident("clone"),
        generics: None,
        params: vec![Param { kind: ParamKind::SelfParam, span: nsp() }],
        return_type: Some(named_ty(&s.name.name)),
        is_async: false,
        body: Some(Block { stmts: Vec::new(), trailing: Some(Box::new(body)), span: nsp() }),
    }
}

/// `receiver.clone()` — a zero-argument method call.
fn clone_call(receiver: Expr) -> Expr {
    let callee = Expr {
        kind: ExprKind::Field { receiver: Box::new(receiver), name: ident("clone") },
        span: nsp(),
    };
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(callee),
            generics: Vec::new(),
            args: Vec::new(),
            trailing_closure: None,
        },
        span: nsp(),
    }
}

/// Fold string-valued expressions with `+` (concatenation).
fn concat(parts: Vec<Expr>) -> Expr {
    let mut it = parts.into_iter();
    let first = it.next().expect("at least one piece");
    it.fold(first, |acc, p| binary(BinaryOp::Add, acc, p))
}

/// Fold comparison expressions with `&&`; an empty set is `true`.
fn conjoin(parts: impl Iterator<Item = Expr>) -> Expr {
    let mut acc: Option<Expr> = None;
    for p in parts {
        acc = Some(match acc {
            None => p,
            Some(prev) => binary(BinaryOp::And, prev, p),
        });
    }
    acc.unwrap_or_else(|| bool_lit(true))
}

// -- small AST builders (each node gets a unique synthetic span) --------------

fn ident(name: &str) -> Ident {
    Ident { name: name.to_string(), span: nsp() }
}
fn ident_expr(name: &str) -> Expr {
    Expr { kind: ExprKind::Ident(ident(name)), span: nsp() }
}
fn self_expr() -> Expr {
    Expr { kind: ExprKind::SelfExpr, span: nsp() }
}
fn bool_lit(b: bool) -> Expr {
    Expr { kind: ExprKind::Bool(b), span: nsp() }
}
fn field_access(receiver: Expr, name: &str) -> Expr {
    Expr { kind: ExprKind::Field { receiver: Box::new(receiver), name: ident(name) }, span: nsp() }
}
fn tuple_index(receiver: Expr, index: u32) -> Expr {
    Expr {
        kind: ExprKind::TupleIndex { receiver: Box::new(receiver), index, index_span: nsp() },
        span: nsp(),
    }
}
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr {
        kind: ExprKind::Binary { op, op_span: nsp(), left: Box::new(left), right: Box::new(right) },
        span: nsp(),
    }
}
fn not(operand: Expr) -> Expr {
    Expr {
        kind: ExprKind::Unary { op: UnaryOp::Not, op_span: nsp(), operand: Box::new(operand) },
        span: nsp(),
    }
}
/// `receiver.<name>(arg)` — a single-argument method call.
fn method_call(receiver: Expr, name: &str, arg: Expr) -> Expr {
    let callee = Expr {
        kind: ExprKind::Field { receiver: Box::new(receiver), name: ident(name) },
        span: nsp(),
    };
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(callee),
            generics: Vec::new(),
            args: vec![arg],
            trailing_closure: None,
        },
        span: nsp(),
    }
}
fn named_ty(name: &str) -> Type {
    Type { kind: TypeKind::Named { name: ident(name), generics: Vec::new() }, span: nsp() }
}
fn str_lit(text: &str) -> Expr {
    let sp = nsp();
    let lit = StringLit { parts: vec![StringPart::Text { text: text.to_string(), span: sp }], span: sp };
    Expr { kind: ExprKind::Str(lit), span: nsp() }
}
fn cast_to_str(expr: Expr) -> Expr {
    Expr {
        kind: ExprKind::Cast {
            op: CastOp::As,
            op_span: nsp(),
            expr: Box::new(expr),
            ty: Box::new(named_ty("str")),
        },
        span: nsp(),
    }
}
