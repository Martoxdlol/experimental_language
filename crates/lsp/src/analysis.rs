//! The bridge between the `compiler` front-end and the language server.
//!
//! [`Compiled`] runs the full lex → parse → analyze pipeline over one open
//! document's text and exposes everything the server's feature handlers need:
//! diagnostics, the span-keyed type/resolution tables, and queries that map an
//! editor position to the symbol or type under the cursor.
//!
//! Editor positions are UTF-16 (the LSP default); the compiler works in UTF-8
//! byte offsets. The free conversion functions at the bottom bridge the two.

use compiler::ast::{ExternItem, ItemKind, Module, TypeKind};
use compiler::ids::DefId;
use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::sema::symbols::{Def, DefKind, Program};
use compiler::sema::{analyze, Analysis, Builtin, ValueRes};
use compiler::span::{FileId, SourceMap, Span};
use compiler::token::{Token, TokenKind};
use compiler::ty::Ty;

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
    pub analysis: Analysis,
    /// Span + message for every lexer/parser/semantic error, already filtered
    /// to the document file.
    pub diagnostics: Vec<(Span, String)>,
}

impl Compiled {
    /// Run the whole front-end over `text`.
    pub fn new(text: String) -> Compiled {
        let mut map = SourceMap::new();
        let file = map.add_file("<doc>", text.clone());

        let (tokens, lex_errors) = lex(&text, file);
        let (module, parse_errors) = parse(&text, &tokens);
        let analysis = analyze(&module);

        let mut diagnostics = Vec::new();
        for e in &lex_errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        for e in &parse_errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        for e in &analysis.errors {
            diagnostics.push((e.span, e.kind.to_string()));
        }
        // Keep only diagnostics that point into the document itself; spans in
        // the prelude or `@Derive`-synthesised virtual files have no editor
        // location.
        diagnostics.retain(|(s, _)| s.file == DOC_FILE);

        Compiled { text, map, tokens, module, analysis, diagnostics }
    }

    /// Render a type using the program's definition names.
    pub fn display_ty(&self, ty: Ty) -> String {
        let prog = &self.analysis.program;
        self.analysis.tcx.display(ty, &|id| prog.def(id).name.clone())
    }

    fn results(&self) -> &compiler::sema::CheckResults {
        &self.analysis.results
    }

    fn program(&self) -> &Program {
        &self.analysis.program
    }

    /// The smallest span in `it` that contains byte offset `off`. Ties (equal
    /// length) keep the first seen. Only spans in the document file qualify.
    fn smallest_containing<'a, I>(off: usize, it: I) -> Option<Span>
    where
        I: Iterator<Item = &'a Span>,
    {
        let mut best: Option<Span> = None;
        for &s in it {
            if s.file != DOC_FILE {
                continue;
            }
            let (lo, hi) = (s.lo.to_usize(), s.hi.to_usize());
            if lo <= off && off < hi {
                let better = best.is_none_or(|b| s.len() < b.len());
                if better {
                    best = Some(s);
                }
            }
        }
        best
    }

    /// The value resolution and its span for the name under `off`, if any.
    pub fn resolution_at(&self, off: usize) -> Option<(Span, ValueRes)> {
        let span = Self::smallest_containing(off, self.results().resolutions.keys())?;
        let res = *self.results().resolutions.get(&span)?;
        Some((span, res))
    }

    /// The type and span of the expression under `off`, if any.
    pub fn expr_ty_at(&self, off: usize) -> Option<(Span, Ty)> {
        let span = Self::smallest_containing(off, self.results().expr_types.keys())?;
        let ty = *self.results().expr_types.get(&span)?;
        Some((span, ty))
    }

    /// The defining span of what a resolution points at, when it lives in the
    /// document (prelude / builtin targets have no editor location).
    pub fn definition_span(&self, res: ValueRes) -> Option<Span> {
        let span = match res {
            ValueRes::Local(id) => *self.results().local_decls.get(&id)?,
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
        (span.file == DOC_FILE).then_some(span)
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
        if let Some(res) = self.results().resolutions.get(&span) {
            return match res {
                ValueRes::Local(id) => {
                    let params = self
                        .results()
                        .fn_params
                        .values()
                        .any(|ps| ps.contains(id));
                    if params {
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
        ItemKind::Extend(_) | ItemKind::Import(_) => return None,
    })
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
        for (&span, &ty) in &self.analysis.results.expr_types {
            if span.file != DOC_FILE || span.hi.to_usize() != dot_off {
                continue;
            }
            let better = best.is_none_or(|(b, _)| span.len() > b.len());
            if better {
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
}
