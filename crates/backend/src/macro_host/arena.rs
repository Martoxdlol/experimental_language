//! The macro host arena (`docs/22`).
//!
//! A procedural macro is a function written in the language, JIT-compiled and
//! run at compile time (phase 2). It manipulates the AST through the
//! `core:compiler` surface (`ASTNode`/`MacroContext`/`Span`), whose methods are
//! thin wrappers over the `__ast_*` / `__mctx_*` extern host functions in
//! [`crate::host`]. Those host functions operate on *opaque handles* — indices
//! into the per-thread [`MacroState`] defined here. The language never sees a
//! raw Rust AST; it only ever holds `i64` handles.
//!
//! The state is thread-local because the macro JIT runs synchronously on the
//! calling thread and the host functions are invoked re-entrantly during that
//! call. One [`MacroCtx`] is active per running macro invocation.

use compiler::ast::*;
use compiler::span::{BytePos, FileId, Span};
use std::cell::RefCell;

/// An AST fragment addressed by an opaque handle. The macro author only ever
/// observes the handle (wrapped in the language-level `ASTNode` struct); the
/// real node lives here.
#[derive(Clone, Debug)]
pub enum Node {
    /// A single top-level item (the decorator-form input, or a `parse_item`
    /// result).
    Item(Item),
    /// A sequence of items (a decorator macro that expands to several
    /// declarations, or a `parse_items` result).
    Items(Vec<Item>),
    /// An expression (an invocation argument, or a `parse_expr` result).
    Expr(Expr),
    /// A block of statements (the block-form input, or a `parse_block` result).
    Block(Block),
    /// A bare identifier (a `fresh_ident` / `unhygienic` result).
    Ident(Ident),
    /// `ASTNode.error_marker()` — the macro reported a problem itself; the
    /// engine leaves the invocation site untouched and suppresses follow-on
    /// expansion of it.
    ErrorMarker,
}

/// Severity of a macro-emitted diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagLevel {
    Error,
    Warn,
    Note,
}

/// A diagnostic a macro raised via `ctx.error` / `warn` / `note`.
#[derive(Clone, Debug)]
pub struct MacroDiag {
    pub level: DiagLevel,
    pub span: Span,
    pub message: String,
}

/// The mutable state of one in-flight macro invocation.
pub struct MacroCtx {
    /// Span of the `@MacroName` invocation itself.
    pub invocation_span: Span,
    /// Positional argument nodes (handles into [`MacroState::nodes`]).
    pub args: Vec<usize>,
    /// Keyword argument nodes, by name.
    pub kwargs: Vec<(String, usize)>,
    /// Diagnostics accumulated during this invocation.
    pub diags: Vec<MacroDiag>,
}

impl Default for MacroCtx {
    fn default() -> Self {
        MacroCtx {
            invocation_span: Span::dummy(),
            args: Vec::new(),
            kwargs: Vec::new(),
            diags: Vec::new(),
        }
    }
}

/// Per-thread macro-expansion state: the node + span arenas, the active
/// invocation contexts, and the registry of synthetic source buffers (so
/// diagnostics that land on macro-generated code can still be rendered).
#[derive(Default)]
pub struct MacroState {
    pub nodes: Vec<Node>,
    pub spans: Vec<Span>,
    pub contexts: Vec<MacroCtx>,
    /// `(virtual FileId, source text)` for every buffer parsed from a macro
    /// (shims, `parse_*` calls). Keyed by FileId so the diagnostic renderer can
    /// recover the snippet a generated span points into.
    pub gen_sources: Vec<(FileId, String)>,
    /// Monotonic counter minting unique virtual `FileId`s for generated source.
    pub file_ctr: u32,
    /// Monotonic counter for `fresh_ident` uniqueness.
    pub fresh_ctr: u32,
}

thread_local! {
    static STATE: RefCell<MacroState> = RefCell::new(MacroState::default());
}

/// Base of the virtual-`FileId` range for macro-generated source. Chosen high
/// enough to never collide with real source files yet distinct from the
/// derive/defaults/anf synthetic file (`u32::MAX - 1`).
const MACRO_FILE_BASE: u32 = 0xF000_0000;

/// Run `f` with mutable access to the per-thread macro state.
pub fn with<R>(f: impl FnOnce(&mut MacroState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Reset the arena between top-level program expansions (keeps the thread-local
/// from growing without bound across many compiles in one process, e.g. the
/// LSP or the test harness).
pub fn reset() {
    with(|s| *s = MacroState::default());
}

impl MacroState {
    /// Intern a node, returning its handle.
    pub fn push_node(&mut self, n: Node) -> i64 {
        self.nodes.push(n);
        (self.nodes.len() - 1) as i64
    }

    /// Intern a span, returning its handle.
    pub fn push_span(&mut self, sp: Span) -> i64 {
        self.spans.push(sp);
        (self.spans.len() - 1) as i64
    }

    pub fn node(&self, h: i64) -> Option<&Node> {
        usize::try_from(h).ok().and_then(|i| self.nodes.get(i))
    }

    pub fn span(&self, h: i64) -> Span {
        usize::try_from(h).ok().and_then(|i| self.spans.get(i)).copied().unwrap_or_else(Span::dummy)
    }

    /// Allocate a fresh virtual `FileId`, recording `src` so diagnostics that
    /// fall inside it can be rendered.
    pub fn new_gen_file(&mut self, src: String) -> FileId {
        let id = FileId(MACRO_FILE_BASE + self.file_ctr);
        self.file_ctr += 1;
        self.gen_sources.push((id, src));
        id
    }

    /// The recorded source text for a generated virtual file, if any.
    #[allow(dead_code)] // consumed by the diagnostic renderer in a later slice
    pub fn gen_source(&self, file: FileId) -> Option<&str> {
        self.gen_sources.iter().find(|(f, _)| *f == file).map(|(_, s)| s.as_str())
    }
}

// ---------------------------------------------------------------------------
// Node inspection (drives the `__ast_*` host functions)
// ---------------------------------------------------------------------------

/// The syntactic-category tag a macro sees via `node.kind()`.
pub fn node_kind(n: &Node) -> &'static str {
    match n {
        Node::Item(it) => item_kind(&it.kind),
        Node::Items(_) => "items",
        Node::Expr(e) => expr_kind(&e.kind),
        Node::Block(_) => "block",
        Node::Ident(_) => "ident",
        Node::ErrorMarker => "error",
    }
}

fn item_kind(k: &ItemKind) -> &'static str {
    match k {
        ItemKind::Var(_) => "var",
        ItemKind::Function(_) => "function",
        ItemKind::Struct(_) => "struct",
        ItemKind::Interface(_) => "interface",
        ItemKind::TypeAlias(_) => "type",
        ItemKind::Module(_) => "mod",
        ItemKind::Extend(_) => "extend",
        ItemKind::Extern(_) => "extern",
        ItemKind::Import(_) => "import",
        ItemKind::Test(_) => "test",
    }
}

fn expr_kind(k: &ExprKind) -> &'static str {
    match k {
        ExprKind::Int(_) => "int",
        ExprKind::Float(_) => "float",
        ExprKind::Bool(_) => "bool",
        ExprKind::Str(_) => "str",
        ExprKind::Char(_) => "char",
        ExprKind::Null => "null",
        ExprKind::Ident(_) => "ident",
        ExprKind::SelfExpr => "self",
        ExprKind::Call { .. } => "call",
        ExprKind::Field { .. } => "field",
        ExprKind::Binary { .. } => "binary",
        ExprKind::Unary { .. } => "unary",
        _ => "expr",
    }
}

/// The declared name of an item node (struct/function/interface/type/extend
/// target), or the spelling of an identifier expression/ident node. Empty when
/// the node has no meaningful name.
pub fn node_name(n: &Node) -> String {
    match n {
        Node::Item(it) => item_name(&it.kind),
        Node::Expr(Expr { kind: ExprKind::Ident(id), .. }) => id.name.clone(),
        Node::Ident(id) => id.name.clone(),
        _ => String::new(),
    }
}

fn item_name(k: &ItemKind) -> String {
    match k {
        ItemKind::Var(v) => v.name.name.clone(),
        ItemKind::Function(f) => f.name.name.clone(),
        ItemKind::Struct(s) => s.name.name.clone(),
        ItemKind::Interface(i) => i.name.name.clone(),
        ItemKind::TypeAlias(t) => t.name.name.clone(),
        ItemKind::Extend(e) => type_name(&e.target),
        ItemKind::Extern(_) | ItemKind::Module(_) | ItemKind::Import(_) | ItemKind::Test(_) => {
            String::new()
        }
    }
}

fn type_name(t: &Type) -> String {
    match &t.kind {
        TypeKind::Named { name, .. } => name.name.clone(),
        _ => String::new(),
    }
}

/// Field count for a struct item node (0 for non-structs / unit structs).
pub fn node_field_count(n: &Node) -> i64 {
    match n {
        Node::Item(Item { kind: ItemKind::Struct(s), .. }) => match &s.kind {
            StructKind::Unit => 0,
            StructKind::Record(f) => f.len() as i64,
            StructKind::Tuple(f) => f.len() as i64,
        },
        _ => 0,
    }
}

/// The name of field `i` of a record struct node (tuple fields render as their
/// index; out-of-range / non-struct yields empty).
pub fn node_field_name(n: &Node, i: i64) -> String {
    let Node::Item(Item { kind: ItemKind::Struct(s), .. }) = n else { return String::new() };
    let i = match usize::try_from(i) {
        Ok(i) => i,
        Err(_) => return String::new(),
    };
    match &s.kind {
        StructKind::Record(fields) => fields.get(i).map(|f| f.name.name.clone()).unwrap_or_default(),
        StructKind::Tuple(fields) => {
            if i < fields.len() {
                i.to_string()
            } else {
                String::new()
            }
        }
        StructKind::Unit => String::new(),
    }
}

pub fn node_is_record(n: &Node) -> bool {
    matches!(n, Node::Item(Item { kind: ItemKind::Struct(s), .. }) if matches!(s.kind, StructKind::Record(_)))
}
pub fn node_is_tuple(n: &Node) -> bool {
    matches!(n, Node::Item(Item { kind: ItemKind::Struct(s), .. }) if matches!(s.kind, StructKind::Tuple(_)))
}
pub fn node_is_unit(n: &Node) -> bool {
    matches!(n, Node::Item(Item { kind: ItemKind::Struct(s), .. }) if matches!(s.kind, StructKind::Unit))
}

/// The span a macro reads via `node.span()`.
pub fn node_span(n: &Node) -> Span {
    match n {
        Node::Item(it) => it.span,
        Node::Items(items) => items.first().map(|i| i.span).unwrap_or_else(Span::dummy),
        Node::Expr(e) => e.span,
        Node::Block(b) => b.span,
        Node::Ident(id) => id.span,
        Node::ErrorMarker => Span::dummy(),
    }
}

/// Source rendering of a node, via the AST pretty-printer (`node.text()`).
pub fn node_text(n: &Node) -> String {
    match n {
        Node::Item(it) => compiler::ast_print::print_item(it),
        Node::Items(items) => {
            items.iter().map(compiler::ast_print::print_item).collect::<Vec<_>>().join("\n")
        }
        Node::Expr(e) => compiler::ast_print::print_expr(e),
        Node::Block(b) => compiler::ast_print::print_block(b),
        Node::Ident(id) => id.name.clone(),
        Node::ErrorMarker => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Re-parse helpers (drive the `__mctx_parse_*` host functions)
// ---------------------------------------------------------------------------

/// Outcome of parsing a macro-built source fragment.
pub enum ParseOutcome {
    Ok(Node),
    /// Parse/lex failure: the human-readable error and the span (into the
    /// generated buffer) it occurred at.
    Err(String, Span),
}

/// Lex + parse `src` as a standalone module under a fresh virtual file. Returns
/// the parsed items plus the allocated `FileId` (registered for rendering).
fn parse_module_src(src: &str) -> Result<(Module, FileId), (String, Span)> {
    let file = with(|s| s.new_gen_file(src.to_string()));
    let (tokens, lex_errs) = compiler::lex(src, file);
    if let Some(e) = lex_errs.first() {
        return Err((format!("{e:?}"), e.span));
    }
    let (module, parse_errs) = compiler::parse(src, &tokens);
    if let Some(e) = parse_errs.first() {
        return Err((e.kind.to_string(), e.span));
    }
    Ok((module, file))
}

/// `ctx.parse_item(src)` — parse a single top-level item.
pub fn parse_item(src: &str) -> ParseOutcome {
    match parse_module_src(src) {
        Ok((m, file)) => match m.items.into_iter().next() {
            Some(it) => ParseOutcome::Ok(Node::Item(it)),
            None => ParseOutcome::Err(
                "parse_item: source contained no item".into(),
                Span::new(file, BytePos(0), BytePos(0)),
            ),
        },
        Err((msg, sp)) => ParseOutcome::Err(format!("parse_item: {msg}"), sp),
    }
}

/// `ctx.parse_items(src)` — parse zero or more top-level items.
pub fn parse_items(src: &str) -> ParseOutcome {
    match parse_module_src(src) {
        Ok((m, _)) => ParseOutcome::Ok(Node::Items(m.items)),
        Err((msg, sp)) => ParseOutcome::Err(format!("parse_items: {msg}"), sp),
    }
}

/// `ctx.parse_expr(src)` — parse a single expression (wrapped in a synthetic
/// function and lifted from its trailing position).
pub fn parse_expr(src: &str) -> ParseOutcome {
    let wrapped = format!("function __pe() {{\n({src})\n}}");
    match parse_module_src(&wrapped) {
        Ok((m, file)) => {
            let trailing = m.items.into_iter().find_map(|it| match it.kind {
                ItemKind::Function(f) => f.body.and_then(|b| b.trailing.map(|t| *t)),
                _ => None,
            });
            match trailing {
                // Strip the disambiguating wrapper paren so the macro sees the
                // real expression node (`binary`, `call`, …) rather than `expr`.
                Some(Expr { kind: ExprKind::Paren(inner), .. }) => ParseOutcome::Ok(Node::Expr(*inner)),
                Some(e) => ParseOutcome::Ok(Node::Expr(e)),
                None => ParseOutcome::Err(
                    "parse_expr: source was not a single expression".into(),
                    Span::new(file, BytePos(0), BytePos(0)),
                ),
            }
        }
        Err((msg, sp)) => ParseOutcome::Err(format!("parse_expr: {msg}"), sp),
    }
}

/// `ctx.parse_block(src)` — parse a brace-free statement sequence as a block.
pub fn parse_block(src: &str) -> ParseOutcome {
    let wrapped = format!("function __pb() {{\n{src}\n}}");
    match parse_module_src(&wrapped) {
        Ok((m, file)) => {
            let body = m.items.into_iter().find_map(|it| match it.kind {
                ItemKind::Function(f) => f.body,
                _ => None,
            });
            match body {
                Some(b) => ParseOutcome::Ok(Node::Block(b)),
                None => ParseOutcome::Err(
                    "parse_block: source was not a valid block".into(),
                    Span::new(file, BytePos(0), BytePos(0)),
                ),
            }
        }
        Err((msg, sp)) => ParseOutcome::Err(format!("parse_block: {msg}"), sp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_item_yields_struct_node_with_name_and_fields() {
        let node = match parse_item("struct Point { x: i64, y: i64 }") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("parse failed: {m}"),
        };
        assert_eq!(node_kind(&node), "struct");
        assert_eq!(node_name(&node), "Point");
        assert_eq!(node_field_count(&node), 2);
        assert_eq!(node_field_name(&node, 0), "x");
        assert_eq!(node_field_name(&node, 1), "y");
        assert!(node_is_record(&node));
        assert!(!node_is_tuple(&node));
    }

    #[test]
    fn parse_item_tuple_and_unit_shapes() {
        let tup = match parse_item("struct Pair(i64, i64)") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("{m}"),
        };
        assert!(node_is_tuple(&tup));
        assert_eq!(node_field_count(&tup), 2);
        let unit = match parse_item("struct Nothing;") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("{m}"),
        };
        assert!(node_is_unit(&unit));
        assert_eq!(node_field_count(&unit), 0);
    }

    #[test]
    fn parse_expr_and_block_classify() {
        let e = match parse_expr("1 + 2") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("{m}"),
        };
        assert_eq!(node_kind(&e), "binary");
        let b = match parse_block("var x = 1; x") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("{m}"),
        };
        assert_eq!(node_kind(&b), "block");
    }

    #[test]
    fn parse_item_reports_errors() {
        assert!(matches!(parse_item("struct {{{"), ParseOutcome::Err(..)));
    }

    #[test]
    fn function_node_names_itself() {
        let f = match parse_item("function add(a: i64, b: i64): i64 { a + b }") {
            ParseOutcome::Ok(n) => n,
            ParseOutcome::Err(m, _) => panic!("{m}"),
        };
        assert_eq!(node_kind(&f), "function");
        assert_eq!(node_name(&f), "add");
        // Functions expose no record fields.
        assert_eq!(node_field_count(&f), 0);
    }

    #[test]
    fn fresh_file_ids_are_unique() {
        let a = with(|s| s.new_gen_file("a".into()));
        let b = with(|s| s.new_gen_file("b".into()));
        assert_ne!(a, b);
    }
}
