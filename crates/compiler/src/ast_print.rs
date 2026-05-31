//! AST source-printer.
//!
//! Renders an [`ast::Module`] (or any sub-node) back to Otter Fusion source
//! text. The output is *correct* — it re-parses to an equivalent AST — but
//! normalized rather than byte-identical to the input: canonical four-space
//! indentation, one statement per line, and conservative parentheses around
//! operator sub-expressions so precedence can never change meaning.
//!
//! Two consumers rely on this module:
//!   * `otter_fusion expand` — print a program after parsing (the AST the rest
//!     of the compiler actually sees), useful for inspecting desugaring and for
//!     teaching the surface syntax.
//!   * the round-trip invariant in the tests: `print(parse(s))` re-parses, and
//!     printing *that* AST again is identical (idempotence). The printer is
//!     written to satisfy this — e.g. ambiguous heads are wrapped in parens that
//!     survive as `Paren`/`Tuple` nodes and so are not wrapped a second time.
//!
//! The printer never consults semantic information; it works purely on the
//! syntactic AST and is therefore safe to run on any parse result, error or not.

use crate::ast::*;
use crate::token::IntBase;

/// Render a whole module back to source text (with a trailing newline).
pub fn print_module(m: &Module) -> String {
    let mut p = Printer::new();
    p.module(m);
    if !p.out.ends_with('\n') {
        p.out.push('\n');
    }
    p.out
}

/// Render a single expression — handy for tests and diagnostics.
pub fn print_expr(e: &Expr) -> String {
    let mut p = Printer::new();
    p.expr(e);
    p.out
}

/// Render a single item.
pub fn print_item(it: &Item) -> String {
    let mut p = Printer::new();
    p.item(it);
    p.out
}

/// Render a single statement (used by the macro host to echo block contents).
pub fn print_stmt(s: &Stmt) -> String {
    let mut p = Printer::new();
    p.stmt(s);
    p.out
}

/// Render a block (`{ … }`), braces included.
pub fn print_block(b: &Block) -> String {
    let mut p = Printer::new();
    p.block(b);
    p.out
}

struct Printer {
    out: String,
    indent: usize,
}

impl Printer {
    fn new() -> Self {
        Printer { out: String::new(), indent: 0 }
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// Start a fresh line at the current indentation.
    fn nl(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    // =======================================================================
    // Module / items
    // =======================================================================

    fn module(&mut self, m: &Module) {
        let mut first = true;
        for d in &m.inner_docs {
            if !first {
                self.nl();
            }
            self.push(d.text.trim_end());
            first = false;
        }
        for it in &m.items {
            if !first {
                self.nl();
                self.nl();
            }
            self.item(it);
            first = false;
        }
    }

    fn item(&mut self, it: &Item) {
        let mut wrote_leading = false;
        for d in &it.docs {
            if wrote_leading {
                self.nl();
            }
            self.push(d.text.trim_end());
            wrote_leading = true;
        }
        for a in &it.attrs {
            if wrote_leading {
                self.nl();
            }
            self.attr(a);
            wrote_leading = true;
        }
        if wrote_leading {
            self.nl();
        }
        if it.visibility.is_public() {
            self.push("pub ");
        }
        self.item_kind(&it.kind);
    }

    fn attr(&mut self, a: &Attribute) {
        self.push("@");
        self.push(&a.name.name);
        if !a.args.is_empty() {
            self.push("(");
            for (i, arg) in a.args.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                match arg {
                    AttrArg::Positional(e) => self.expr(e),
                    AttrArg::Named { name, value, .. } => {
                        self.push(&name.name);
                        self.push(" = ");
                        self.expr(value);
                    }
                }
            }
            self.push(")");
        }
    }

    fn item_kind(&mut self, k: &ItemKind) {
        match k {
            ItemKind::Var(v) => self.var_item(v),
            ItemKind::Function(f) => self.function_item(f),
            ItemKind::Struct(s) => self.struct_item(s),
            ItemKind::Interface(i) => self.interface_item(i),
            ItemKind::TypeAlias(t) => self.type_alias_item(t),
            ItemKind::Module(m) => self.module_item(m),
            ItemKind::Extend(e) => self.extend_item(e),
            ItemKind::Extern(e) => self.extern_item(e),
            ItemKind::Import(i) => self.import_item(i),
            ItemKind::Test(t) => self.test_item(t),
        }
    }

    fn var_item(&mut self, v: &VarItem) {
        self.push("var ");
        self.push(&v.name.name);
        if let Some(ty) = &v.ty {
            self.push(": ");
            self.ty(ty);
        }
        self.push(" = ");
        self.expr(&v.init);
        self.push(";");
    }

    fn function_item(&mut self, f: &FunctionItem) {
        self.push("function");
        if !f.name.name.is_empty() {
            self.push(" ");
            self.push(&f.name.name);
        }
        self.generic_params(&f.generics);
        self.push("(");
        self.params(&f.params);
        self.push(")");
        if let Some(rt) = &f.return_type {
            self.push(": ");
            self.ty(rt);
        }
        if f.is_async {
            self.push(" async");
        }
        match &f.body {
            Some(b) => {
                self.push(" ");
                self.block(b);
            }
            None => self.push(";"),
        }
    }

    fn params(&mut self, ps: &[Param]) {
        for (i, p) in ps.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            match &p.kind {
                ParamKind::SelfParam => self.push("self"),
                ParamKind::Normal { name, ty } => {
                    self.push(&name.name);
                    self.push(": ");
                    self.ty(ty);
                }
            }
        }
    }

    fn struct_item(&mut self, s: &StructItem) {
        self.push("struct ");
        self.push(&s.name.name);
        self.generic_params(&s.generics);
        match &s.kind {
            StructKind::Unit => self.push(";"),
            StructKind::Tuple(fields) => {
                self.push("(");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    if f.visibility.is_public() {
                        self.push("pub ");
                    }
                    self.ty(&f.ty);
                }
                self.push(");");
            }
            StructKind::Record(fields) => {
                if fields.is_empty() {
                    self.push(" {}");
                    return;
                }
                self.push(" {");
                self.indent += 1;
                for f in fields {
                    for d in &f.docs {
                        self.nl();
                        self.push(d.text.trim_end());
                    }
                    for a in &f.attrs {
                        self.nl();
                        self.attr(a);
                    }
                    self.nl();
                    if f.visibility.is_public() {
                        self.push("pub ");
                    }
                    self.push(&f.name.name);
                    self.push(": ");
                    self.ty(&f.ty);
                    self.push(",");
                }
                self.indent -= 1;
                self.nl();
                self.push("}");
            }
        }
    }

    fn interface_item(&mut self, i: &InterfaceItem) {
        self.push("interface ");
        self.push(&i.name.name);
        self.generic_params(&i.generics);
        if !i.supers.is_empty() {
            self.push(": ");
            self.type_join(&i.supers, " + ");
        }
        if i.members.is_empty() {
            self.push(" {}");
            return;
        }
        self.push(" {");
        self.indent += 1;
        for m in &i.members {
            for d in &m.docs {
                self.nl();
                self.push(d.text.trim_end());
            }
            for a in &m.attrs {
                self.nl();
                self.attr(a);
            }
            self.nl();
            self.function_sig(&m.function);
            match &m.default_body {
                Some(b) => {
                    self.push(" ");
                    self.block(b);
                }
                None => self.push(";"),
            }
        }
        self.indent -= 1;
        self.nl();
        self.push("}");
    }

    fn function_sig(&mut self, f: &FunctionSig) {
        self.push("function ");
        self.push(&f.name.name);
        self.generic_params(&f.generics);
        self.push("(");
        self.params(&f.params);
        self.push(")");
        if let Some(rt) = &f.return_type {
            self.push(": ");
            self.ty(rt);
        }
        if f.is_async {
            self.push(" async");
        }
    }

    fn type_alias_item(&mut self, t: &TypeAliasItem) {
        self.push("type ");
        self.push(&t.name.name);
        self.generic_params(&t.generics);
        self.push(" = ");
        self.ty(&t.aliased);
        self.push(";");
    }

    fn module_item(&mut self, m: &ModuleItem) {
        self.push("mod ");
        self.push(&m.name.name);
        match &m.kind {
            ModuleKind::External => self.push(";"),
            ModuleKind::Inline { inner_docs, items } => {
                if inner_docs.is_empty() && items.is_empty() {
                    self.push(" {}");
                    return;
                }
                self.push(" {");
                self.indent += 1;
                for d in inner_docs {
                    self.nl();
                    self.push(d.text.trim_end());
                }
                for it in items {
                    self.nl();
                    self.item(it);
                }
                self.indent -= 1;
                self.nl();
                self.push("}");
            }
        }
    }

    fn extend_item(&mut self, e: &ExtendItem) {
        self.push("extend");
        self.generic_params(&e.generics);
        self.push(" ");
        self.ty(&e.target);
        if !e.interfaces.is_empty() {
            self.push(": ");
            self.type_join(&e.interfaces, " + ");
        }
        if e.members.is_empty() {
            self.push(" {}");
            return;
        }
        self.push(" {");
        self.indent += 1;
        for m in &e.members {
            for d in &m.docs {
                self.nl();
                self.push(d.text.trim_end());
            }
            for a in &m.attrs {
                self.nl();
                self.attr(a);
            }
            self.nl();
            if m.visibility.is_public() {
                self.push("pub ");
            }
            self.function_item(&m.function);
        }
        self.indent -= 1;
        self.nl();
        self.push("}");
    }

    fn extern_item(&mut self, e: &ExternItem) {
        match e {
            ExternItem::Function(f) => {
                self.push("extern ");
                self.function_item(f);
            }
            ExternItem::Struct(s) => {
                self.push("extern ");
                self.struct_item(s);
            }
            ExternItem::OpaqueType(name) => {
                self.push("extern type ");
                self.push(&name.name);
                self.push(";");
            }
            ExternItem::Var { name, ty } => {
                self.push("extern var ");
                self.push(&name.name);
                self.push(": ");
                self.ty(ty);
                self.push(";");
            }
        }
    }

    fn import_item(&mut self, i: &ImportItem) {
        match &i.kind {
            ImportKind::Ambient => {
                self.push("import ");
                self.string_lit(&i.path);
                self.push(";");
            }
            ImportKind::Namespace(name) => {
                self.push("import ");
                self.string_lit(&i.path);
                self.push(" as ");
                self.push(&name.name);
                self.push(";");
            }
            ImportKind::Named(names) => {
                self.push("import { ");
                for (j, n) in names.iter().enumerate() {
                    if j > 0 {
                        self.push(", ");
                    }
                    self.push(&n.name.name);
                    if let Some(alias) = &n.alias {
                        self.push(" as ");
                        self.push(&alias.name);
                    }
                }
                self.push(" } from ");
                self.string_lit(&i.path);
                self.push(";");
            }
        }
    }

    fn test_item(&mut self, t: &TestItem) {
        self.push(if t.is_bench { "bench " } else { "test " });
        self.push("\"");
        for ch in t.name.chars() {
            match ch {
                '"' => self.push("\\\""),
                '\\' => self.push("\\\\"),
                _ => self.out.push(ch),
            }
        }
        self.push("\" ");
        self.block(&t.body);
    }

    // =======================================================================
    // Generics
    // =======================================================================

    fn generic_params(&mut self, g: &Option<GenericParams>) {
        let Some(g) = g else { return };
        if g.params.is_empty() {
            return;
        }
        self.push("<");
        for (i, p) in g.params.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.push(&p.name.name);
            if !p.bounds.is_empty() {
                self.push(": ");
                self.type_join(&p.bounds, " + ");
            }
            if let Some(def) = &p.default {
                self.push(" = ");
                self.ty(def);
            }
        }
        self.push(">");
    }

    /// `<T1, T2>` for a plain type-argument list; nothing if empty.
    fn type_args(&mut self, args: &[Type]) {
        if args.is_empty() {
            return;
        }
        self.push("<");
        self.type_join(args, ", ");
        self.push(">");
    }

    fn type_join(&mut self, types: &[Type], sep: &str) {
        for (i, t) in types.iter().enumerate() {
            if i > 0 {
                self.push(sep);
            }
            self.ty(t);
        }
    }

    // =======================================================================
    // Types
    // =======================================================================

    fn ty(&mut self, t: &Type) {
        match &t.kind {
            TypeKind::Named { name, generics } => {
                self.push(&name.name);
                self.type_args(generics);
            }
            TypeKind::Tuple(elems) => {
                self.push("(");
                self.type_join(elems, ", ");
                if elems.len() == 1 {
                    self.push(",");
                }
                self.push(")");
            }
            TypeKind::Function { params, ret } => {
                self.push("(");
                self.type_join(params, ", ");
                self.push(") => ");
                self.ty(ret);
            }
            TypeKind::ExternFunction { params, ret } => {
                self.push("extern (");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    if let Some(name) = &p.name {
                        self.push(&name.name);
                        self.push(": ");
                    }
                    self.ty(&p.ty);
                }
                self.push(") => ");
                self.ty(ret);
            }
            TypeKind::Union(variants) => {
                self.type_join(variants, " | ");
            }
            TypeKind::Pointer(inner) => {
                self.push("*");
                self.ty(inner);
            }
            TypeKind::Array { elem, len } => {
                self.push("[");
                self.ty(elem);
                self.push("; ");
                self.expr(len);
                self.push("]");
            }
            TypeKind::SelfType => self.push("Self"),
            TypeKind::Paren(inner) => {
                self.push("(");
                self.ty(inner);
                self.push(")");
            }
        }
    }

    // =======================================================================
    // Blocks & statements
    // =======================================================================

    fn block(&mut self, b: &Block) {
        if b.stmts.is_empty() && b.trailing.is_none() {
            self.push("{}");
            return;
        }
        self.push("{");
        self.indent += 1;
        for s in &b.stmts {
            self.nl();
            self.stmt(s);
        }
        if let Some(t) = &b.trailing {
            self.nl();
            self.expr(t);
        }
        self.indent -= 1;
        self.nl();
        self.push("}");
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Var(lv) => {
                self.push("var ");
                self.pattern(&lv.pattern);
                if let Some(ty) = &lv.ty {
                    self.push(": ");
                    self.ty(ty);
                }
                self.push(" = ");
                self.expr(&lv.init);
                self.push(";");
            }
            StmtKind::Assign { target, value } => {
                self.expr(target);
                self.push(" = ");
                self.expr(value);
                self.push(";");
            }
            StmtKind::Expr(e) => {
                self.expr(e);
                if !is_block_like(e) {
                    self.push(";");
                }
            }
            StmtKind::Item(it) => self.item(it),
        }
    }

    // =======================================================================
    // Expressions
    // =======================================================================

    /// Render `e` parenthesized when it is an operator/prefix/control-flow
    /// expression, so that using it as a sub-operand cannot change parsing.
    fn sub(&mut self, e: &Expr) {
        if needs_wrap(e) {
            self.push("(");
            self.expr(e);
            self.push(")");
        } else {
            self.expr(e);
        }
    }

    /// Render an expression that sits in a "statement head" position
    /// (`if`/`while`/`for`/`match` scrutinees), wrapping the cases that the
    /// parser's no-struct-literal restriction would otherwise misread.
    fn head(&mut self, e: &Expr) {
        if matches!(e.kind, ExprKind::StructLit { .. } | ExprKind::MapLit(_)) {
            self.push("(");
            self.expr(e);
            self.push(")");
        } else {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Int(lit) => {
                self.push(int_prefix(lit.base));
                self.push(&lit.raw);
                if let Some(suf) = &lit.suffix {
                    self.push(suf);
                }
            }
            ExprKind::Float(lit) => {
                self.push(&lit.raw);
                if let Some(suf) = &lit.suffix {
                    self.push(suf);
                }
            }
            ExprKind::Bool(b) => self.push(if *b { "true" } else { "false" }),
            ExprKind::Null => self.push("null"),
            ExprKind::Char(c) => self.push(&c.raw),
            ExprKind::Str(s) => self.string_lit(s),

            ExprKind::Ident(i) => self.push(&i.name),
            ExprKind::SelfExpr => self.push("self"),
            ExprKind::Underscore => self.push("_"),

            ExprKind::Tuple(elems) => {
                self.push("(");
                for (i, el) in elems.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(el);
                }
                if elems.len() == 1 {
                    self.push(",");
                }
                self.push(")");
            }
            ExprKind::Paren(inner) => {
                self.push("(");
                self.expr(inner);
                self.push(")");
            }
            ExprKind::List(elems) => {
                self.push("[");
                for (i, el) in elems.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(el);
                }
                self.push("]");
            }
            ExprKind::MapLit(items) => {
                if items.is_empty() {
                    self.push("{:}");
                    return;
                }
                self.push("{ ");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    match item {
                        MapItem::Entry { key, value, .. } => {
                            self.expr(key);
                            self.push(": ");
                            self.expr(value);
                        }
                        MapItem::Spread(base) => {
                            self.push("..");
                            self.expr(base);
                        }
                    }
                }
                self.push(" }");
            }
            ExprKind::StructLit { path, fields, spread } => {
                self.type_path(path);
                if fields.is_empty() && spread.is_none() {
                    self.push(" {}");
                    return;
                }
                self.push(" { ");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.push(&f.name.name);
                    if let Some(v) = &f.value {
                        self.push(": ");
                        self.expr(v);
                    }
                }
                if let Some(base) = spread {
                    if !fields.is_empty() {
                        self.push(", ");
                    }
                    self.push("..");
                    self.expr(base);
                }
                self.push(" }");
            }

            ExprKind::Unary { op, operand, .. } => {
                self.push(unary_op(*op));
                self.sub(operand);
            }
            ExprKind::Binary { op, left, right, .. } => {
                self.sub(left);
                self.push(" ");
                self.push(binary_op(*op));
                self.push(" ");
                self.sub(right);
            }
            ExprKind::Cast { op, expr, ty, .. } => {
                self.sub(expr);
                self.push(match op {
                    CastOp::As => " as ",
                    CastOp::Is => " is ",
                });
                self.ty(ty);
            }

            ExprKind::Field { receiver, name } => {
                self.sub(receiver);
                self.push(".");
                self.push(&name.name);
            }
            ExprKind::TupleIndex { receiver, index, .. } => {
                self.sub(receiver);
                self.push(".");
                self.push(&index.to_string());
            }
            ExprKind::Call { callee, generics, args, trailing_closure } => {
                self.sub(callee);
                self.type_args(generics);
                self.push("(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(a);
                }
                self.push(")");
                if let Some(tc) = trailing_closure {
                    self.push(" ");
                    self.trailing_closure(tc);
                }
            }
            ExprKind::Index { receiver, index } => {
                self.sub(receiver);
                self.push("[");
                self.expr(index);
                self.push("]");
            }
            ExprKind::Try { expr, .. } => {
                self.sub(expr);
                self.push("?");
            }
            ExprKind::Ref { expr, .. } => {
                self.push("&");
                self.sub(expr);
            }
            ExprKind::Deref { expr, .. } => {
                self.push("*");
                self.sub(expr);
            }
            ExprKind::Await { expr, .. } => {
                self.push("await ");
                self.sub(expr);
            }
            ExprKind::Spawn { expr, .. } => {
                self.push("spawn ");
                self.sub(expr);
            }

            ExprKind::If { cond, then_block, else_branch } => {
                self.push("if ");
                self.head(cond);
                self.push(" ");
                self.block(then_block);
                if let Some(eb) = else_branch {
                    self.push(" else ");
                    match eb {
                        ElseBranch::If(inner) => self.expr(inner),
                        ElseBranch::Block(b) => self.block(b),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.push("match ");
                self.head(scrutinee);
                self.push(" {");
                self.indent += 1;
                for arm in arms {
                    self.nl();
                    self.pattern(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.push(" if ");
                        self.expr(g);
                    }
                    self.push(" => ");
                    self.expr(&arm.body);
                    self.push(",");
                }
                self.indent -= 1;
                self.nl();
                self.push("}");
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Loop(b) => {
                self.push("loop ");
                self.block(b);
            }
            ExprKind::While { cond, body } => {
                self.push("while ");
                self.head(cond);
                self.push(" ");
                self.block(body);
            }
            ExprKind::For { pattern, in_async, iter, body } => {
                self.push("for ");
                if *in_async {
                    self.push("await ");
                }
                self.pattern(pattern);
                self.push(" in ");
                self.head(iter);
                self.push(" ");
                self.block(body);
            }
            ExprKind::Return(val) => {
                self.push("return");
                if let Some(v) = val {
                    self.push(" ");
                    self.expr(v);
                }
            }
            ExprKind::Break(val) => {
                self.push("break");
                if let Some(v) = val {
                    self.push(" ");
                    self.expr(v);
                }
            }
            ExprKind::Continue => self.push("continue"),

            ExprKind::Closure { params, return_type, is_async, body } => {
                self.push("(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.push(&p.name.name);
                    if let Some(ty) = &p.ty {
                        self.push(": ");
                        self.ty(ty);
                    }
                }
                self.push(")");
                if let Some(rt) = return_type {
                    self.push(": ");
                    self.ty(rt);
                }
                if *is_async {
                    self.push(" async");
                }
                self.push(" => ");
                self.expr(body);
            }
            ExprKind::AnonFn(f) => self.function_item(f),
            ExprKind::AsyncBlock(b) => {
                self.push("async ");
                self.block(b);
            }
            ExprKind::MacroCall { name, args, block, .. } => {
                self.push("@");
                self.push(&name.name);
                if !args.is_empty() {
                    self.push("(");
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.push(", ");
                        }
                        match arg {
                            AttrArg::Positional(e) => self.expr(e),
                            AttrArg::Named { name, value, .. } => {
                                self.push(&name.name);
                                self.push(" = ");
                                self.expr(value);
                            }
                        }
                    }
                    self.push(")");
                }
                if let Some(b) = block {
                    self.push(" ");
                    self.block(b);
                }
            }
        }
    }

    /// Render a trailing closure with brace syntax, preserving the implicit
    /// `it` binding (which only exists in trailing-closure position).
    fn trailing_closure(&mut self, e: &Expr) {
        let ExprKind::Closure { params, body, .. } = &e.kind else {
            // Should not happen, but stay correct: print as an ordinary expr.
            self.expr(e);
            return;
        };
        self.push("{");
        if !params.is_empty() {
            self.push(" ");
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.push(&p.name.name);
                if let Some(ty) = &p.ty {
                    self.push(": ");
                    self.ty(ty);
                }
            }
            self.push(" =>");
        }
        match &body.kind {
            ExprKind::Block(b) => {
                if b.stmts.is_empty() && b.trailing.is_none() {
                    if params.is_empty() {
                        self.push("}");
                    } else {
                        self.push(" }");
                    }
                    return;
                }
                self.indent += 1;
                for s in &b.stmts {
                    self.nl();
                    self.stmt(s);
                }
                if let Some(t) = &b.trailing {
                    self.nl();
                    self.expr(t);
                }
                self.indent -= 1;
                self.nl();
                self.push("}");
            }
            _ => {
                self.push(" ");
                self.expr(body);
                self.push(" }");
            }
        }
    }

    fn type_path(&mut self, p: &TypePath) {
        self.push(&p.name.name);
        self.type_args(&p.generics);
    }

    fn string_lit(&mut self, s: &StringLit) {
        self.push("\"");
        for part in &s.parts {
            match part {
                StringPart::Text { text, .. } => self.push(text),
                StringPart::Ident(i) => {
                    self.push("$");
                    self.push(&i.name);
                }
                StringPart::Expr(e) => {
                    self.push("${");
                    self.expr(e);
                    self.push("}");
                }
            }
        }
        self.push("\"");
    }

    // =======================================================================
    // Patterns
    // =======================================================================

    fn pattern(&mut self, p: &Pattern) {
        match &p.kind {
            PatternKind::Wildcard => self.push("_"),
            PatternKind::Binding(i) => self.push(&i.name),
            PatternKind::Literal(e) => self.expr(e),
            PatternKind::TypeBinding { ty, binding } => {
                self.ty(ty);
                if let Some(b) = binding {
                    self.push(" ");
                    self.push(&b.name);
                }
            }
            PatternKind::UnitPath(tp) => self.type_path(tp),
            PatternKind::TupleStruct { path, fields, rest } => {
                self.type_path(path);
                self.push("(");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.pattern(f);
                }
                if let Some(r) = rest {
                    if !fields.is_empty() {
                        self.push(", ");
                    }
                    self.rest_pattern(r);
                }
                self.push(")");
            }
            PatternKind::RecordStruct { path, fields, has_rest } => {
                self.type_path(path);
                if fields.is_empty() && !has_rest {
                    self.push(" {}");
                    return;
                }
                self.push(" { ");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.push(&f.name.name);
                    if let Some(pat) = &f.pattern {
                        self.push(": ");
                        self.pattern(pat);
                    }
                }
                if *has_rest {
                    if !fields.is_empty() {
                        self.push(", ");
                    }
                    self.push("..");
                }
                self.push(" }");
            }
            PatternKind::Tuple { elems, rest } => {
                self.push("(");
                self.elems_with_rest(elems, rest);
                self.push(")");
            }
            PatternKind::List { elems, rest } => {
                self.push("[");
                self.elems_with_rest(elems, rest);
                self.push("]");
            }
            PatternKind::Or(pats) => {
                for (i, p) in pats.iter().enumerate() {
                    if i > 0 {
                        self.push(" | ");
                    }
                    self.pattern(p);
                }
            }
        }
    }

    /// Render a comma-separated pattern list with an optional `..rest` spliced
    /// in at its recorded index.
    fn elems_with_rest(&mut self, elems: &[Pattern], rest: &Option<(usize, RestPattern)>) {
        let rest_at = rest.as_ref().map(|(i, _)| *i);
        let mut wrote = false;
        for i in 0..=elems.len() {
            if rest_at == Some(i) {
                if wrote {
                    self.push(", ");
                }
                self.rest_pattern(&rest.as_ref().unwrap().1);
                wrote = true;
            }
            if i < elems.len() {
                if wrote {
                    self.push(", ");
                }
                self.pattern(&elems[i]);
                wrote = true;
            }
        }
    }

    fn rest_pattern(&mut self, r: &RestPattern) {
        self.push("..");
        if let Some(name) = &r.name {
            self.push(&name.name);
        }
    }
}

// ===========================================================================
// Small helpers
// ===========================================================================

fn is_block_like(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Loop(_)
            | ExprKind::Block(_)
            | ExprKind::AsyncBlock(_)
    )
}

/// Whether `e` must be parenthesized when used as a sub-operand of an operator
/// or postfix/prefix expression.
fn needs_wrap(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Closure { .. }
            | ExprKind::AnonFn(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Loop(_)
            | ExprKind::Return(_)
            | ExprKind::Break(_)
            | ExprKind::Continue
            | ExprKind::Await { .. }
            | ExprKind::Spawn { .. }
            | ExprKind::Ref { .. }
            | ExprKind::Deref { .. }
            | ExprKind::AsyncBlock(_)
    )
}

fn int_prefix(base: IntBase) -> &'static str {
    match base {
        IntBase::Dec => "",
        IntBase::Hex => "0x",
        IntBase::Oct => "0o",
        IntBase::Bin => "0b",
    }
}

fn unary_op(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
    }
}

fn binary_op(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lex, parse, FileId};

    /// Parse `src`, print the AST, and return `(printed, parse_errors)`.
    fn print_once(src: &str) -> (String, usize) {
        let (tokens, lex_errs) = lex(src, FileId(0));
        assert!(lex_errs.is_empty(), "lex errors in source: {lex_errs:?}");
        let (module, parse_errs) = parse(src, &tokens);
        (print_module(&module), parse_errs.len())
    }

    /// The core invariant: printing is a fixed point. `parse → print` must
    /// produce source that (a) parses with no errors and (b) prints to exactly
    /// the same text — i.e. the printer is idempotent on its own output.
    fn assert_round_trip(src: &str) {
        let (first, errs1) = print_once(src);
        assert_eq!(errs1, 0, "first parse of source had errors\n--- src ---\n{src}");
        let (second, errs2) = print_once(&first);
        assert_eq!(
            errs2, 0,
            "re-parsing printed output produced {errs2} error(s)\n--- printed ---\n{first}"
        );
        assert_eq!(first, second, "printer is not idempotent\n--- first ---\n{first}\n--- second ---\n{second}");
    }

    #[test]
    fn functions_and_signatures() {
        assert_round_trip(
            r#"
            function add(a: i64, b: i64): i64 { a + b }
            function noargs() {}
            function generic<T, U: Display>(x: T, y: U): T { x }
            pub function pub_fn(): bool { true }
            function fetches(): i64 async { 1 }
            "#,
        );
    }

    #[test]
    fn structs_all_shapes() {
        assert_round_trip(
            r#"
            struct Unit;
            struct Pair(pub i64, i64);
            struct Person { name: str, pub age: i64 }
            pub struct Generic<T> { value: T }
            "#,
        );
    }

    #[test]
    fn expressions_and_operators() {
        assert_round_trip(
            r#"
            function f(): i64 {
                var a = 1 + 2 * 3 - 4 / 5 % 6;
                var b = (a == 1) && (a != 2) || !false;
                var c = a & 7 | 8 ^ 9;
                var d = a << 2 >> 1;
                var e = -a;
                var g = a as f64;
                var h = a is i64;
                a
            }
            "#,
        );
    }

    #[test]
    fn nested_unary_and_precedence() {
        assert_round_trip(
            r#"
            function f(): i64 {
                var a = -(-5);
                var b = (1 + 2) * 3;
                var c = 1 + 2 * 3;
                var d = !(a > 0);
                a
            }
            "#,
        );
    }

    #[test]
    fn control_flow() {
        assert_round_trip(
            r#"
            function f(x: i64): i64 {
                if x > 0 {
                    return 1;
                } else if x < 0 {
                    return -1;
                } else {
                    return 0;
                }
                while x > 0 {
                    x = x - 1;
                }
                loop {
                    break 7;
                }
                for i in items {
                    print(i);
                }
                0
            }
            "#,
        );
    }

    #[test]
    fn match_and_patterns() {
        assert_round_trip(
            r#"
            function f(v: Shape): i64 {
                match v {
                    Circle { radius } => radius,
                    Rect(w, h) => w * h,
                    Point => 0,
                    i64 n if n > 0 => n,
                    _ => -1,
                }
            }
            function g(t: (i64, i64, i64)): i64 {
                match t {
                    (a, ..rest) => a,
                }
            }
            "#,
        );
    }

    #[test]
    fn closures_and_calls() {
        assert_round_trip(
            r#"
            function f() {
                var double = (x: i64): i64 => x * 2;
                var noargs = () => 0;
                var mapped = xs.map { it * 2 };
                var withparams = xs.map { x => x + 1 };
                var anon = function(y: i64): i64 { y };
                g<i64>(1, 2, 3);
            }
            "#,
        );
    }

    #[test]
    fn literals_and_strings() {
        assert_round_trip(
            r#"
            function f() {
                var a = 42;
                var b = 0xFF;
                var c = 0b1010;
                var d = 0o17;
                var e = 3.14;
                var g = "plain";
                var h = "value is $a and ${b + 1}";
                var i = 'x';
                var j = true;
                var k = null;
                var l = [1, 2, 3];
                var m = (1, 2);
                var n = { "key": a, ..base };
            }
            "#,
        );
    }

    #[test]
    fn interfaces_and_extends() {
        assert_round_trip(
            r#"
            interface Animal {
                function speak(self): str;
                function legs(self): i64 { 4 }
            }
            extend Dog: Animal {
                function speak(self): str { "woof" }
                pub function name(self): str { "rex" }
            }
            extend<T> Box<T> {
                function get(self): T { self.value }
            }
            "#,
        );
    }

    #[test]
    fn types_and_aliases() {
        assert_round_trip(
            r#"
            type Callback = (i64, i64) => bool;
            type Maybe<T> = T | null;
            type Pair = (i64, str);
            type Ptr = *i64;
            extern type Opaque;
            extern function c_fn(x: i64): i64;
            extern var errno: i64;
            "#,
        );
    }

    #[test]
    fn modules_imports_tests() {
        assert_round_trip(
            r#"
            import "std/io";
            import "std/math" as math;
            import { sin, cos as cosine } from "std/trig";
            mod inner {
                function helper(): i64 { 1 }
            }
            test "addition works" {
                var x = 1 + 1;
            }
            bench "fast path" {
                var y = 0;
            }
            "#,
        );
    }

    #[test]
    fn postfix_chains_and_field_access() {
        assert_round_trip(
            r#"
            function f(p: Point): i64 {
                var a = p.x;
                var b = p.pos.y;
                var c = arr[0];
                var d = mat[0][1];
                var e = tup.0;
                var g = compute()?;
                var h = (a + b).abs();
                a
            }
            "#,
        );
    }

    #[test]
    fn doc_comments_and_attributes() {
        assert_round_trip(
            "/// A documented function.\n@inline\nfunction f(): i64 { 1 }\n",
        );
    }
}
