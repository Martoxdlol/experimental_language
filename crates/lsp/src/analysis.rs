//! The bridge between the `compiler` front-end and the language server.
//!
//! [`Compiled`] runs the full lex → parse → analyze pipeline over one open
//! document's text and exposes everything the server's feature handlers need:
//! diagnostics, the span-keyed type/resolution tables, and queries that map an
//! editor position to the symbol or type under the cursor.
//!
//! Editor positions are UTF-16 (the LSP default); the compiler works in UTF-8
//! byte offsets. The free conversion functions at the bottom bridge the two.

use compiler::ast::{
    ExternItem, FunctionItem, FunctionSig, GenericParams, Item, ItemKind, Module, ParamKind,
    StructItem, StructKind, Type, TypeKind,
};
use compiler::hir::{self, Hir};
use compiler::ids::{DefId, LocalId};
use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::ast::ModuleKind;
use compiler::sema::resolve_ctx::normalize;
use compiler::sema::symbols::{Def, DefKind, Externals, Program};
use compiler::sema::{analyze_multi_ctx, Analysis, Builtin, ResolveContext, ValueRes};
use compiler::span::{FileId, SourceMap, Span};
use compiler::token::{Token, TokenKind};
use compiler::ty::Ty;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{Position, Range};

/// The single file an open document occupies in its private `SourceMap`.
pub const DOC_FILE: FileId = FileId(0);

/// A semantic-token class. The numeric value is the index into the legend
/// declared by the server (`semantic_token_legend`), so the two must agree.
///
/// `Keyword`/`Number`/`String`/`Comment`/`Operator` are part of the legend so
/// the indices stay stable, but they are never emitted: those token kinds are
/// already colored by the TextMate grammar with richer sub-scopes.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u32)]
#[allow(dead_code)]
pub enum TokenClass {
    Type = 0,
    Struct = 1,
    Interface = 2,
    Function = 3,
    Method = 4,
    Variable = 5,
    Parameter = 6,
    Property = 7,
    Keyword = 8,
    Number = 9,
    String = 10,
    Comment = 11,
    Operator = 12,
}

/// The full result of analysing one open document.
pub struct Compiled {
    pub text: String,
    pub map: SourceMap,
    pub tokens: Vec<Token>,
    /// The parsed module (pre-`@Derive` expansion — the user's actual items).
    pub module: Module,
    /// The full analysis, including the typed, resolved, desugared HIR
    /// (`analysis.hir` — the same tree codegen consumes). The server's position
    /// queries read provenance off its nodes rather than span-keyed side tables.
    pub analysis: Analysis,
    /// Position-query index built by walking `analysis.hir`.
    pub index: HirIndex,
    /// Span + message for every lexer/parser/semantic error, already filtered
    /// to the document file.
    pub diagnostics: Vec<(Span, String)>,
}

/// A position-query index built by walking the typed HIR once. Every entry is a
/// node field (a span + its type / resolution), so the server answers hover,
/// go-to-definition, and semantic-token queries straight from the HIR — no
/// `CheckResults` span tables. Spans outside the document file are kept (callers
/// filter), since a body may interleave prelude-derived spans.
pub struct HirIndex {
    /// `(span, type)` for every expression node, plus each call's callee name
    /// span → its callee type (the function type / receiver type).
    pub expr_types: Vec<(Span, Ty)>,
    /// `(span, resolution)` for every resolved name occurrence: `Name` nodes and
    /// each call's callee name (folded into its dispatch kind).
    pub resolutions: Vec<(Span, ValueRes)>,
    /// Locals that are function parameters (for variable-vs-parameter coloring).
    pub params: HashSet<LocalId>,
    /// The binding-occurrence span of every local (mirrors [`Hir::local_decls`]).
    pub local_decls: HashMap<LocalId, Span>,
    /// The type of every local (union of every body's `locals`), for hover.
    pub local_types: HashMap<LocalId, Ty>,
}

impl HirIndex {
    fn build(hir: &Hir) -> HirIndex {
        let mut params = HashSet::new();
        for sig in hir.fn_sigs.values() {
            for (local, _) in &sig.params {
                params.insert(*local);
            }
        }
        let mut local_types = HashMap::new();
        for body in hir.bodies.values() {
            for (local, ty) in &body.locals {
                local_types.insert(*local, *ty);
            }
        }
        let mut idx = HirIndex {
            expr_types: Vec::new(),
            resolutions: Vec::new(),
            params,
            local_decls: hir.local_decls.clone(),
            local_types,
        };
        for body in hir.bodies.values() {
            idx.walk_block(&body.block);
        }
        idx
    }

    fn walk_block(&mut self, b: &hir::Block) {
        for s in &b.stmts {
            self.walk_stmt(s);
        }
        if let Some(e) = &b.trailing {
            self.walk_expr(e);
        }
    }

    fn walk_stmt(&mut self, s: &hir::Stmt) {
        use hir::StmtKind as S;
        match &s.kind {
            S::Let { pattern, init } => {
                self.walk_pattern(pattern);
                self.walk_expr(init);
            }
            S::Assign { target, value } => {
                self.walk_expr(target);
                self.walk_expr(value);
            }
            S::Expr(e) => self.walk_expr(e),
            // The nested item's body lives in `hir.bodies` and is walked there.
            S::Item(_) => {}
        }
    }

    fn walk_pattern(&mut self, p: &hir::Pattern) {
        use hir::PatternKind as P;
        match &p.kind {
            P::Literal(e) => self.walk_expr(e),
            P::TupleStruct { fields, .. } => fields.iter().for_each(|f| self.walk_pattern(f)),
            P::RecordStruct { fields, .. } => {
                fields.iter().for_each(|f| self.walk_pattern(&f.pattern))
            }
            P::Tuple { elems, .. } | P::List { elems, .. } | P::Or(elems) => {
                elems.iter().for_each(|e| self.walk_pattern(e))
            }
            P::Wildcard | P::Bind(_) | P::TypeBind { .. } | P::UnitPath { .. } => {}
        }
    }

    fn walk_expr(&mut self, e: &hir::Expr) {
        use hir::ExprKind as K;
        self.expr_types.push((e.span, e.ty));
        match &e.kind {
            K::Name(res) => self.resolutions.push((e.span, *res)),
            K::Str(parts) => {
                for part in parts {
                    if let hir::StrPart::Interp { expr, .. } = part {
                        self.walk_expr(expr);
                    }
                }
            }
            K::Tuple(xs) | K::List(xs) => xs.iter().for_each(|x| self.walk_expr(x)),
            K::Map(entries) => {
                for entry in entries {
                    match entry {
                        hir::MapEntry::Kv { key, value } => {
                            self.walk_expr(key);
                            self.walk_expr(value);
                        }
                        hir::MapEntry::Spread(x) => self.walk_expr(x),
                    }
                }
            }
            K::Struct { fields, spread, .. } => {
                fields.iter().for_each(|f| self.walk_expr(&f.value));
                if let Some(s) = spread {
                    self.walk_expr(s);
                }
            }
            K::Field { receiver, .. } | K::TupleIndex { receiver, .. } => self.walk_expr(receiver),
            K::Index { receiver, index } => {
                self.walk_expr(receiver);
                self.walk_expr(index);
            }
            K::Call { kind, args, callee_span, callee_ty } => {
                // The callee name folded into the dispatch kind: re-expose its
                // type (for hover) and resolution (for go-to-definition).
                self.expr_types.push((*callee_span, *callee_ty));
                if let Some(res) = callee_resolution(kind) {
                    self.resolutions.push((*callee_span, res));
                }
                args.iter().for_each(|a| self.walk_expr(a));
                if let hir::CallKind::Closure { callee } = kind {
                    self.walk_expr(callee);
                }
            }
            K::Intrinsic { args, .. } => args.iter().for_each(|a| self.walk_expr(a)),
            K::Unary { operand, .. } => self.walk_expr(operand),
            K::Binary { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            K::Cast { expr, .. } | K::Ref(expr) | K::Deref(expr) | K::Adjust { expr, .. } => {
                self.walk_expr(expr)
            }
            K::Try { expr, .. } | K::Await { expr, .. } | K::Spawn { expr, .. } => {
                self.walk_expr(expr)
            }
            K::If { cond, then_block, else_branch } => {
                self.walk_expr(cond);
                self.walk_block(then_block);
                if let Some(e) = else_branch {
                    self.walk_expr(e);
                }
            }
            K::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.walk_pattern(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g);
                    }
                    self.walk_expr(&arm.body);
                }
            }
            K::Block(b) | K::Loop(b) => self.walk_block(b),
            K::While { cond, body } => {
                self.walk_expr(cond);
                self.walk_block(body);
            }
            K::For { pattern, iter, body, .. } => {
                self.walk_pattern(pattern);
                self.walk_expr(iter);
                self.walk_block(body);
            }
            K::Return(v) | K::Break(v) => {
                if let Some(e) = v {
                    self.walk_expr(e);
                }
            }
            K::Closure { body, .. } => self.walk_expr(body),
            K::AsyncBlock { body, .. } => self.walk_block(body),
            K::Int(_) | K::Float(_) | K::Bool(_) | K::Null | K::Char(_) | K::Continue
            | K::Discard | K::Error => {}
        }
    }
}

/// Discover the file-backed submodules declared in `module`, parse each into
/// `map`/`externals` (recursively), and key them by their module path from the
/// crate root. Mirrors the CLI's `load_submodules` but reads through `read` (so
/// an open, unsaved editor buffer wins over the on-disk copy) and silently skips
/// modules that cannot be read (the open document still analyses, with an
/// "unloaded module" the checker reports against the import).
fn load_subs(
    map: &mut SourceMap,
    dir: &Path,
    module: &Module,
    mod_path: &mut Vec<String>,
    externals: &mut Externals,
    file_of: &mut HashMap<Vec<String>, PathBuf>,
    read: &dyn Fn(&Path) -> Option<String>,
) {
    for item in &module.items {
        let ItemKind::Module(m) = &item.kind else { continue };
        if !matches!(m.kind, ModuleKind::External) {
            continue;
        }
        let child_path = dir.join(format!("{}.otter", m.name.name));
        let Some(src) = read(&child_path) else { continue };
        let file = map.add_file(child_path.display().to_string(), src.clone());
        let (tokens, _lex_errors) = lex(&src, file);
        let (child_module, _parse_errors) = parse(&src, &tokens);
        mod_path.push(m.name.name.clone());
        file_of.insert(mod_path.clone(), normalize(&child_path));
        // A child's own submodules live under `<dir>/<name>/` (`docs/17` §17.2).
        let child_dir = dir.join(&m.name.name);
        load_subs(map, &child_dir, &child_module, mod_path, externals, file_of, read);
        externals.insert(mod_path.clone(), child_module);
        mod_path.pop();
    }
}

/// The value resolution a call's callee name folds into (for IDE queries on the
/// call name). Builtin methods and closure-value calls have no callee def — the
/// former resolves structurally, the latter is a `Name`/expression handled
/// separately.
fn callee_resolution(kind: &hir::CallKind) -> Option<ValueRes> {
    use hir::CallKind as C;
    Some(match kind {
        C::Direct { def, .. } | C::Extern { def } => ValueRes::Function(*def),
        C::Method { def, .. } => ValueRes::Method(*def),
        C::Builtin(b) => ValueRes::Builtin(*b),
        C::TupleCtor { def, .. } => ValueRes::StructCtor(*def),
        C::BuiltinMethod { .. } | C::Closure { .. } => return None,
    })
}

impl Compiled {
    /// Run the whole front-end over a single document's `text` (no submodule
    /// loading — used by tests and for documents without a filesystem path).
    pub fn new(text: String) -> Compiled {
        Compiled::build(text, None, &|_| None)
    }

    /// Run the front-end over an on-disk document, loading the file-backed
    /// submodules it declares so cross-module imports resolve (`docs/17`). The
    /// document is at `base_dir/<stem>.otter`; its submodules live under
    /// `base_dir/<stem>/`. `read` resolves a submodule path to its text —
    /// the server passes a reader that prefers an open editor buffer over disk.
    pub fn new_multi(
        text: String,
        base_dir: std::path::PathBuf,
        stem: String,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Compiled {
        Compiled::build(text, Some((base_dir, stem)), read)
    }

    fn build(
        text: String,
        project: Option<(std::path::PathBuf, String)>,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Compiled {
        let mut map = SourceMap::new();
        let file = map.add_file("<doc>", text.clone());

        let (tokens, lex_errors) = lex(&text, file);
        let (module, parse_errors) = parse(&text, &tokens);

        // Load file-backed submodules so the open document type-checks against
        // them. The document is a top-level entry, so its `mod foo` declarations
        // resolve to *siblings* `<base_dir>/foo.otter` (`docs/17` §17.2).
        let mut externals = Externals::new();
        let mut file_of: HashMap<Vec<String>, PathBuf> = HashMap::new();
        let mut ctx = ResolveContext::direct();
        if let Some((base_dir, stem)) = project {
            // The entry's own file, keyed at the root path `[]`.
            file_of.insert(Vec::new(), normalize(&base_dir.join(format!("{stem}.otter"))));
            let mut mod_path = Vec::new();
            load_subs(&mut map, &base_dir, &module, &mut mod_path, &mut externals, &mut file_of, read);
            ctx = ResolveContext {
                project: true,
                package_name: None,
                no_std: false,
                source_root: Some(normalize(&base_dir)),
                package_root: base_dir.parent().map(normalize),
                file_of,
                file_import_allow: Vec::new(),
                dependencies: HashSet::new(),
                packages: HashMap::new(),
                file_targets: HashMap::new(),
                macro_recursion_limit: None,
            };
        }

        // Expand user procedural macros (`docs/22`) on a *clone* before analysis,
        // so the editor sees diagnostics, hover, and go-to over the post-expansion
        // program — while `module` keeps the user's verbatim items for syntactic
        // views. A file with no `@ProcMacro` definitions expands to itself at no
        // cost (and runs no macro code).
        let mut expanded = module.clone();
        let mut exp_externals = externals.clone();
        let macro_errors =
            macros::expand_user_macros(&mut expanded, &mut exp_externals, &ctx);

        let analysis = analyze_multi_ctx(&expanded, &exp_externals, &ctx);
        let index = HirIndex::build(&analysis.hir);

        let mut diagnostics = Vec::new();
        for e in &lex_errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        for e in &parse_errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        for e in &macro_errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        for e in &analysis.errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        // Keep only diagnostics that point into the document itself; spans in
        // the prelude, `@Derive`-synthesised virtual files, or loaded submodules
        // have no editor location in this document.
        diagnostics.retain(|(s, _)| s.file == DOC_FILE);

        Compiled { text, map, tokens, module, analysis, index, diagnostics }
    }

    /// The names of every `@ProcMacro` function defined in the open document
    /// (recursing into inline submodules) — for `@`-prefix macro completion
    /// (`docs/22`).
    pub fn proc_macro_names(&self) -> Vec<String> {
        fn walk(module: &compiler::ast::Module, out: &mut Vec<String>) {
            use compiler::ast::*;
            for it in &module.items {
                if let ItemKind::Function(f) = &it.kind {
                    if it.attrs.iter().any(|a| a.name.name == "ProcMacro") {
                        out.push(f.name.name.clone());
                    }
                }
                if let ItemKind::Module(ModuleItem {
                    kind: ModuleKind::Inline { items, .. }, ..
                }) = &it.kind
                {
                    let sub = Module { inner_docs: Vec::new(), items: items.clone(), span: it.span };
                    walk(&sub, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.module, &mut out);
        out
    }

    /// Render a type using the program's definition names.
    pub fn display_ty(&self, ty: Ty) -> String {
        let prog = &self.analysis.program;
        self.analysis.tcx.display(ty, &|id| prog.def(id).name.clone())
    }

    fn program(&self) -> &Program {
        &self.analysis.program
    }

    /// The smallest `(span, value)` whose span contains `off` (document file
    /// only). Ties keep the first seen. The HIR-walk analogue of the old
    /// `smallest_containing` over a span-keyed table.
    fn smallest_at<T: Copy>(off: usize, items: &[(Span, T)]) -> Option<(Span, T)> {
        let mut best: Option<(Span, T)> = None;
        for &(s, v) in items {
            if s.file != DOC_FILE {
                continue;
            }
            let (lo, hi) = (s.lo.to_usize(), s.hi.to_usize());
            if lo <= off && off < hi && best.is_none_or(|(b, _)| s.len() < b.len()) {
                best = Some((s, v));
            }
        }
        best
    }

    /// The value resolution and its span for the name under `off`, if any.
    pub fn resolution_at(&self, off: usize) -> Option<(Span, ValueRes)> {
        Self::smallest_at(off, &self.index.resolutions)
    }

    /// The type and span of the expression under `off`, if any.
    pub fn expr_ty_at(&self, off: usize) -> Option<(Span, Ty)> {
        Self::smallest_at(off, &self.index.expr_types)
    }

    /// The defining span of what a resolution points at, when it lives in the
    /// document (prelude / builtin targets have no editor location).
    pub fn definition_span(&self, res: ValueRes) -> Option<Span> {
        let span = match res {
            ValueRes::Local(id) => *self.index.local_decls.get(&id)?,
            ValueRes::Function(d)
            | ValueRes::Method(d)
            | ValueRes::Global(d)
            | ValueRes::StructCtor(d) => {
                let def = self.program().def(d);
                // Prefer the name occurrence (a tighter, friendlier jump target)
                // over the whole-item span.
                def.item
                    .as_ref()
                    .and_then(item_name_span)
                    .unwrap_or(def.span)
            }
            ValueRes::Builtin(_) => return None,
        };
        // Accept the open document and any loaded submodule file; a virtual file
        // (the prelude or synthesised code, beyond the real file count) has no
        // editor location.
        ((span.file.0 as usize) < self.map.file_count()).then_some(span)
    }

    /// The name-occurrence span of a (type or value) definition `def`, or `None`
    /// for a virtual-file definition. Prefers the name token over the whole item.
    pub fn def_name_span(&self, def: DefId) -> Option<Span> {
        let d = self.program().def(def);
        let span = d.item.as_ref().and_then(item_name_span).unwrap_or(d.span);
        ((span.file.0 as usize) < self.map.file_count()).then_some(span)
    }

    /// Resolve a type name written at byte offset `off` (in a type annotation) to
    /// its definition. Returns the innermost type-name token containing `off`
    /// (so `Foo<Bar>` resolves `Foo` or `Bar` depending on the cursor) along with
    /// the on-screen span of that name, for hover/goto.
    pub fn type_def_at(&self, off: usize) -> Option<(Span, DefId)> {
        let refs = collect_type_refs(&self.module.items);
        let mut best: Option<(Span, &str)> = None;
        for (span, name) in &refs {
            if span.lo.to_usize() <= off
                && off < span.hi.to_usize()
                && best.is_none_or(|(b, _)| span.len() < b.len())
            {
                best = Some((*span, name.as_str()));
            }
        }
        let (span, name) = best?;
        let def = self.lookup_type_def(name)?;
        Some((span, def))
    }

    /// The definition-name span for a type name at `off` — type-position
    /// go-to-definition.
    pub fn type_def_span_at(&self, off: usize) -> Option<Span> {
        let (_, def) = self.type_def_at(off)?;
        self.def_name_span(def)
    }

    /// A human-readable definition for `def`, used in hover popups.
    pub fn def_label(&self, def: &Def) -> String {
        format!("{} `{}`", def.kind.describe(), def.name)
    }

    /// Classify identifier tokens for semantic highlighting, in source order.
    /// Returns `(span, class)` pairs.
    ///
    /// Keywords, numbers, strings, comments, and operators are *not* emitted —
    /// the TextMate grammar already colors them with richer sub-scopes (e.g.
    /// `keyword.control` vs `keyword.declaration`), and an LSP token of type
    /// `keyword` would overwrite that with a single flat color. The LSP only
    /// emits classes where it adds information the grammar cannot derive:
    /// struct vs interface vs alias for type names, function vs method,
    /// parameter vs local variable, property accesses, etc.
    pub fn semantic_tokens(&self) -> Vec<(Span, TokenClass)> {
        let (type_names, fn_names) = self.declared_names();
        let mut out = Vec::with_capacity(self.tokens.len());
        let mut prev_kind: Option<TokenKind> = None;
        for tok in &self.tokens {
            let after_dot = prev_kind == Some(TokenKind::Dot);
            prev_kind = Some(tok.kind);
            if tok.kind == TokenKind::Ident {
                let class = self.classify_ident(tok.span, after_dot, &type_names, &fn_names);
                out.push((tok.span, class));
            }
        }
        out
    }

    /// Classify an identifier token. Prefers the checker's exact resolution;
    /// otherwise falls back to whether the name is a declared type or function.
    fn classify_ident(
        &self,
        span: Span,
        after_dot: bool,
        type_names: &std::collections::HashMap<String, TokenClass>,
        fn_names: &std::collections::HashSet<String>,
    ) -> TokenClass {
        if let Some((_, res)) = self.index.resolutions.iter().find(|(s, _)| *s == span) {
            return match res {
                ValueRes::Local(id) => {
                    if self.index.params.contains(id) {
                        TokenClass::Parameter
                    } else {
                        TokenClass::Variable
                    }
                }
                ValueRes::Function(_) | ValueRes::Builtin(_) => TokenClass::Function,
                ValueRes::Method(_) => TokenClass::Method,
                ValueRes::Global(_) => TokenClass::Variable,
                ValueRes::StructCtor(_) => TokenClass::Struct,
            };
        }
        let name = self.map.slice(span);
        if let Some(&c) = type_names.get(name) {
            return c;
        }
        if fn_names.contains(name) {
            return TokenClass::Function;
        }
        // An identifier immediately after `.` with no value resolution is a
        // field/property access (method calls already resolved above).
        if after_dot {
            return TokenClass::Property;
        }
        // The checker only records value-position resolutions, so a name used
        // in a *type* position (a primitive like `i64`, a prelude type like
        // `List`, a generic param like `T`, an attribute target like `Clone`)
        // would otherwise fall through to `Variable` and override the grammar's
        // type coloring. Recover the type intent from shape.
        if is_primitive_type_name(name)
            || name.chars().next().is_some_and(char::is_uppercase)
        {
            return TokenClass::Type;
        }
        TokenClass::Variable
    }

    /// Collect the names declared at module top level: types (with their kind)
    /// and functions. Used both for fallback highlighting and completion.
    pub fn declared_names(
        &self,
    ) -> (
        std::collections::HashMap<String, TokenClass>,
        std::collections::HashSet<String>,
    ) {
        use compiler::ast::ItemKind::*;
        let mut types = std::collections::HashMap::new();
        let mut fns = std::collections::HashSet::new();
        for item in &self.module.items {
            match &item.kind {
                Function(f) => {
                    fns.insert(f.name.name.clone());
                }
                Struct(s) => {
                    types.insert(s.name.name.clone(), TokenClass::Struct);
                }
                Interface(i) => {
                    types.insert(i.name.name.clone(), TokenClass::Interface);
                }
                TypeAlias(a) => {
                    types.insert(a.name.name.clone(), TokenClass::Type);
                }
                _ => {}
            }
        }
        (types, fns)
    }
}

/// The span of the *name* of an item, for a tighter go-to-definition target
/// than the whole-item span. `extend`/`import` items have no name.
pub fn item_name_span(item: &ItemKind) -> Option<Span> {
    Some(match item {
        ItemKind::Function(f) => f.name.span,
        ItemKind::Struct(s) => s.name.span,
        ItemKind::Interface(i) => i.name.span,
        ItemKind::TypeAlias(a) => a.name.span,
        ItemKind::Var(v) => v.name.span,
        ItemKind::Module(m) => m.name.span,
        ItemKind::Extern(ext) => match ext {
            ExternItem::Function(f) => f.name.span,
            ExternItem::Struct(s) => s.name.span,
            ExternItem::OpaqueType(n) => n.span,
            ExternItem::Var { name, .. } => name.span,
        },
        ItemKind::Extend(_) | ItemKind::Import(_) | ItemKind::Test(_) => return None,
    })
}

/// Collect `(name-span, type-name)` for every type-name occurrence in the
/// **item-level** type positions of `items` — function / method signatures
/// (params, returns, generic bounds & defaults), struct/extern-struct fields,
/// type aliases, `extend`/interface targets and supers, and extern items —
/// recursing into inline submodules. Powers type-position go-to-definition.
/// (Function-body annotations `var x: T`, `e as T`, and pattern type names are
/// a follow-up; the variable/value itself already resolves via the HIR index.)
pub fn collect_type_refs(items: &[Item]) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for item in items {
        collect_item_type_refs(&item.kind, &mut out);
    }
    out
}

fn collect_item_type_refs(item: &ItemKind, out: &mut Vec<(Span, String)>) {
    match item {
        ItemKind::Function(f) => collect_fn_type_refs(f, out),
        ItemKind::Var(v) => {
            if let Some(t) = &v.ty {
                walk_type(t, out);
            }
        }
        ItemKind::Struct(s) => collect_struct_type_refs(s, out),
        ItemKind::Interface(i) => {
            collect_generics(&i.generics, out);
            i.supers.iter().for_each(|t| walk_type(t, out));
            for m in &i.members {
                collect_sig_type_refs(&m.function, out);
            }
        }
        ItemKind::TypeAlias(a) => {
            collect_generics(&a.generics, out);
            walk_type(&a.aliased, out);
        }
        ItemKind::Extend(e) => {
            collect_generics(&e.generics, out);
            walk_type(&e.target, out);
            e.interfaces.iter().for_each(|t| walk_type(t, out));
            for m in &e.members {
                collect_fn_type_refs(&m.function, out);
            }
        }
        ItemKind::Module(m) => {
            if let ModuleKind::Inline { items, .. } = &m.kind {
                for it in items {
                    collect_item_type_refs(&it.kind, out);
                }
            }
        }
        ItemKind::Extern(ext) => match ext {
            ExternItem::Function(f) => collect_fn_type_refs(f, out),
            ExternItem::Struct(s) => collect_struct_type_refs(s, out),
            ExternItem::Var { ty, .. } => walk_type(ty, out),
            ExternItem::OpaqueType(_) => {}
        },
        // A test body has no item-level type annotations to index.
        ItemKind::Import(_) | ItemKind::Test(_) => {}
    }
}

fn collect_generics(g: &Option<GenericParams>, out: &mut Vec<(Span, String)>) {
    if let Some(g) = g {
        for p in &g.params {
            p.bounds.iter().for_each(|t| walk_type(t, out));
            if let Some(d) = &p.default {
                walk_type(d, out);
            }
        }
    }
}

fn collect_fn_type_refs(f: &FunctionItem, out: &mut Vec<(Span, String)>) {
    collect_generics(&f.generics, out);
    for p in &f.params {
        if let ParamKind::Normal { ty, .. } = &p.kind {
            walk_type(ty, out);
        }
    }
    if let Some(r) = &f.return_type {
        walk_type(r, out);
    }
}

fn collect_sig_type_refs(s: &FunctionSig, out: &mut Vec<(Span, String)>) {
    collect_generics(&s.generics, out);
    for p in &s.params {
        if let ParamKind::Normal { ty, .. } = &p.kind {
            walk_type(ty, out);
        }
    }
    if let Some(r) = &s.return_type {
        walk_type(r, out);
    }
}

fn collect_struct_type_refs(s: &StructItem, out: &mut Vec<(Span, String)>) {
    collect_generics(&s.generics, out);
    match &s.kind {
        StructKind::Record(fs) => fs.iter().for_each(|f| walk_type(&f.ty, out)),
        StructKind::Tuple(fs) => fs.iter().for_each(|f| walk_type(&f.ty, out)),
        StructKind::Unit => {}
    }
}

/// Recursively collect the head-name span of every `Named` type within `t`
/// (including nested generic arguments, tuple/union members, function params,
/// pointee, array element).
fn walk_type(t: &Type, out: &mut Vec<(Span, String)>) {
    match &t.kind {
        TypeKind::Named { name, generics } => {
            out.push((name.span, name.name.clone()));
            generics.iter().for_each(|g| walk_type(g, out));
        }
        TypeKind::Tuple(ts) | TypeKind::Union(ts) => ts.iter().for_each(|x| walk_type(x, out)),
        TypeKind::Function { params, ret } => {
            params.iter().for_each(|p| walk_type(p, out));
            walk_type(ret, out);
        }
        TypeKind::ExternFunction { params, ret } => {
            params.iter().for_each(|p| walk_type(&p.ty, out));
            walk_type(ret, out);
        }
        TypeKind::Pointer(b) | TypeKind::Paren(b) => walk_type(b, out),
        TypeKind::Array { elem, .. } => walk_type(elem, out),
        TypeKind::SelfType => {}
    }
}

/// Is `name` the textual name of a built-in primitive type? Used by semantic-
/// token classification to recover the type intent for names the checker never
/// records (type-position only).
pub fn is_primitive_type_name(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "never"
            | "dynamic"
    )
}

/// A builtin's display signature, for hover.
pub fn builtin_signature(b: Builtin) -> &'static str {
    match b {
        Builtin::Print => "print(str): null",
        Builtin::Println => "println(str): null",
        Builtin::Panic => "panic(str): never",
        Builtin::PanicWith => "panic_with(value: dynamic): never",
        Builtin::Exit => "exit(i32): never",
        Builtin::Abort => "abort(): never",
    }
}

// --------------------------------------------------------------------------
// Position <-> byte-offset conversion (UTF-16 editor positions <-> UTF-8).
// --------------------------------------------------------------------------

/// Byte offset (UTF-8) of an LSP `Position` (line + UTF-16 column) in `text`.
pub fn offset_at(text: &str, pos: Position) -> usize {
    let mut idx = 0usize;
    // Advance to the start of the target line.
    if pos.line > 0 {
        let mut seen = 0u32;
        let mut found = false;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == pos.line {
                    idx = i + 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return text.len();
        }
    }
    // Walk UTF-16 code units across the line up to the target column.
    let mut utf16 = 0u32;
    for ch in text[idx..].chars() {
        if ch == '\n' || utf16 >= pos.character {
            break;
        }
        utf16 += ch.len_utf16() as u32;
        idx += ch.len_utf8();
    }
    idx
}

/// The LSP `Position` (line + UTF-16 column) of a UTF-8 byte offset in `text`.
pub fn position_at(text: &str, off: usize) -> Position {
    let off = off.min(text.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, b) in text[..off].bytes().enumerate() {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let mut character = 0u32;
    for ch in text[line_start..off].chars() {
        character += ch.len_utf16() as u32;
    }
    Position { line, character }
}

/// Find the byte offset just inside `needle`'s first occurrence in `text`
/// (test helper, kept here so both test modules can use it).
#[cfg(test)]
pub(crate) fn find_at(text: &str, needle: &str) -> usize {
    text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"))
}

/// Convert a document-file span to an LSP range.
pub fn span_to_range(text: &str, span: Span) -> Range {
    Range {
        start: position_at(text, span.lo.to_usize()),
        end: position_at(text, span.hi.to_usize()),
    }
}

// --------------------------------------------------------------------------
// Completion helpers.
// --------------------------------------------------------------------------

/// What the editor is asking us to complete when the cursor is in `text` at
/// byte offset `off`. The lexer is fast and exact, but we need only a few
/// bytes of local context here — so do the look-back inline.
#[derive(Debug, PartialEq, Eq)]
pub struct DotContext {
    /// Byte offset of the `.` itself.
    pub dot_offset: usize,
    /// `Some(start..end)` if the receiver text is a plain identifier (for
    /// recognising `Type.method` static-method completion). `None` when the
    /// receiver is a more complex expression (e.g. `foo().`, `xs[0].`).
    pub receiver_ident: Option<(usize, usize)>,
}

/// Decide whether `off` sits in a `recv.|` (member-access) position.
///
/// We accept both the just-typed-`.` case and the in-progress `recv.par|`
/// case where the user is filtering the suggestion list by typing a partial
/// member name.
pub fn dot_completion_context(text: &str, off: usize) -> Option<DotContext> {
    let bytes = text.as_bytes();
    let mut walk = off.min(bytes.len());
    // Skip over the identifier currently being typed at the cursor.
    while walk > 0 && is_ident_byte(bytes[walk - 1]) {
        walk -= 1;
    }
    if walk == 0 || bytes[walk - 1] != b'.' {
        return None;
    }
    let dot_offset = walk - 1;
    let mut recv_end = dot_offset;
    // Strip whitespace between the receiver and the `.` (rare but tolerated).
    while recv_end > 0 && matches!(bytes[recv_end - 1], b' ' | b'\t') {
        recv_end -= 1;
    }
    let receiver_ident = if recv_end > 0 && is_ident_byte(bytes[recv_end - 1]) {
        let mut start = recv_end;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
        // Ensure we don't consume a digit-led "ident" — those are numeric
        // literals, not identifiers.
        if !bytes[start].is_ascii_digit() {
            Some((start, recv_end))
        } else {
            None
        }
    } else {
        None
    };
    Some(DotContext { dot_offset, receiver_ident })
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl Compiled {
    /// The type of the expression that immediately precedes `dot_off`, if any
    /// — i.e. the receiver in `receiver.member`. Picks the LARGEST recorded
    /// expression span ending exactly at the dot, so `a.b.c.|` returns the
    /// type of the full `a.b.c` chain.
    pub fn receiver_type_at_dot(&self, dot_off: usize) -> Option<Ty> {
        let mut best: Option<(Span, Ty)> = None;
        for &(span, ty) in &self.index.expr_types {
            if span.file != DOC_FILE || span.hi.to_usize() != dot_off {
                continue;
            }
            if best.is_none_or(|(b, _)| span.len() > b.len()) {
                best = Some((span, ty));
            }
        }
        best.map(|(_, ty)| ty)
    }

    /// Look up a top-level type def in the document's module by name.
    pub fn lookup_type_def(&self, name: &str) -> Option<DefId> {
        // Document file lives in ROOT until multi-module support lands.
        self.analysis.program.resolve_type_in(
            compiler::ids::ModId::ROOT,
            name,
        )
    }

    /// Every direct field of a struct/extern-struct def, in declaration order.
    pub fn struct_fields(&self, struct_def: DefId) -> Vec<&Def> {
        let prog = &self.analysis.program;
        prog.defs
            .iter()
            .filter(|d| d.kind == DefKind::Field && d.parent == Some(struct_def))
            .collect()
    }

    /// Every `extend` method whose target's top-level name matches `type_name`.
    /// `want_static` selects instance vs static methods. Methods include both
    /// public and private — visibility is enforced by the checker elsewhere;
    /// at completion time we suggest everything that lives in scope.
    pub fn extend_methods_for(&self, type_name: &str, want_static: bool) -> Vec<&Def> {
        let prog = &self.analysis.program;
        prog.defs
            .iter()
            .filter(|d| d.kind == DefKind::ExtendMethod && d.is_static == want_static)
            .filter(|d| {
                let Some(parent_id) = d.parent else { return false };
                let Some(ItemKind::Extend(e)) = &prog.def(parent_id).item else {
                    return false;
                };
                top_named_name(&e.target).map_or(false, |n| n == type_name)
            })
            .collect()
    }

    /// All interface methods declared inside `iface_def`.
    pub fn interface_methods(&self, iface_def: DefId, want_static: bool) -> Vec<&Def> {
        let prog = &self.analysis.program;
        prog.defs
            .iter()
            .filter(|d| {
                d.kind == DefKind::InterfaceMethod
                    && d.parent == Some(iface_def)
                    && d.is_static == want_static
            })
            .collect()
    }

    /// Render a function-like def's signature for completion `detail`/hover.
    /// Falls back to the def kind's name when the item is not a function.
    pub fn def_signature(&self, def: &Def) -> String {
        use compiler::ast::ParamKind;
        let Some(ItemKind::Function(f)) = &def.item else {
            return def.kind.describe().to_string();
        };
        let mut s = String::new();
        s.push('(');
        let mut first = true;
        for p in &f.params {
            if matches!(p.kind, ParamKind::SelfParam) {
                continue;
            }
            if !first {
                s.push_str(", ");
            }
            first = false;
            if let ParamKind::Normal { name, ty } = &p.kind {
                s.push_str(&name.name);
                s.push_str(": ");
                s.push_str(&render_type(&self.map, ty));
            }
        }
        s.push(')');
        if let Some(rt) = &f.return_type {
            s.push_str(": ");
            s.push_str(&render_type(&self.map, rt));
        }
        s
    }
}

/// The top-level type-name of a syntactic type, if it is a `Named` reference.
/// Looks through paren grouping but not unions/tuples/function-types.
fn top_named_name(t: &compiler::ast::Type) -> Option<&str> {
    match &t.kind {
        TypeKind::Named { name, .. } => Some(&name.name),
        TypeKind::Paren(inner) => top_named_name(inner),
        _ => None,
    }
}

/// Render a syntactic type back to its source slice — perfect-fidelity is
/// not required; we just want a readable hint for completion `detail`.
fn render_type(map: &SourceMap, t: &compiler::ast::Type) -> String {
    if t.span.file == DOC_FILE {
        map.slice(t.span).to_string()
    } else {
        // Prelude/derive-synthesised types have no editor-visible source; we
        // fall back to the structural kind which is still informative.
        match &t.kind {
            TypeKind::Named { name, .. } => name.name.clone(),
            TypeKind::Tuple(_) => "tuple".into(),
            TypeKind::Function { .. } => "function".into(),
            TypeKind::ExternFunction { .. } => "extern function".into(),
            TypeKind::Union(_) => "union".into(),
            TypeKind::Pointer(_) => "pointer".into(),
            TypeKind::Array { .. } => "array".into(),
            TypeKind::SelfType => "Self".into(),
            TypeKind::Paren(inner) => render_type(map, inner),
        }
    }
}

/// Intrinsic methods on built-in types: `(name, signature)`. Kept in lock-step
/// with the matching arms in `compiler::sema::check::builtins`.
pub fn list_intrinsic_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("push", "(value: E)"),
        ("size", "(): i64"),
        ("is_empty", "(): bool"),
        ("get", "(index: i64): E | null"),
        ("set", "(index: i64, value: E)"),
        ("clear", "()"),
        ("pop", "(): E | null"),
        ("insert", "(index: i64, value: E)"),
        ("remove", "(index: i64): E | null"),
        ("truncate", "(n: i64)"),
        ("contains", "(value: E): bool"),
        ("index_of", "(value: E): i64 | null"),
        ("iter", "(): Iterator<E>"),
        ("map", "((E) => U): List<U>"),
        ("filter", "((E) => bool): List<E>"),
        ("each", "((E) => null)"),
        ("fold", "(init, (acc, E) => acc): acc"),
        ("clone", "(): List<E>"),
    ]
}

pub fn map_intrinsic_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("size", "(): i64"),
        ("is_empty", "(): bool"),
        ("contains", "(key: K): bool"),
        ("get", "(key: K): V | null"),
        ("set", "(key: K, value: V)"),
        ("remove", "(key: K): V | null"),
        ("clear", "()"),
        ("keys", "(): List<K>"),
        ("values", "(): List<V>"),
        ("clone", "(): Map<K, V>"),
    ]
}

pub fn str_intrinsic_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("size", "(): i64"),
        ("byte_size", "(): i64"),
        ("is_empty", "(): bool"),
        ("contains", "(needle: str): bool"),
        ("starts_with", "(prefix: str): bool"),
        ("ends_with", "(suffix: str): bool"),
        ("substring", "(start: i64, end: i64): str"),
        ("to_upper", "(): str"),
        ("to_lower", "(): str"),
        ("trim", "(): str"),
        ("repeat", "(n: i64): str"),
        ("replace", "(old: str, new: str): str"),
        ("index_of", "(needle: str): i64 | null"),
        ("split", "(sep: str): List<str>"),
        ("get", "(index: i64): char | null"),
        ("clone", "(): str"),
    ]
}

pub fn primitive_static_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("MIN", "(constant)"),
        ("MAX", "(constant)"),
        ("wrapping_add", "(a, b)"),
        ("wrapping_sub", "(a, b)"),
        ("wrapping_mul", "(a, b)"),
        ("saturating_add", "(a, b)"),
        ("saturating_sub", "(a, b)"),
        ("saturating_mul", "(a, b)"),
        ("checked_add", "(a, b)"),
        ("checked_sub", "(a, b)"),
        ("checked_mul", "(a, b)"),
        ("overflowing_add", "(a, b)"),
        ("overflowing_sub", "(a, b)"),
        ("overflowing_mul", "(a, b)"),
    ]
}

pub fn float_static_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("INFINITY", "(constant)"),
        ("NEG_INFINITY", "(constant)"),
        ("NAN", "(constant)"),
    ]
}

pub fn float_instance_methods() -> &'static [(&'static str, &'static str)] {
    &[
        ("is_nan", "(): bool"),
        ("is_infinite", "(): bool"),
        ("is_finite", "(): bool"),
        ("clone", "(): f64"),
    ]
}

pub fn int_instance_methods() -> &'static [(&'static str, &'static str)] {
    &[("clone", "(): Self")]
}

/// Every reserved keyword's source text, for completion. Kept in sync with
/// `compiler::token::Keyword`.
pub fn keyword_texts() -> &'static [&'static str] {
    &[
        "var", "function", "struct", "interface", "type", "mod", "extend", "extern", "import",
        "pub", "async", "spawn", "self", "Self", "if", "else", "match", "return", "for", "in",
        "while", "loop", "break", "continue", "await", "as", "is", "true", "false", "null",
        "yield",
    ]
}

/// A precomputed line-start table for fast byte-offset → `Position` conversion
/// — used on the semantic-tokens hot path, which converts every token.
pub struct LineIndex {
    /// Byte offset of the start of each line (always begins with 0).
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }

    /// The LSP `Position` of byte offset `off` (UTF-16 column).
    pub fn position(&self, text: &str, off: usize) -> Position {
        let off = off.min(text.len());
        let line = match self.starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.starts[line];
        let mut character = 0u32;
        for ch in text[line_start..off].chars() {
            character += ch.len_utf16() as u32;
        }
        Position { line: line as u32, character }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler::sema::ValueRes;

    const PROG: &str = "\
function add(a: i64, b: i64): i64 { a + b }
function main() {
  var total = add(1, 2);
  var y = total;
}
";

    fn second(text: &str, needle: &str) -> usize {
        let first = text.find(needle).unwrap();
        first + needle.len() + text[first + needle.len()..].find(needle).unwrap()
    }

    #[test]
    fn offset_position_roundtrip_with_unicode() {
        let text = "// café\nfunction main() {}\n";
        // Pick the offset of `main`.
        let off = text.find("main").unwrap();
        let pos = position_at(text, off);
        assert_eq!(pos.line, 1);
        assert_eq!(offset_at(text, pos), off);
        // A column past the (UTF-16-short) accented line clamps to its end.
        let p = Position { line: 0, character: 100 };
        let o = offset_at(text, p);
        assert_eq!(&text[..o], "// café");
    }

    #[test]
    fn diagnostics_report_type_errors_in_range() {
        let c = Compiled::new("function main() { var x: i64 = \"s\"; }".into());
        assert!(!c.diagnostics.is_empty(), "expected a type error");
        let (span, _) = c.diagnostics[0];
        assert_eq!(c.map.slice(span), "\"s\"");
    }

    #[test]
    fn clean_program_has_no_diagnostics() {
        let c = Compiled::new(PROG.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn multi_file_import_resolves_submodule() {
        // `main.otter` imports from a `util` submodule. With submodule loading
        // the import resolves and the program has no diagnostics; without it the
        // checker would report "cannot find module `util`".
        let main = "mod util;\n\
                    import { double } from \"self:util\";\n\
                    function main() { var x = double(21); }";
        let util = "pub function double(x: i64): i64 { x * 2 }";
        let read = |p: &Path| -> Option<String> {
            if p.ends_with("util.otter") {
                Some(util.to_string())
            } else {
                None
            }
        };
        let c = Compiled::new_multi(
            main.into(),
            std::path::PathBuf::from("/proj/src"),
            "main".to_string(),
            &read,
        );
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn missing_submodule_does_not_panic() {
        // A `mod` whose file cannot be read leaves the import unresolved (a
        // diagnostic against the open doc) but must not crash the server.
        let main = "mod util;\n\
                    import { double } from \"self:util\";\n\
                    function main() { var x = double(21); }";
        let read = |_: &Path| -> Option<String> { None };
        let c = Compiled::new_multi(
            main.into(),
            std::path::PathBuf::from("/proj/src"),
            "main".to_string(),
            &read,
        );
        // It analysed (no panic); the unresolved import surfaces as a diagnostic.
        assert!(!c.diagnostics.is_empty());
    }

    #[test]
    fn resolution_and_goto_for_function_call() {
        let c = Compiled::new(PROG.into());
        let off = find_at(PROG, "add(1");
        let (_, res) = c.resolution_at(off).expect("resolution at call");
        assert!(matches!(res, ValueRes::Function(_)));
        let def = c.definition_span(res).expect("def span");
        // Definition points at the `add` in `function add`.
        assert_eq!(c.map.slice(def), "add");
        assert_eq!(def.lo.to_usize(), PROG.find("add").unwrap());
    }

    #[test]
    fn cross_file_goto_resolves_into_submodule() {
        // Goto-definition on a call to an imported function resolves into the
        // submodule's file (a non-`DOC_FILE` span the server maps to that file).
        let main = "mod util;\n\
                    import { double } from \"self:util\";\n\
                    function main() { var x = double(21); }";
        let util = "pub function double(x: i64): i64 { x * 2 }";
        let read = |p: &Path| -> Option<String> {
            if p.ends_with("util.otter") { Some(util.to_string()) } else { None }
        };
        let c = Compiled::new_multi(
            main.into(),
            std::path::PathBuf::from("/proj/src"),
            "main".to_string(),
            &read,
        );
        let off = main.find("double(21)").unwrap();
        let (_, res) = c.resolution_at(off).expect("resolution at call");
        let def = c.definition_span(res).expect("def span");
        // The definition lives in the submodule file (not the open document).
        assert_ne!(def.file, DOC_FILE);
        assert!((def.file.0 as usize) < c.map.file_count());
        let sf = c.map.file(def.file);
        assert!(sf.name.ends_with("util.otter"), "got: {}", sf.name);
        assert_eq!(&sf.src[def.lo.to_usize()..def.hi.to_usize()], "double");
    }

    #[test]
    fn type_position_goto_resolves_to_struct_def() {
        // Goto on a type name in an annotation (a param type, a return type, a
        // field type) jumps to the type's definition — even though type-position
        // names are not value resolutions in the HIR index.
        let src = "struct Point { x: i64, y: i64 }\n\
                   struct Line { from: Point, to: Point }\n\
                   function mid(a: Point, b: Point): Point { a }\n\
                   function main() {}";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
        // Cursor on `Point` in the `from: Point` field type.
        let field_use = src.find("from: Point").unwrap() + "from: ".len();
        let def = c.type_def_span_at(field_use).expect("type def span (field)");
        assert_eq!(c.map.slice(def), "Point");
        assert_eq!(def.lo.to_usize(), src.find("Point").unwrap());
        // Cursor on `Point` in the `mid` return type also resolves.
        let ret_use = src.find("): Point").unwrap() + "): ".len();
        let def2 = c.type_def_span_at(ret_use).expect("type def span (return)");
        assert_eq!(c.map.slice(def2), "Point");
        // The hover path resolves the same name to a struct definition.
        let (_, def_id) = c.type_def_at(field_use).expect("type def at field");
        assert_eq!(c.analysis.program.def(def_id).kind, DefKind::Struct);
    }

    #[test]
    fn cross_file_references_span_multiple_files() {
        // `double` is defined in util.otter, called in main.otter, and also called
        // by `quad` *within* util.otter — so "find references" must surface use
        // sites in BOTH files (the index spans every analyzed module).
        let main = "mod util;\n\
                    import { quad } from \"self:util\";\n\
                    import { double } from \"self:util\";\n\
                    function main() { var x = double(21) + quad(10); }";
        let util = "pub function double(x: i64): i64 { x * 2 }\n\
                    pub function quad(x: i64): i64 { double(double(x)) }";
        let read = |p: &Path| -> Option<String> {
            if p.ends_with("util.otter") { Some(util.to_string()) } else { None }
        };
        let c = Compiled::new_multi(
            main.into(),
            std::path::PathBuf::from("/proj/src"),
            "main".to_string(),
            &read,
        );
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
        // Resolve `double` at its call in main, then mirror the server's
        // cross-file reference collection.
        let off = main.find("double(21)").unwrap();
        let (_, target) = c.resolution_at(off).expect("resolution at call");
        let files: std::collections::HashSet<u32> = c
            .index
            .resolutions
            .iter()
            .filter(|(_, r)| *r == target)
            .map(|(s, _)| s.file.0)
            .collect();
        // Uses appear in the open document (main) and the submodule (util).
        assert!(files.len() >= 2, "references should span ≥2 files, got {files:?}");
        assert!(files.contains(&DOC_FILE.0), "missing the open-document use site");
        let util_fid = (0..c.map.file_count() as u32)
            .find(|&i| c.map.file(FileId(i)).name.ends_with("util.otter"))
            .expect("util file loaded");
        assert!(files.contains(&util_fid), "missing the cross-file (util) use sites");
    }

    #[test]
    fn macro_program_analyzes_without_spurious_errors() {
        // The LSP expands user macros before analysis, so a defined macro's
        // invocation type-checks and the generated method is visible (`docs/22`).
        let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
            @ProcMacro\n\
            pub function Tag(ctx: MacroContext, input: ASTNode): ASTNode {\n\
              ctx.parse_items(input.text() + \" extend \" + input.name() + \" { function tag(self): i64 { 1 } }\")\n\
            }\n\
            @Tag\n\
            struct W { x: i64 }\n\
            function main() { var w = W { x: 0 }; var t = w.tag(); }\n";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected diagnostics: {:?}", c.diagnostics);
        assert_eq!(c.proc_macro_names(), vec!["Tag".to_string()]);
    }

    #[test]
    fn macro_using_std_is_flagged_by_lsp() {
        let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
            import { println } from \"std:io\";\n\
            @ProcMacro\n\
            pub function Bad(ctx: MacroContext, input: ASTNode): ASTNode { println(\"x\"); input }\n\
            @Bad\n\
            struct W { x: i64 }\n\
            function main() {}\n";
        let c = Compiled::new(src.into());
        assert!(
            c.diagnostics.iter().any(|(_, m)| m.contains("sandboxed")),
            "expected sandbox diagnostic: {:?}",
            c.diagnostics
        );
    }

    #[test]
    fn resolution_and_goto_for_local() {
        let c = Compiled::new(PROG.into());
        let off = second(PROG, "total");
        let (_, res) = c.resolution_at(off).expect("resolution at local use");
        assert!(matches!(res, ValueRes::Local(_)));
        let def = c.definition_span(res).expect("local decl span");
        // Goes back to the first `total` (the `var total` binding).
        assert_eq!(def.lo.to_usize(), PROG.find("total").unwrap());
    }

    #[test]
    fn expr_type_at_literal_and_callee() {
        let c = Compiled::new(PROG.into());
        // The `1` argument literal is an integer expression.
        let lit = find_at(PROG, "1, 2");
        let (_, ty) = c.expr_ty_at(lit).expect("expr type at literal");
        assert_eq!(c.display_ty(ty), "i64");
        // Hovering the callee name shows its function type.
        let callee = find_at(PROG, "add(1");
        let (_, fty) = c.expr_ty_at(callee).expect("expr type at callee");
        assert_eq!(c.display_ty(fty), "(i64, i64) => i64");
    }

    #[test]
    fn semantic_tokens_classify_identifiers_but_not_grammar_handled_tokens() {
        let c = Compiled::new(PROG.into());
        let toks = c.semantic_tokens();
        // Keywords are intentionally not emitted — the TextMate grammar colors
        // them with richer sub-scopes.
        assert!(toks.iter().all(|(s, _)| c.map.slice(*s) != "function"));
        // Numeric literals are likewise grammar-handled.
        assert!(toks.iter().all(|(s, _)| c.map.slice(*s) != "1"));
        // The `add` call site is classified as a function (the LSP knows it
        // resolves to a function definition).
        assert!(toks.iter().any(|(s, k)| {
            c.map.slice(*s) == "add"
                && *k == TokenClass::Function
                && s.lo.to_usize() == PROG.find("add(1").unwrap()
        }));
        // The `total` local use is classified as a variable.
        assert!(toks
            .iter()
            .any(|(s, k)| c.map.slice(*s) == "total" && *k == TokenClass::Variable));
        // Parameters get their own class.
        assert!(toks
            .iter()
            .any(|(s, k)| c.map.slice(*s) == "a" && *k == TokenClass::Parameter));
        // Type-position primitives are classified as Type (not Variable) — the
        // checker only records value-position resolutions, so without this
        // recovery `i64` in `a: i64` would otherwise paint with the variable
        // color and override the grammar's primitive scope.
        assert!(toks
            .iter()
            .any(|(s, k)| c.map.slice(*s) == "i64" && *k == TokenClass::Type));
    }

    #[test]
    fn semantic_tokens_classify_unresolved_uppercase_as_type() {
        // `T` is a generic param (not a value resolution and not a top-level
        // declared name), so the fallback path must still classify it as Type
        // — otherwise it would render with the variable color.
        let src = "function id<T>(x: T): T { x }\n";
        let c = Compiled::new(src.into());
        let toks = c.semantic_tokens();
        let t_count = toks
            .iter()
            .filter(|(s, k)| c.map.slice(*s) == "T" && *k == TokenClass::Type)
            .count();
        assert!(t_count >= 3, "expected each `T` to be Type, got {t_count}");
    }

    #[test]
    fn async_thread_spawn_hovers_as_joinhandle_of_awaited_output() {
        // An async `Thread.spawn` worker `() => Future<R>` yields `JoinHandle<R>`
        // (the awaited `R`, not `Future<R>`) — docs/20 §1. Hovering the call must
        // surface that signature, so the editor mirrors the checker.
        let src = "\
import { Future } from \"core:prelude\";
import { Thread, JoinHandle, Joined, Panicked } from \"std:thread\";
function main(): Future<null> async {
  var h = Thread.spawn(() async => { 1 + 2 });
  var _ = await h.join();
}
";
        let c = Compiled::new(src.into());
        let off = find_at(src, "Thread.spawn(");
        let (_, ty) = c.expr_ty_at(off).expect("expr type at async Thread.spawn");
        assert_eq!(c.display_ty(ty), "JoinHandle<i64>");
    }

    #[test]
    fn async_closure_hovers_as_future_returning_callable() {
        // An async closure `(p) async => E` / `function(p): Future<T> async { … }`
        // has the value type `(p) => Future<T>`: hovering it surfaces a callable
        // returning a `Future`, not a bare `Future` (docs/21 §7). The LSP walks the
        // post-desugar HIR, so both surface forms render identically.
        let src = "\
import { Future } from \"core:prelude\";
import { sleep } from \"std:async\";
function main(): Future<null> async {
  var base: i64 = 1;
  var f = function(x: i64): Future<i64> async { var _ = await sleep(1); base + x };
  var g = (n: i64): Future<i64> async => base * n;
  var _ = await f(2);
  var _ = await g(3);
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
        // The `function(..) async { }` form.
        let foff = find_at(src, "function(x: i64)");
        let (_, fty) = c.expr_ty_at(foff).expect("expr type at async closure");
        assert_eq!(c.display_ty(fty), "(i64) => Future<i64>");
        // The arrow `(p) async => E` form renders identically.
        let goff = find_at(src, "(n: i64): Future<i64> async =>");
        let (_, gty) = c.expr_ty_at(goff).expect("expr type at async arrow closure");
        assert_eq!(c.display_ty(gty), "(i64) => Future<i64>");
    }

    #[test]
    fn semantic_tokens_classify_prelude_types_and_primitives_in_type_position() {
        // Mirrors a realistic snippet (see examples/threads.otter): in type
        // position, `JoinHandle`, `Future`, `i64`, and `Thread` must all emit
        // as Type so the theme can paint them like types, not variables.
        let src = "\
function main(): Future<null> async {
  var a: JoinHandle<i64> = Thread.spawn(() => 0);
}
";
        let c = Compiled::new(src.into());
        let toks = c.semantic_tokens();
        for name in ["JoinHandle", "Future", "Thread", "i64"] {
            assert!(
                toks.iter()
                    .any(|(s, k)| c.map.slice(*s) == name && *k == TokenClass::Type),
                "expected `{name}` to be classified as Type, tokens were: {:?}",
                toks.iter()
                    .map(|(s, k)| (c.map.slice(*s), *k))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn line_index_matches_position_at() {
        let text = "abc\nde\nfghij\n";
        let idx = LineIndex::new(text);
        for off in 0..=text.len() {
            assert_eq!(idx.position(text, off), position_at(text, off), "off={off}");
        }
    }

    #[test]
    fn dot_context_detects_member_access() {
        let text = "var p = q.foo";
        let off = text.len(); // cursor at end
        let ctx = dot_completion_context(text, off).expect("dot context");
        assert_eq!(&text[ctx.dot_offset..=ctx.dot_offset], ".");
        let (s, e) = ctx.receiver_ident.expect("receiver ident");
        assert_eq!(&text[s..e], "q");
    }

    #[test]
    fn dot_context_handles_cursor_right_after_dot() {
        let text = "thing.";
        let ctx = dot_completion_context(text, text.len()).expect("dot context");
        assert_eq!(ctx.dot_offset, 5);
        let (s, e) = ctx.receiver_ident.unwrap();
        assert_eq!(&text[s..e], "thing");
    }

    #[test]
    fn dot_context_is_none_without_dot() {
        assert!(dot_completion_context("plain_ident", 5).is_none());
    }

    #[test]
    fn completion_keywords_match_compiler_lexer() {
        // Lock the completion list to the compiler's keyword enum so a new
        // keyword can't be added in one place and forgotten in the other.
        // If this list grows, update `keyword_texts` (and the TextMate grammar).
        let expected = [
            "var", "function", "struct", "interface", "type", "mod", "extend", "extern", "import",
            "pub", "async", "spawn", "self", "Self", "if", "else", "match", "return", "for", "in",
            "while", "loop", "break", "continue", "await", "as", "is", "true", "false", "null",
            "yield",
        ];
        for kw in expected {
            assert!(
                compiler::token::Keyword::from_str(kw).is_some(),
                "{kw} not a lexer keyword"
            );
            assert!(
                keyword_texts().contains(&kw),
                "{kw} missing from completion list"
            );
        }
    }

    #[test]
    fn receiver_type_resolves_struct_field_chain() {
        // Type-checking infers types for every sub-expression, including the
        // receiver chain — so a dot-completion lookup at the trailing `.`
        // finds the right struct type to suggest members on.
        let src = "\
struct Point { x: i64, y: i64 }
function main() {
  var p = Point { x: 1, y: 2 };
  var q = p.x;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("p.x").unwrap() + 1; // offset of `.`
        let ty = c.receiver_type_at_dot(dot).expect("receiver type");
        assert_eq!(c.display_ty(ty), "Point");
    }

    // --- HIR-backed position queries (Stage 4) -----------------------------
    // These exercise the provenance reconstructed from the typed HIR for forms
    // the desugaring folds away (method calls, constructors), proving the LSP
    // no longer needs the checker's span-keyed `resolutions`/`expr_types`.

    const METHODS: &str = "\
struct Counter { n: i64 }
extend Counter {
  function bump(self): i64 { self.n + 1 }
}
function main() {
  var c = Counter { n: 1 };
  var r = c.bump();
}
";

    #[test]
    fn goto_definition_on_method_call_name() {
        // `c.bump()` desugars to `Call { kind: Method }` with no callee `Name`
        // node — the HIR carries `callee_span` so go-to-definition still works.
        let c = Compiled::new(METHODS.into());
        let off = find_at(METHODS, "bump()");
        let (_, res) = c.resolution_at(off).expect("resolution at method call");
        assert!(matches!(res, ValueRes::Method(_)), "got {res:?}");
        let def = c.definition_span(res).expect("method def span");
        // Jumps to the `bump` in `function bump`.
        assert_eq!(c.map.slice(def), "bump");
        assert_eq!(def.lo.to_usize(), METHODS.find("bump(self)").unwrap());
    }

    #[test]
    fn references_to_function_include_call_site() {
        // A direct call folds the callee into `CallKind::Direct`; the HIR's
        // `callee_span` re-exposes the `add` use site for find-references.
        let c = Compiled::new(PROG.into());
        let target = {
            let off = find_at(PROG, "function add");
            // resolution at the declaration name is not recorded; resolve via
            // the call site instead.
            let call = find_at(PROG, "add(1");
            let _ = off;
            c.resolution_at(call).expect("call resolution").1
        };
        assert!(matches!(target, ValueRes::Function(_)));
        // The call-site span is present in the HIR resolution index.
        let call_off = find_at(PROG, "add(1");
        let hit = c
            .index
            .resolutions
            .iter()
            .any(|(s, r)| *r == target && s.lo.to_usize() == call_off);
        assert!(hit, "expected the call site in the HIR resolution index");
    }

    #[test]
    fn struct_constructor_call_resolves_to_ctor() {
        // `Counter { .. }` is a record literal (resolved on its own), but a
        // tuple/unit constructor *call* folds into `CallKind::TupleCtor`. Verify
        // a positional constructor call resolves through the HIR callee span.
        let src = "\
struct Pair(i64, i64)
function main() {
  var p = Pair(1, 2);
}
";
        let c = Compiled::new(src.into());
        let off = find_at(src, "Pair(1");
        let (_, res) = c.resolution_at(off).expect("ctor resolution");
        assert!(matches!(res, ValueRes::StructCtor(_)), "got {res:?}");
    }

    #[test]
    fn local_hover_type_comes_from_hir_index() {
        // The local's type for hover is read from the HIR index (every body's
        // `locals`), not a `CheckResults` table.
        let c = Compiled::new(PROG.into());
        // The *use* site `var y = total;` (the binding occurrence is not a
        // resolution — only uses are).
        let off = second(PROG, "total");
        let (_, res) = c.resolution_at(off).expect("local resolution");
        let ValueRes::Local(id) = res else { panic!("expected a local, got {res:?}") };
        let ty = *c.index.local_types.get(&id).expect("local type in index");
        assert_eq!(c.display_ty(ty), "i64");
    }

    #[test]
    fn callee_hover_type_is_function_type() {
        // Hovering the callee name yields the *function* type (callee_ty), not
        // the call's result type — provenance carried on the HIR `Call` node.
        let c = Compiled::new(PROG.into());
        let callee = find_at(PROG, "add(1");
        let (_, fty) = c.expr_ty_at(callee).expect("callee type");
        assert_eq!(c.display_ty(fty), "(i64, i64) => i64");
        // While the whole call expression's type is the result type.
        let whole = find_at(PROG, "add(1, 2)");
        // The smallest span containing the result-type position is the call.
        let _ = whole;
    }
}

#[cfg(test)]
mod fmt_integration_tests {
    use super::*;

    #[test]
    fn formatting_reindents_and_is_token_safe() {
        // The LSP formatting handler formats the open buffer via `compiler::fmt`
        // and returns a whole-document edit. Exercise that path's pieces.
        let src = "function main() {\nvar x = 1;\n  if x > 0 {\nx = 2;\n}\n}\n";
        let c = Compiled::new(src.into());
        let formatted = compiler::fmt::format_source(&c.text);
        assert_eq!(
            formatted,
            "function main() {\n  var x = 1;\n  if x > 0 {\n    x = 2;\n  }\n}\n"
        );
        // The reformat must be token-preserving (the handler declines otherwise).
        assert!(compiler::fmt::token_stream_preserved(&c.text, &formatted));
        // The whole-document edit end position is the text's end.
        let end = position_at(&c.text, c.text.len());
        assert_eq!(end.line, 6); // six newlines → line index 6 at EOF
    }

    #[test]
    fn formatting_already_formatted_is_noop() {
        let src = "function main() {\n  var x = 1;\n}\n";
        let c = Compiled::new(src.into());
        assert_eq!(compiler::fmt::format_source(&c.text), c.text);
    }
}

#[cfg(test)]
mod lint_diag_tests {
    use super::*;

    #[test]
    fn lint_warnings_available_for_open_doc() {
        // The server publishes these (as WARNING diagnostics) when the program
        // is error-free; here we exercise the underlying analysis.
        // Self-contained (no imports needed): `dead` is unused, `k` is returned.
        let c = Compiled::new("function f(): i64 { var dead = 5; var k = 1; k }".into());
        assert!(c.diagnostics.is_empty(), "program should be error-free: {:?}", c.diagnostics);
        let warns = compiler::lint::collect_lints(&c.analysis, &c.map);
        assert!(
            warns.iter().any(|(_, m)| m.contains("unused variable `dead`")),
            "expected an unused-variable warning; got: {warns:?}"
        );
    }
}

#[cfg(test)]
mod code_action_tests {
    use super::*;

    #[test]
    fn unused_var_quickfix_targets_the_binding() {
        // The "prefix `_`" code action inserts at the binding's start; here we
        // confirm the lint reports the binding span the action would edit.
        let c = Compiled::new("function f(): i64 { var unused = 5; var k = 1; k }".into());
        let l = compiler::lint::analyze(&c.analysis, &c.map);
        let (span, name) = l.unused_locals.iter().find(|(_, n)| n == "unused").expect("unused var");
        assert_eq!(c.map.slice(*span), "unused");
        // The insertion point is the binding's start column.
        let at = span_to_range(&c.text, *span).start;
        assert_eq!(c.text.lines().nth(at.line as usize).unwrap().as_bytes()[at.character as usize], b'u');
        let _ = name;
    }
}
