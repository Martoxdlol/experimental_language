//! Hand-written recursive-descent + Pratt parser.
//!
//! Consumes the token stream produced by [`crate::lexer::lex`] together with
//! the source text it was lexed from, and produces an `ast::Module` plus a
//! list of `ParseError`s. The parser tries to keep going past errors so a
//! downstream tool (LSP, fmt) still gets a usable tree.
//!
//! ### Notable design choices
//!
//! * **Spans on every node.** Every AST node records the half-open byte range
//!   that produced it. Spans are joined upward as we assemble compound nodes.
//! * **Speculative parsing for `(`.** When we see `(` in expression position
//!   we can't tell up front whether it's a grouping, a tuple, or the
//!   parameter list of an arrow closure. We checkpoint and try the closure
//!   form first.
//! * **Speculative parsing for `<`.** `f<T>(...)` and `Foo<T> { ... }` would
//!   otherwise be ambiguous with `f < T > (...)`. We commit to generic
//!   arguments only if we can parse `< Type ( , Type )* >` followed by `(`
//!   or `{`.
//! * **`>>` splitting.** Right-shift `>>` is one token to the lexer; in
//!   nested generics like `Foo<Bar<i64>>` we eat the first `>` of the `>>`
//!   and remember to leave one behind for the outer closer.
//! * **No backtracking past tokens with errors.** Speculative parsers don't
//!   commit errors until they commit to the parse — `restore` truncates the
//!   error list.

use crate::ast::*;
use crate::parse_diag::{ParseError, ParseErrorKind};
use crate::span::{BytePos, Span};
use crate::token::{IntBase, Keyword, Token, TokenKind};

// ===========================================================================
// Public entry point
// ===========================================================================

/// Parse a token stream into a `Module`.
///
/// `src` must be the source text the tokens were produced from. `tokens`
/// must end with `TokenKind::Eof`.
pub fn parse(src: &str, tokens: &[Token]) -> (Module, Vec<ParseError>) {
    let mut parser = Parser::new(src, tokens);
    let module = parser.parse_module();
    (module, parser.errors)
}

// ===========================================================================
// Parser state
// ===========================================================================

struct Parser<'src> {
    src: &'src str,
    tokens: &'src [Token],
    pos: usize,
    /// When `true`, the token at `pos` is `Shr` whose first `>` has been
    /// consumed virtually. The next `peek_kind` will report `Gt`; the next
    /// `eat_close_angle` will consume the second half and finally advance.
    half_eaten_gt: bool,
    errors: Vec<ParseError>,
}

#[derive(Copy, Clone)]
struct Checkpoint {
    pos: usize,
    half_eaten_gt: bool,
    errors_len: usize,
}

/// Restrictions used when descending into sub-expressions.
#[derive(Copy, Clone, Default)]
struct Restrict {
    /// `if cond { ... }` — inside `cond` we cannot start a struct literal at
    /// the top level, because the `{` belongs to the `if` body. The same
    /// rule applies to `while`, `for`, `match` headers.
    no_struct_lit: bool,
}

impl<'src> Parser<'src> {
    fn new(src: &'src str, tokens: &'src [Token]) -> Self {
        Self {
            src,
            tokens,
            pos: 0,
            half_eaten_gt: false,
            errors: Vec::new(),
        }
    }

    // ---- token-stream primitives -------------------------------------------

    fn peek_kind(&self) -> TokenKind {
        if self.half_eaten_gt {
            TokenKind::Gt
        } else {
            self.tokens[self.pos].kind
        }
    }

    fn peek_kind_at(&self, n: usize) -> TokenKind {
        if n == 0 {
            return self.peek_kind();
        }
        // Stepping past a half-eaten `>` consumes the second half virtually.
        let base = if self.half_eaten_gt {
            // The current "token" is the synthetic `>`. The next real token
            // is at self.pos + 1 (past the original `>>`), so offset-by-1
            // becomes offset-0 in the real array.
            self.pos + n
        } else {
            self.pos + n
        };
        self.tokens
            .get(base)
            .map(|t| t.kind)
            .unwrap_or(TokenKind::Eof)
    }

    fn peek_span(&self) -> Span {
        if self.half_eaten_gt {
            let outer = self.tokens[self.pos].span;
            // Second half of `>>` starts one byte in.
            Span::new(outer.file, BytePos(outer.lo.0 + 1), outer.hi)
        } else {
            self.tokens[self.pos].span
        }
    }

    fn bump(&mut self) -> Token {
        if self.half_eaten_gt {
            let outer = self.tokens[self.pos].span;
            let half = Span::new(outer.file, BytePos(outer.lo.0 + 1), outer.hi);
            self.half_eaten_gt = false;
            self.pos += 1;
            Token::new(TokenKind::Gt, half)
        } else {
            let tok = self.tokens[self.pos];
            if !matches!(tok.kind, TokenKind::Eof) {
                self.pos += 1;
            }
            tok
        }
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.peek_kind() == kind
    }

    fn at_kw(&self, kw: Keyword) -> bool {
        matches!(self.peek_kind(), TokenKind::Kw(k) if k == kw)
    }

    fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn eat_kw(&mut self, kw: Keyword) -> Option<Token> {
        if self.at_kw(kw) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &'static str) -> Option<Token> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec![what],
                    found: self.peek_kind(),
                },
                span,
            ));
            None
        }
    }

    fn error(&mut self, err: ParseError) {
        self.errors.push(err);
    }

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            pos: self.pos,
            half_eaten_gt: self.half_eaten_gt,
            errors_len: self.errors.len(),
        }
    }

    fn restore(&mut self, cp: Checkpoint) {
        self.pos = cp.pos;
        self.half_eaten_gt = cp.half_eaten_gt;
        self.errors.truncate(cp.errors_len);
    }

    fn slice(&self, span: Span) -> &str {
        &self.src[span.range()]
    }

    /// Try to eat a single `>` token, splitting `>>` if necessary. Returns
    /// `true` on success.
    fn eat_close_angle(&mut self) -> bool {
        match self.peek_kind() {
            TokenKind::Gt => {
                self.bump();
                true
            }
            TokenKind::Shr if !self.half_eaten_gt => {
                self.half_eaten_gt = true;
                true
            }
            _ => false,
        }
    }
}

// ===========================================================================
// Module / items
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_module(&mut self) -> Module {
        let start_span = self.peek_span();

        // Inner `//!` doc comments are only legal at the top of the file —
        // before any item-leading docs/attrs.
        let mut inner_docs = Vec::new();
        while matches!(self.peek_kind(), TokenKind::DocInner) {
            let tok = self.bump();
            inner_docs.push(DocComment {
                text: self.slice(tok.span).to_string(),
                span: tok.span,
                is_inner: true,
            });
        }

        let mut items = Vec::new();
        while !self.at(TokenKind::Eof) {
            if let Some(item) = self.parse_item(false) {
                items.push(item);
            } else {
                // Recovery: skip until we find something item-shaped.
                if !self.at(TokenKind::Eof) {
                    self.bump();
                }
            }
        }

        let end_span = self.peek_span();
        Module {
            inner_docs,
            items,
            span: start_span.join(end_span),
        }
    }

    /// Parse a single item. Returns `None` if recovery is needed.
    /// `inside_inline_mod` constrains `mod foo` (external form) to be
    /// disallowed there.
    fn parse_item(&mut self, inside_inline_mod: bool) -> Option<Item> {
        let head_span = self.peek_span();
        let docs = self.collect_outer_docs();
        let attrs = self.parse_attributes();

        let (vis, vis_span_first) = self.parse_visibility();
        let item_start_span = match (docs.first(), attrs.first(), &vis) {
            (Some(d), _, _) => d.span,
            (None, Some(a), _) => a.span,
            (None, None, Visibility::Public(s)) => *s,
            _ => head_span,
        };
        let _ = vis_span_first;

        let kind_start = self.peek_kind();
        let kind: ItemKind = match kind_start {
            TokenKind::Kw(Keyword::Var) => ItemKind::Var(self.parse_var_item()?),
            TokenKind::Kw(Keyword::Function) => ItemKind::Function(self.parse_function_item(false)?),
            TokenKind::Kw(Keyword::Struct) => ItemKind::Struct(self.parse_struct_item(false)?),
            TokenKind::Kw(Keyword::Interface) => ItemKind::Interface(self.parse_interface_item()?),
            TokenKind::Kw(Keyword::Type) => ItemKind::TypeAlias(self.parse_type_alias_item()?),
            TokenKind::Kw(Keyword::Mod) => {
                ItemKind::Module(self.parse_module_item(inside_inline_mod)?)
            }
            TokenKind::Kw(Keyword::Extend) => ItemKind::Extend(self.parse_extend_item()?),
            TokenKind::Kw(Keyword::Extern) => ItemKind::Extern(self.parse_extern_item()?),
            TokenKind::Kw(Keyword::Import) => ItemKind::Import(self.parse_import_item()?),
            _ => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec![
                            "`var`", "`function`", "`struct`", "`interface`",
                            "`type`", "`mod`", "`extend`", "`extern`", "`import`",
                        ],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                return None;
            }
        };

        let end_span = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(item_start_span);
        Some(Item {
            docs,
            attrs,
            visibility: vis,
            kind,
            span: item_start_span.join(end_span),
        })
    }

    fn collect_outer_docs(&mut self) -> Vec<DocComment> {
        let mut docs = Vec::new();
        while matches!(self.peek_kind(), TokenKind::DocOuter) {
            let tok = self.bump();
            docs.push(DocComment {
                text: self.slice(tok.span).to_string(),
                span: tok.span,
                is_inner: false,
            });
        }
        docs
    }

    fn parse_attributes(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        loop {
            // Doc comments can appear between attributes; absorb them too.
            while matches!(self.peek_kind(), TokenKind::DocOuter) {
                let _ = self.bump();
            }
            if !self.at(TokenKind::At) {
                break;
            }
            let at_tok = self.bump();
            let name_tok = match self.eat(TokenKind::Ident) {
                Some(t) => t,
                None => {
                    let span = self.peek_span();
                    self.error(ParseError::new(
                        ParseErrorKind::Expected {
                            expected: vec!["identifier after `@`"],
                            found: self.peek_kind(),
                        },
                        span,
                    ));
                    // Recovery: skip the `@` and continue.
                    continue;
                }
            };
            let name = self.ident_from(name_tok);
            let mut args = Vec::new();
            if self.eat(TokenKind::LParen).is_some() {
                while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
                    let arg = self.parse_attr_arg();
                    args.push(arg);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RParen, "`)`");
            }
            let end = self
                .tokens
                .get(self.pos.saturating_sub(1))
                .map(|t| t.span)
                .unwrap_or(at_tok.span);
            attrs.push(Attribute {
                name,
                args,
                span: at_tok.span.join(end),
            });
        }
        attrs
    }

    fn parse_attr_arg(&mut self) -> AttrArg {
        // `name = value` or `value`.
        if matches!(self.peek_kind(), TokenKind::Ident)
            && matches!(self.peek_kind_at(1), TokenKind::Eq)
        {
            let name_tok = self.bump();
            let name = self.ident_from(name_tok);
            self.bump(); // =
            let value = self.parse_expr(Restrict::default());
            let span = name.span.join(value.span);
            AttrArg::Named { name, value, span }
        } else {
            AttrArg::Positional(self.parse_expr(Restrict::default()))
        }
    }

    fn parse_visibility(&mut self) -> (Visibility, Option<Span>) {
        if self.at_kw(Keyword::Pub) {
            let tok = self.bump();
            (Visibility::Public(tok.span), Some(tok.span))
        } else {
            (Visibility::Private, None)
        }
    }
}

// ===========================================================================
// Specific item kinds
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_var_item(&mut self) -> Option<VarItem> {
        self.bump(); // `var`
        let name = self.expect_ident("variable name")?;
        let ty = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect(TokenKind::Eq, "`=` to initialize")?;
        let init = self.parse_expr(Restrict::default());
        self.expect(TokenKind::Semi, "`;`");
        Some(VarItem { name, ty, init })
    }

    fn parse_function_item(&mut self, allow_no_body: bool) -> Option<FunctionItem> {
        self.bump(); // `function`
        let name = self.expect_ident("function name")?;
        let generics = self.parse_optional_generic_params();
        self.expect(TokenKind::LParen, "`(` to start parameter list")?;
        let params = self.parse_param_list();
        self.expect(TokenKind::RParen, "`)`");
        let return_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        let is_async = self.eat_kw(Keyword::Async).is_some();
        let body = if self.at(TokenKind::LBrace) {
            Some(self.parse_block())
        } else if allow_no_body {
            // No body — must be terminated by `;`.
            self.expect(TokenKind::Semi, "`;` after extern function declaration");
            None
        } else {
            // Expected a body — emit an error but don't bail completely.
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec!["`{` to start function body"],
                    found: self.peek_kind(),
                },
                span,
            ));
            None
        };
        Some(FunctionItem {
            name,
            generics,
            params,
            return_type,
            is_async,
            body,
        })
    }

    fn parse_param_list(&mut self) -> Vec<Param> {
        let mut params = Vec::new();
        while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
            let p = self.parse_param();
            params.push(p);
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        params
    }

    fn parse_param(&mut self) -> Param {
        let start = self.peek_span();
        if self.at_kw(Keyword::SelfLower) {
            let tok = self.bump();
            return Param {
                kind: ParamKind::SelfParam,
                span: tok.span,
            };
        }
        let name_tok = self.bump();
        let name = match name_tok.kind {
            TokenKind::Ident => self.ident_from(name_tok),
            _ => {
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["parameter name"],
                        found: name_tok.kind,
                    },
                    name_tok.span,
                ));
                self.ident_from(name_tok)
            }
        };
        self.expect(TokenKind::Colon, "`:` after parameter name");
        let ty = self.parse_type();
        Param {
            kind: ParamKind::Normal { name: name.clone(), ty: ty.clone() },
            span: start.join(ty.span),
        }
    }

    fn parse_struct_item(&mut self, is_extern: bool) -> Option<StructItem> {
        self.bump(); // `struct`
        let name = self.expect_ident("struct name")?;
        let generics = self.parse_optional_generic_params();
        let kind = match self.peek_kind() {
            TokenKind::Semi => {
                self.bump();
                StructKind::Unit
            }
            TokenKind::LParen => {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
                    let start = self.peek_span();
                    let (vis, _) = self.parse_visibility();
                    let ty = self.parse_type();
                    let span = start.join(ty.span);
                    fields.push(TupleField { visibility: vis, ty, span });
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RParen, "`)`");
                // Tuple structs end with `;` per spec; consume if present
                // (some examples omit it for `pub struct Pair(...)`).
                let _ = self.eat(TokenKind::Semi);
                StructKind::Tuple(fields)
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                    let f = self.parse_struct_field();
                    fields.push(f);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace, "`}`");
                StructKind::Record(fields)
            }
            _ => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["`;`, `(`, or `{`"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                StructKind::Unit
            }
        };
        Some(StructItem { name, generics, is_extern, kind })
    }

    fn parse_struct_field(&mut self) -> StructField {
        let start = self.peek_span();
        let docs = self.collect_outer_docs();
        let attrs = self.parse_attributes();
        let (vis, _) = self.parse_visibility();
        let name = match self.expect_ident("field name") {
            Some(n) => n,
            None => Ident::new("<error>", self.peek_span()),
        };
        self.expect(TokenKind::Colon, "`:`");
        let ty = self.parse_type();
        StructField {
            docs,
            attrs,
            visibility: vis,
            name,
            ty: ty.clone(),
            span: start.join(ty.span),
        }
    }

    fn parse_interface_item(&mut self) -> Option<InterfaceItem> {
        self.bump(); // `interface`
        let name = self.expect_ident("interface name")?;
        let generics = self.parse_optional_generic_params();
        let mut supers = Vec::new();
        if self.eat(TokenKind::Colon).is_some() {
            loop {
                supers.push(self.parse_type_no_union());
                if self.eat(TokenKind::Plus).is_none() {
                    break;
                }
            }
        }
        self.expect(TokenKind::LBrace, "`{`")?;
        let mut members = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            if let Some(m) = self.parse_interface_member() {
                members.push(m);
            } else {
                // Recover by skipping a token.
                if !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                    self.bump();
                }
            }
        }
        self.expect(TokenKind::RBrace, "`}`");
        Some(InterfaceItem { name, generics, supers, members })
    }

    fn parse_interface_member(&mut self) -> Option<InterfaceMember> {
        let start = self.peek_span();
        let docs = self.collect_outer_docs();
        let attrs = self.parse_attributes();
        if !self.at_kw(Keyword::Function) {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec!["`function` in interface body"],
                    found: self.peek_kind(),
                },
                span,
            ));
            return None;
        }
        self.bump(); // `function`
        let name = self.expect_ident("method name")?;
        let generics = self.parse_optional_generic_params();
        self.expect(TokenKind::LParen, "`(`");
        let params = self.parse_param_list();
        self.expect(TokenKind::RParen, "`)`");
        let return_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        let is_async = self.eat_kw(Keyword::Async).is_some();
        let default_body = if self.at(TokenKind::LBrace) {
            Some(self.parse_block())
        } else {
            // Method declaration without a body — must be terminated by `;`.
            self.expect(TokenKind::Semi, "`;` after method declaration");
            None
        };
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        Some(InterfaceMember {
            docs,
            attrs,
            function: FunctionSig {
                name,
                generics,
                params,
                return_type,
                is_async,
            },
            default_body,
            span: start.join(end),
        })
    }

    fn parse_type_alias_item(&mut self) -> Option<TypeAliasItem> {
        self.bump(); // `type`
        let name = self.expect_ident("type name")?;
        let generics = self.parse_optional_generic_params();
        self.expect(TokenKind::Eq, "`=`")?;
        let aliased = self.parse_type();
        self.expect(TokenKind::Semi, "`;`");
        Some(TypeAliasItem { name, generics, aliased })
    }

    fn parse_module_item(&mut self, inside_inline_mod: bool) -> Option<ModuleItem> {
        self.bump(); // `mod`
        let name = self.expect_ident("module name")?;
        if self.at(TokenKind::LBrace) {
            self.bump();
            // Inner `//!` allowed only at the very top of the inline body.
            let mut inner_docs = Vec::new();
            while matches!(self.peek_kind(), TokenKind::DocInner) {
                let tok = self.bump();
                inner_docs.push(DocComment {
                    text: self.slice(tok.span).to_string(),
                    span: tok.span,
                    is_inner: true,
                });
            }
            let mut items = Vec::new();
            while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                if let Some(it) = self.parse_item(true) {
                    items.push(it);
                } else if !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                    self.bump();
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
            Some(ModuleItem {
                name,
                kind: ModuleKind::Inline { inner_docs, items },
            })
        } else {
            if inside_inline_mod {
                self.error(ParseError::new(
                    ParseErrorKind::NestedExternalMod,
                    name.span,
                ));
            }
            self.expect(TokenKind::Semi, "`;`");
            Some(ModuleItem { name, kind: ModuleKind::External })
        }
    }

    fn parse_extend_item(&mut self) -> Option<ExtendItem> {
        self.bump(); // `extend`
        let generics = self.parse_optional_generic_params();
        let target = self.parse_type();
        let mut interfaces = Vec::new();
        if self.eat(TokenKind::Colon).is_some() {
            loop {
                interfaces.push(self.parse_type_no_union());
                if self.eat(TokenKind::Plus).is_none() {
                    break;
                }
            }
        }
        self.expect(TokenKind::LBrace, "`{`")?;
        let mut members = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            if let Some(m) = self.parse_extend_member() {
                members.push(m);
            } else if !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                self.bump();
            }
        }
        self.expect(TokenKind::RBrace, "`}`");
        Some(ExtendItem { generics, target, interfaces, members })
    }

    fn parse_extend_member(&mut self) -> Option<ExtendMember> {
        let start = self.peek_span();
        let docs = self.collect_outer_docs();
        let attrs = self.parse_attributes();
        let (vis, _) = self.parse_visibility();
        if !self.at_kw(Keyword::Function) {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec!["`function` in extend body"],
                    found: self.peek_kind(),
                },
                span,
            ));
            return None;
        }
        let function = self.parse_function_item(true)?;
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        Some(ExtendMember {
            docs,
            attrs,
            visibility: vis,
            function,
            span: start.join(end),
        })
    }

    fn parse_extern_item(&mut self) -> Option<ExternItem> {
        self.bump(); // `extern`
        match self.peek_kind() {
            TokenKind::Kw(Keyword::Function) => {
                let f = self.parse_function_item(true)?;
                Some(ExternItem::Function(f))
            }
            TokenKind::Kw(Keyword::Struct) => {
                let s = self.parse_struct_item(true)?;
                Some(ExternItem::Struct(s))
            }
            TokenKind::Kw(Keyword::Type) => {
                self.bump();
                let name = self.expect_ident("type name")?;
                self.expect(TokenKind::Semi, "`;`");
                Some(ExternItem::OpaqueType(name))
            }
            TokenKind::Kw(Keyword::Var) => {
                self.bump();
                let name = self.expect_ident("variable name")?;
                self.expect(TokenKind::Colon, "`:`");
                let ty = self.parse_type();
                self.expect(TokenKind::Semi, "`;`");
                Some(ExternItem::Var { name, ty })
            }
            _ => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["`function`, `struct`, `type`, or `var` after `extern`"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                None
            }
        }
    }

    fn parse_import_item(&mut self) -> Option<ImportItem> {
        self.bump(); // `import`
        if self.eat(TokenKind::LBrace).is_some() {
            // import { a, b as c } from "path"
            let mut names = Vec::new();
            while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                let start = self.peek_span();
                let name = self.expect_ident("imported name")?;
                let alias = if self.eat_kw(Keyword::As).is_some() {
                    Some(self.expect_ident("alias name")?)
                } else {
                    None
                };
                let end = alias.as_ref().map(|a| a.span).unwrap_or(name.span);
                names.push(ImportName {
                    name,
                    alias,
                    span: start.join(end),
                });
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RBrace, "`}`");
            // `from "path"` — `from` is an identifier (not a keyword).
            if matches!(self.peek_kind(), TokenKind::Ident)
                && self.slice(self.peek_span()) == "from"
            {
                self.bump();
            } else {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["`from`"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
            }
            let path = self.parse_string_literal();
            self.expect(TokenKind::Semi, "`;`");
            Some(ImportItem {
                kind: ImportKind::Named(names),
                path,
            })
        } else {
            // import "path" [as Name];
            let path = self.parse_string_literal();
            let kind = if self.eat_kw(Keyword::As).is_some() {
                let name = self.expect_ident("namespace name")?;
                ImportKind::Namespace(name)
            } else {
                ImportKind::Ambient
            };
            self.expect(TokenKind::Semi, "`;`");
            Some(ImportItem { kind, path })
        }
    }
}

// ===========================================================================
// Generic parameters
// ===========================================================================

impl<'src> Parser<'src> {
    /// `<T, U: Bound1 + Bound2, V = Default>` — optional.
    fn parse_optional_generic_params(&mut self) -> Option<GenericParams> {
        if !self.at(TokenKind::Lt) {
            return None;
        }
        let lt = self.bump();
        let mut params = Vec::new();
        loop {
            if self.eat_close_angle() {
                break;
            }
            let p = self.parse_generic_param();
            params.push(p);
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            if !self.eat_close_angle() {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["`,` or `>`"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
            }
            break;
        }
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(lt.span);
        Some(GenericParams {
            params,
            span: lt.span.join(end),
        })
    }

    fn parse_generic_param(&mut self) -> GenericParam {
        let start = self.peek_span();
        let name = self
            .expect_ident("type parameter name")
            .unwrap_or_else(|| Ident::new("<error>", start));
        let mut bounds = Vec::new();
        if self.eat(TokenKind::Colon).is_some() {
            loop {
                // Bounds are interface names; unions don't make sense here,
                // so we use `parse_type_no_union` to keep `|` for siblings.
                bounds.push(self.parse_type_no_union());
                if self.eat(TokenKind::Plus).is_none() {
                    break;
                }
            }
        }
        let default = if self.eat(TokenKind::Eq).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        GenericParam {
            name,
            bounds,
            default,
            span: start.join(end),
        }
    }
}

// ===========================================================================
// Types
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_type(&mut self) -> Type {
        let first = self.parse_type_no_union();
        if !self.at(TokenKind::Pipe) {
            return first;
        }
        let mut alts = vec![first];
        while self.eat(TokenKind::Pipe).is_some() {
            alts.push(self.parse_type_no_union());
        }
        let span = alts.first().unwrap().span.join(alts.last().unwrap().span);
        Type { kind: TypeKind::Union(alts), span }
    }

    fn parse_type_no_union(&mut self) -> Type {
        let start = self.peek_span();
        match self.peek_kind() {
            TokenKind::Star => {
                self.bump();
                let inner = self.parse_type_no_union();
                let span = start.join(inner.span);
                Type {
                    kind: TypeKind::Pointer(Box::new(inner)),
                    span,
                }
            }
            TokenKind::LParen => self.parse_tuple_or_function_type(),
            TokenKind::LBracket => {
                // `[T; N]`
                self.bump();
                let elem = self.parse_type();
                self.expect(TokenKind::Semi, "`;` in array type");
                let len = self.parse_expr(Restrict::default());
                let close = self.expect(TokenKind::RBracket, "`]`");
                let end = close.map(|t| t.span).unwrap_or(len.span);
                Type {
                    kind: TypeKind::Array { elem: Box::new(elem), len: Box::new(len) },
                    span: start.join(end),
                }
            }
            TokenKind::Kw(Keyword::SelfUpper) => {
                let tok = self.bump();
                Type { kind: TypeKind::SelfType, span: tok.span }
            }
            TokenKind::Kw(Keyword::Extern) => {
                self.bump();
                self.expect(TokenKind::LParen, "`(`");
                let mut params = Vec::new();
                while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
                    let p = self.parse_extern_param_type();
                    params.push(p);
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                self.expect(TokenKind::RParen, "`)`");
                self.expect(TokenKind::FatArrow, "`=>`");
                let ret = self.parse_type_no_union();
                let span = start.join(ret.span);
                Type {
                    kind: TypeKind::ExternFunction {
                        params,
                        ret: Box::new(ret),
                    },
                    span,
                }
            }
            TokenKind::Ident => self.parse_named_type(),
            TokenKind::Kw(Keyword::Null) => {
                // `null` is the sole value of the `null` type AND the type
                // name itself.
                let tok = self.bump();
                Type {
                    kind: TypeKind::Named {
                        name: Ident::new("null", tok.span),
                        generics: Vec::new(),
                    },
                    span: tok.span,
                }
            }
            // Primitive keywords aren't actual keywords in our lexer — they
            // come through as `Ident`. Anything else here is an error.
            _ => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["type"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                Type {
                    kind: TypeKind::Named {
                        name: Ident::new("<error>", span),
                        generics: Vec::new(),
                    },
                    span,
                }
            }
        }
    }

    fn parse_extern_param_type(&mut self) -> ExternParamType {
        let start = self.peek_span();
        // `name: Type` or just `Type`.
        let name = if matches!(self.peek_kind(), TokenKind::Ident)
            && matches!(self.peek_kind_at(1), TokenKind::Colon)
        {
            let tok = self.bump();
            self.bump(); // `:`
            Some(self.ident_from(tok))
        } else {
            None
        };
        let ty = self.parse_type_no_union();
        let span = start.join(ty.span);
        ExternParamType { name, ty, span }
    }

    fn parse_tuple_or_function_type(&mut self) -> Type {
        let lparen = self.bump();
        // Empty parens — only valid in `() => R` function types.
        if self.at(TokenKind::RParen) {
            let rparen = self.bump();
            if self.eat(TokenKind::FatArrow).is_some() {
                let ret = self.parse_type_no_union();
                let span = lparen.span.join(ret.span);
                return Type {
                    kind: TypeKind::Function {
                        params: Vec::new(),
                        ret: Box::new(ret),
                    },
                    span,
                };
            }
            self.error(ParseError::new(
                ParseErrorKind::Message(
                    "`()` is not a type; use a 1-tuple is impossible — write `null` or function type".into(),
                ),
                lparen.span.join(rparen.span),
            ));
            return Type {
                kind: TypeKind::Tuple(Vec::new()),
                span: lparen.span.join(rparen.span),
            };
        }
        let first = self.parse_type();
        let mut more = Vec::new();
        let mut had_comma = false;
        while self.eat(TokenKind::Comma).is_some() {
            had_comma = true;
            if self.at(TokenKind::RParen) {
                break;
            }
            more.push(self.parse_type());
        }
        let rparen = self.expect(TokenKind::RParen, "`)`");
        let close_span = rparen.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        // Function type?
        if self.eat(TokenKind::FatArrow).is_some() {
            let ret = self.parse_type_no_union();
            let mut params = vec![first];
            params.extend(more);
            let span = lparen.span.join(ret.span);
            return Type {
                kind: TypeKind::Function {
                    params,
                    ret: Box::new(ret),
                },
                span,
            };
        }
        if had_comma {
            let mut params = vec![first];
            params.extend(more);
            Type {
                kind: TypeKind::Tuple(params),
                span: lparen.span.join(close_span),
            }
        } else {
            Type {
                kind: TypeKind::Paren(Box::new(first)),
                span: lparen.span.join(close_span),
            }
        }
    }

    fn parse_named_type(&mut self) -> Type {
        let tok = self.bump();
        let name = self.ident_from(tok);
        let start = name.span;
        let mut generics = Vec::new();
        if self.at(TokenKind::Lt) {
            // Always commit in type position.
            self.bump();
            loop {
                if self.eat_close_angle() {
                    break;
                }
                generics.push(self.parse_type());
                if self.eat(TokenKind::Comma).is_some() {
                    continue;
                }
                if !self.eat_close_angle() {
                    let span = self.peek_span();
                    self.error(ParseError::new(
                        ParseErrorKind::Expected {
                            expected: vec!["`,` or `>`"],
                            found: self.peek_kind(),
                        },
                        span,
                    ));
                }
                break;
            }
        }
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        Type {
            kind: TypeKind::Named { name, generics },
            span: start.join(end),
        }
    }
}

// ===========================================================================
// Blocks and statements
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_block(&mut self) -> Block {
        let lbrace = self.expect(TokenKind::LBrace, "`{`");
        let start = lbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        let mut stmts = Vec::new();
        let mut trailing: Option<Box<Expr>> = None;
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            // Item declarations at block level.
            if self.peek_is_item_start() {
                if let Some(it) = self.parse_item(false) {
                    let span = it.span;
                    stmts.push(Stmt {
                        kind: StmtKind::Item(Box::new(it)),
                        span,
                    });
                }
                continue;
            }
            // `var` binding.
            if self.at_kw(Keyword::Var) {
                let var_start = self.peek_span();
                self.bump();
                let pattern = self.parse_pattern();
                let ty = if self.eat(TokenKind::Colon).is_some() {
                    Some(self.parse_type())
                } else {
                    None
                };
                self.expect(TokenKind::Eq, "`=` to initialize");
                let init = self.parse_expr(Restrict::default());
                let semi = self.expect(TokenKind::Semi, "`;`");
                let end = semi.map(|t| t.span).unwrap_or(init.span);
                stmts.push(Stmt {
                    kind: StmtKind::Var(LocalVar { pattern, ty, init }),
                    span: var_start.join(end),
                });
                continue;
            }

            // Expression-rooted statement (or trailing expression).
            let expr = self.parse_expr(Restrict::default());
            let is_block_form = is_block_form_expr(&expr);
            // Assignment statement?
            if self.at(TokenKind::Eq) && !is_block_form {
                let eq_tok = self.bump();
                let value = self.parse_expr(Restrict::default());
                let semi = self.expect(TokenKind::Semi, "`;`");
                let end = semi.map(|t| t.span).unwrap_or(value.span);
                stmts.push(Stmt {
                    kind: StmtKind::Assign { target: expr, value },
                    span: eq_tok.span.join(end),
                });
                continue;
            }
            // Expression statement or trailing.
            if self.eat(TokenKind::Semi).is_some() {
                let span = expr.span;
                stmts.push(Stmt {
                    kind: StmtKind::Expr(expr),
                    span,
                });
            } else if self.at(TokenKind::RBrace) {
                trailing = Some(Box::new(expr));
                break;
            } else if is_block_form {
                // Block-form expressions don't need a semicolon as statements.
                let span = expr.span;
                stmts.push(Stmt {
                    kind: StmtKind::Expr(expr),
                    span,
                });
            } else {
                // We didn't see `;` and we're not at `}` — error and stop.
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["`;` or `}`"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                let stmt_span = expr.span;
                stmts.push(Stmt {
                    kind: StmtKind::Expr(expr),
                    span: stmt_span,
                });
                // Try to advance to avoid infinite loops.
                if !self.at(TokenKind::Eof) {
                    self.bump();
                }
            }
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`");
        let end = rbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Block {
            stmts,
            trailing,
            span: start.join(end),
        }
    }

    fn peek_is_item_start(&self) -> bool {
        // `pub` or doc/attribute can prefix; we recognise the leading keyword.
        let mut n = 0usize;
        loop {
            let k = self.peek_kind_at(n);
            match k {
                TokenKind::DocOuter | TokenKind::At => {
                    // Skip docs; for attributes, we'd need to walk past the
                    // arg list — for simplicity we just check non-arg attrs.
                    n += 1;
                }
                TokenKind::Kw(Keyword::Pub) => n += 1,
                TokenKind::Kw(Keyword::Function)
                | TokenKind::Kw(Keyword::Struct)
                | TokenKind::Kw(Keyword::Interface)
                | TokenKind::Kw(Keyword::Type)
                | TokenKind::Kw(Keyword::Mod)
                | TokenKind::Kw(Keyword::Extend)
                | TokenKind::Kw(Keyword::Extern)
                | TokenKind::Kw(Keyword::Import) => return true,
                _ => return false,
            }
        }
    }
}

fn is_block_form_expr(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Block(_)
            | ExprKind::Loop(_)
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::AsyncBlock(_)
    )
}

// ===========================================================================
// Expressions (Pratt-style precedence climbing)
// ===========================================================================

/// Binary-operator precedence. Higher = binds tighter.
///
/// `as` / `is` are at the cast tier (between unary and `*`/`/`/`%`) but are
/// handled positionally by `parse_cast`, so they don't appear in this table.
const PREC_OR: u8 = 10;
const PREC_AND: u8 = 20;
const PREC_CMP: u8 = 30;
const PREC_BITOR: u8 = 40;
const PREC_BITXOR: u8 = 50;
const PREC_BITAND: u8 = 60;
const PREC_SHIFT: u8 = 70;
const PREC_ADD: u8 = 80;
const PREC_MUL: u8 = 90;

impl<'src> Parser<'src> {
    fn parse_expr(&mut self, restrict: Restrict) -> Expr {
        self.parse_expr_bp(0, restrict)
    }

    fn parse_expr_bp(&mut self, min_bp: u8, restrict: Restrict) -> Expr {
        let mut left = self.parse_unary(restrict);
        loop {
            let op_info = match self.peek_binop() {
                Some(info) => info,
                None => break,
            };
            let (op, op_span, prec, is_non_assoc) = op_info;
            if prec < min_bp {
                break;
            }
            // For non-associative comparison ops, do not allow chaining: we
            // recurse with `prec + 1` so any further comparison at the same
            // precedence is rejected when we try to fold it.
            self.advance_binop(op);
            let right = if is_non_assoc {
                self.parse_expr_bp(prec + 1, restrict)
            } else {
                self.parse_expr_bp(prec + 1, restrict)
            };
            // If we then see another comparison op at the same precedence,
            // the outer loop will iterate and re-encounter the operator — we
            // need to check that case and surface a chain error.
            if is_non_assoc {
                if let Some((next_op, _, next_prec, true)) = self.peek_binop() {
                    if next_prec == prec {
                        let chained = bin_op_str(next_op);
                        let span = self.peek_span();
                        self.error(ParseError::new(
                            ParseErrorKind::NonAssociativeChain { op: chained },
                            span,
                        ));
                    }
                }
            }
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    op_span,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        left
    }

    fn parse_unary(&mut self, restrict: Restrict) -> Expr {
        let start = self.peek_span();
        match self.peek_kind() {
            TokenKind::Minus => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Neg,
                        op_span: start,
                        operand: Box::new(operand),
                    },
                    span,
                }
            }
            TokenKind::Bang => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Not,
                        op_span: start,
                        operand: Box::new(operand),
                    },
                    span,
                }
            }
            TokenKind::Tilde => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::BitNot,
                        op_span: start,
                        operand: Box::new(operand),
                    },
                    span,
                }
            }
            TokenKind::Amp => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Ref { expr: Box::new(operand), amp_span: start },
                    span,
                }
            }
            TokenKind::Star => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Deref { expr: Box::new(operand), star_span: start },
                    span,
                }
            }
            TokenKind::Kw(Keyword::Await) => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Await { expr: Box::new(operand), kw_span: start },
                    span,
                }
            }
            TokenKind::Kw(Keyword::Spawn) => {
                self.bump();
                let operand = self.parse_unary(restrict);
                let span = start.join(operand.span);
                Expr {
                    kind: ExprKind::Spawn { expr: Box::new(operand), kw_span: start },
                    span,
                }
            }
            _ => self.parse_cast(restrict),
        }
    }

    /// Casts (`as` / `is`) are part of the postfix-and-cast chain. We bind
    /// them tighter than the arithmetic operators per the docs.
    fn parse_cast(&mut self, restrict: Restrict) -> Expr {
        let mut left = self.parse_postfix(restrict);
        loop {
            let op = if self.at_kw(Keyword::As) {
                CastOp::As
            } else if self.at_kw(Keyword::Is) {
                CastOp::Is
            } else {
                break;
            };
            let op_tok = self.bump();
            let ty = self.parse_type_no_union();
            let span = left.span.join(ty.span);
            left = Expr {
                kind: ExprKind::Cast {
                    op,
                    op_span: op_tok.span,
                    expr: Box::new(left),
                    ty: Box::new(ty),
                },
                span,
            };
        }
        left
    }

    fn peek_binop(&self) -> Option<(BinaryOp, Span, u8, bool)> {
        let span = self.peek_span();
        let (op, prec, is_non_assoc) = match self.peek_kind() {
            TokenKind::PipePipe => (BinaryOp::Or, PREC_OR, false),
            TokenKind::AmpAmp => (BinaryOp::And, PREC_AND, false),
            TokenKind::EqEq => (BinaryOp::Eq, PREC_CMP, true),
            TokenKind::BangEq => (BinaryOp::Ne, PREC_CMP, true),
            TokenKind::Lt => (BinaryOp::Lt, PREC_CMP, true),
            TokenKind::LtEq => (BinaryOp::Le, PREC_CMP, true),
            TokenKind::Gt => (BinaryOp::Gt, PREC_CMP, true),
            TokenKind::GtEq => (BinaryOp::Ge, PREC_CMP, true),
            TokenKind::Pipe => (BinaryOp::BitOr, PREC_BITOR, false),
            TokenKind::Caret => (BinaryOp::BitXor, PREC_BITXOR, false),
            TokenKind::Amp => (BinaryOp::BitAnd, PREC_BITAND, false),
            TokenKind::Shl => (BinaryOp::Shl, PREC_SHIFT, false),
            TokenKind::Shr => (BinaryOp::Shr, PREC_SHIFT, false),
            TokenKind::Plus => (BinaryOp::Add, PREC_ADD, false),
            TokenKind::Minus => (BinaryOp::Sub, PREC_ADD, false),
            TokenKind::Star => (BinaryOp::Mul, PREC_MUL, false),
            TokenKind::Slash => (BinaryOp::Div, PREC_MUL, false),
            TokenKind::Percent => (BinaryOp::Rem, PREC_MUL, false),
            // `as` / `is` are pseudo-binary operators with their own precedence,
            // but handled in `parse_cast` so they reorder with postfix correctly.
            _ => return None,
        };
        Some((op, span, prec, is_non_assoc))
    }

    fn advance_binop(&mut self, _op: BinaryOp) {
        self.bump();
    }
}

fn bin_op_str(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
    }
}

// ===========================================================================
// Postfix and primary
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_postfix(&mut self, restrict: Restrict) -> Expr {
        let mut expr = self.parse_primary(restrict);
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.bump();
                    match self.peek_kind() {
                        TokenKind::Ident => {
                            let tok = self.bump();
                            let name = self.ident_from(tok);
                            let span = expr.span.join(name.span);
                            expr = Expr {
                                kind: ExprKind::Field { receiver: Box::new(expr), name },
                                span,
                            };
                        }
                        // `Thread.spawn(...)`: allow `spawn` after `.` as a
                        // field name even though it is a reserved keyword.
                        TokenKind::Kw(Keyword::Spawn) => {
                            let tok = self.bump();
                            let name = Ident::new("spawn", tok.span);
                            let span = expr.span.join(name.span);
                            expr = Expr {
                                kind: ExprKind::Field { receiver: Box::new(expr), name },
                                span,
                            };
                        }
                        TokenKind::Int { base: IntBase::Dec, has_suffix: false } => {
                            let tok = self.bump();
                            let text = self.slice(tok.span);
                            let index = text
                                .parse::<u32>()
                                .ok();
                            if index.is_none() || text.starts_with('_') {
                                self.error(ParseError::new(
                                    ParseErrorKind::InvalidTupleIndex,
                                    tok.span,
                                ));
                            }
                            let span = expr.span.join(tok.span);
                            expr = Expr {
                                kind: ExprKind::TupleIndex {
                                    receiver: Box::new(expr),
                                    index: index.unwrap_or(0),
                                    index_span: tok.span,
                                },
                                span,
                            };
                        }
                        _ => {
                            let span = self.peek_span();
                            self.error(ParseError::new(
                                ParseErrorKind::Expected {
                                    expected: vec!["field name or tuple index after `.`"],
                                    found: self.peek_kind(),
                                },
                                span,
                            ));
                            break;
                        }
                    }
                }
                TokenKind::LParen => {
                    self.bump();
                    let (args, trailing) = self.parse_call_args_and_optional_trailing(restrict);
                    let close_span = trailing
                        .as_ref()
                        .map(|tc| tc.span)
                        .or_else(|| args.last().map(|a| a.span))
                        .unwrap_or(expr.span);
                    let span = expr.span.join(close_span);
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            generics: Vec::new(),
                            args,
                            trailing_closure: trailing,
                        },
                        span,
                    };
                }
                TokenKind::LBracket => {
                    self.bump();
                    let idx = self.parse_expr(Restrict::default());
                    let close = self.expect(TokenKind::RBracket, "`]`");
                    let end = close.map(|t| t.span).unwrap_or(idx.span);
                    let span = expr.span.join(end);
                    expr = Expr {
                        kind: ExprKind::Index {
                            receiver: Box::new(expr),
                            index: Box::new(idx),
                        },
                        span,
                    };
                }
                TokenKind::Question => {
                    let tok = self.bump();
                    let span = expr.span.join(tok.span);
                    expr = Expr {
                        kind: ExprKind::Try { expr: Box::new(expr), q_span: tok.span },
                        span,
                    };
                }
                TokenKind::Lt => {
                    // Speculative: turn `<` into a generic-arg list ONLY if it
                    // is followed by a parenthesised call. We deliberately do
                    // NOT commit for `Ident<T> { ... }` here — struct
                    // literals are handled directly in `parse_primary`.
                    let cp = self.checkpoint();
                    self.bump();
                    let mut tys = Vec::new();
                    let parsed = (|| {
                        loop {
                            if self.eat_close_angle() {
                                return true;
                            }
                            let ty = self.parse_type();
                            tys.push(ty);
                            if self.eat(TokenKind::Comma).is_some() {
                                continue;
                            }
                            return self.eat_close_angle();
                        }
                    })();
                    if !parsed || tys.is_empty() || !self.at(TokenKind::LParen) {
                        self.restore(cp);
                        break;
                    }
                    // Commit: parse `(args)` as a generic call.
                    self.bump();
                    let (args, trailing) = self.parse_call_args_and_optional_trailing(restrict);
                    let close_span = trailing
                        .as_ref()
                        .map(|tc| tc.span)
                        .or_else(|| args.last().map(|a| a.span))
                        .or_else(|| tys.last().map(|t| t.span))
                        .unwrap_or(expr.span);
                    let span = expr.span.join(close_span);
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            generics: tys,
                            args,
                            trailing_closure: trailing,
                        },
                        span,
                    };
                }
                TokenKind::LBrace if !restrict.no_struct_lit && self.is_trailing_closure_head(&expr) => {
                    // Trailing closure on a previously-built call/method-ref.
                    let closure = self.parse_trailing_closure();
                    let span = expr.span.join(closure.span);
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            generics: Vec::new(),
                            args: Vec::new(),
                            trailing_closure: Some(Box::new(closure)),
                        },
                        span,
                    };
                }
                _ => break,
            }
        }
        expr
    }

    /// Heuristic: a `{` here represents a trailing closure only if the
    /// receiver is a method/call/index chain — i.e. not a bare ident (which
    /// would be a struct literal handled in `parse_primary`).
    fn is_trailing_closure_head(&self, e: &Expr) -> bool {
        matches!(
            &e.kind,
            ExprKind::Field { .. }
                | ExprKind::Call { .. }
                | ExprKind::Index { .. }
        )
    }

    fn parse_call_args_and_optional_trailing(
        &mut self,
        restrict: Restrict,
    ) -> (Vec<Expr>, Option<Box<Expr>>) {
        let mut args = Vec::new();
        while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
            args.push(self.parse_expr(Restrict::default()));
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RParen, "`)`");
        let trailing = if !restrict.no_struct_lit && self.at(TokenKind::LBrace) {
            Some(Box::new(self.parse_trailing_closure()))
        } else {
            None
        };
        (args, trailing)
    }

    fn parse_trailing_closure(&mut self) -> Expr {
        let lbrace = self.bump();
        // Attempt `params =>` header. If success, body is rest of block.
        let cp = self.checkpoint();
        let mut params = Vec::new();
        let mut had_params = false;
        let header_ok = (|| {
            loop {
                if !matches!(self.peek_kind(), TokenKind::Ident) {
                    return false;
                }
                let name_tok = self.bump();
                let name = self.ident_from(name_tok);
                let ty = if self.eat(TokenKind::Colon).is_some() {
                    Some(self.parse_type())
                } else {
                    None
                };
                let span = name.span.join(ty.as_ref().map(|t| t.span).unwrap_or(name.span));
                params.push(ClosureParam { name, ty, span });
                if self.eat(TokenKind::Comma).is_some() {
                    continue;
                }
                return self.eat(TokenKind::FatArrow).is_some();
            }
        })();
        if !header_ok {
            self.restore(cp);
            params.clear();
        } else {
            had_params = true;
        }
        let _ = had_params;
        // Body: everything until matching `}`. Re-use block parsing for
        // statement-level structure.
        let body_start = self.peek_span();
        let mut stmts = Vec::new();
        let mut trailing: Option<Box<Expr>> = None;
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            // Parse inner statements just like a block.
            self.parse_block_stmt_into(&mut stmts, &mut trailing);
            if trailing.is_some() {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`");
        let end = rbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        let body_block = Block {
            stmts,
            trailing,
            span: body_start.join(end),
        };
        let body_span = body_block.span;
        let body_expr = Expr {
            kind: ExprKind::Block(body_block),
            span: body_span,
        };
        Expr {
            kind: ExprKind::Closure {
                params,
                return_type: None,
                is_async: false,
                body: Box::new(body_expr),
            },
            span: lbrace.span.join(end),
        }
    }

    fn parse_block_stmt_into(
        &mut self,
        stmts: &mut Vec<Stmt>,
        trailing: &mut Option<Box<Expr>>,
    ) {
        if self.peek_is_item_start() {
            if let Some(it) = self.parse_item(false) {
                let span = it.span;
                stmts.push(Stmt {
                    kind: StmtKind::Item(Box::new(it)),
                    span,
                });
            }
            return;
        }
        if self.at_kw(Keyword::Var) {
            let var_start = self.peek_span();
            self.bump();
            let pattern = self.parse_pattern();
            let ty = if self.eat(TokenKind::Colon).is_some() {
                Some(self.parse_type())
            } else {
                None
            };
            self.expect(TokenKind::Eq, "`=` to initialize");
            let init = self.parse_expr(Restrict::default());
            let semi = self.expect(TokenKind::Semi, "`;`");
            let end = semi.map(|t| t.span).unwrap_or(init.span);
            stmts.push(Stmt {
                kind: StmtKind::Var(LocalVar { pattern, ty, init }),
                span: var_start.join(end),
            });
            return;
        }
        let expr = self.parse_expr(Restrict::default());
        let is_block_form = is_block_form_expr(&expr);
        if self.at(TokenKind::Eq) && !is_block_form {
            let eq_tok = self.bump();
            let value = self.parse_expr(Restrict::default());
            let semi = self.expect(TokenKind::Semi, "`;`");
            let end = semi.map(|t| t.span).unwrap_or(value.span);
            stmts.push(Stmt {
                kind: StmtKind::Assign { target: expr, value },
                span: eq_tok.span.join(end),
            });
            return;
        }
        if self.eat(TokenKind::Semi).is_some() {
            let span = expr.span;
            stmts.push(Stmt { kind: StmtKind::Expr(expr), span });
        } else if self.at(TokenKind::RBrace) {
            *trailing = Some(Box::new(expr));
        } else if is_block_form {
            let span = expr.span;
            stmts.push(Stmt { kind: StmtKind::Expr(expr), span });
        } else {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec!["`;` or `}`"],
                    found: self.peek_kind(),
                },
                span,
            ));
            let stmt_span = expr.span;
            stmts.push(Stmt { kind: StmtKind::Expr(expr), span: stmt_span });
            if !self.at(TokenKind::Eof) && !self.at(TokenKind::RBrace) {
                self.bump();
            }
        }
    }

    // ---- primary -----------------------------------------------------------

    fn parse_primary(&mut self, restrict: Restrict) -> Expr {
        let start = self.peek_span();
        let kind = self.peek_kind();
        match kind {
            TokenKind::Int { base, has_suffix } => {
                let tok = self.bump();
                let (raw, suffix) = self.split_numeric(tok.span, base, has_suffix);
                Expr {
                    kind: ExprKind::Int(IntLit { raw, base, suffix }),
                    span: tok.span,
                }
            }
            TokenKind::Float { has_suffix } => {
                let tok = self.bump();
                let text = self.slice(tok.span);
                let (raw, suffix) = self.split_float_suffix(text, has_suffix);
                Expr {
                    kind: ExprKind::Float(FloatLit { raw, suffix }),
                    span: tok.span,
                }
            }
            TokenKind::Char => {
                let tok = self.bump();
                let raw = self.slice(tok.span).to_string();
                // We do a light decode just enough to fill `value`; if there
                // are errors we still preserve the raw text.
                let value = decode_char_literal(&raw).unwrap_or('\0');
                Expr {
                    kind: ExprKind::Char(CharLit { raw }),
                    span: tok.span,
                }
                .with_dummy_value(value)
            }
            TokenKind::StrStart => self.parse_string_expr(),
            TokenKind::Kw(Keyword::True) => {
                let tok = self.bump();
                Expr { kind: ExprKind::Bool(true), span: tok.span }
            }
            TokenKind::Kw(Keyword::False) => {
                let tok = self.bump();
                Expr { kind: ExprKind::Bool(false), span: tok.span }
            }
            TokenKind::Kw(Keyword::Null) => {
                let tok = self.bump();
                Expr { kind: ExprKind::Null, span: tok.span }
            }
            TokenKind::Kw(Keyword::SelfLower) => {
                let tok = self.bump();
                Expr { kind: ExprKind::SelfExpr, span: tok.span }
            }
            TokenKind::Underscore => {
                let tok = self.bump();
                Expr { kind: ExprKind::Underscore, span: tok.span }
            }
            TokenKind::Kw(Keyword::Return) => self.parse_return(),
            TokenKind::Kw(Keyword::Break) => self.parse_break(),
            TokenKind::Kw(Keyword::Continue) => {
                let tok = self.bump();
                Expr { kind: ExprKind::Continue, span: tok.span }
            }
            TokenKind::Kw(Keyword::If) => self.parse_if(),
            TokenKind::Kw(Keyword::Match) => self.parse_match(),
            TokenKind::Kw(Keyword::Loop) => {
                let kw = self.bump();
                let body = self.parse_block();
                let span = kw.span.join(body.span);
                Expr { kind: ExprKind::Loop(body), span }
            }
            TokenKind::Kw(Keyword::While) => {
                let kw = self.bump();
                let cond = self.parse_expr(Restrict { no_struct_lit: true });
                let body = self.parse_block();
                let span = kw.span.join(body.span);
                Expr {
                    kind: ExprKind::While { cond: Box::new(cond), body },
                    span,
                }
            }
            TokenKind::Kw(Keyword::For) => self.parse_for(),
            TokenKind::Kw(Keyword::Async) => {
                let kw = self.bump();
                let body = self.parse_block();
                let span = kw.span.join(body.span);
                Expr { kind: ExprKind::AsyncBlock(body), span }
            }
            TokenKind::Kw(Keyword::Function) => self.parse_anon_function(),
            TokenKind::LBrace => {
                if self.peek_is_map_literal() {
                    self.parse_map_literal()
                } else {
                    let block = self.parse_block();
                    let span = block.span;
                    Expr { kind: ExprKind::Block(block), span }
                }
            }
            TokenKind::LBracket => self.parse_list_literal(),
            TokenKind::LParen => self.parse_paren_or_closure_or_tuple(),
            TokenKind::Ident => self.parse_ident_primary(restrict),
            _ => {
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["expression"],
                        found: self.peek_kind(),
                    },
                    start,
                ));
                // Recovery: produce a placeholder Null and consume one token.
                let tok_span = start;
                if !self.at(TokenKind::Eof) {
                    self.bump();
                }
                Expr { kind: ExprKind::Null, span: tok_span }
            }
        }
    }

    fn parse_return(&mut self) -> Expr {
        let kw = self.bump();
        let value = if can_start_expr(self.peek_kind()) {
            Some(Box::new(self.parse_expr(Restrict::default())))
        } else {
            None
        };
        let span = match &value {
            Some(v) => kw.span.join(v.span),
            None => kw.span,
        };
        Expr { kind: ExprKind::Return(value), span }
    }

    fn parse_break(&mut self) -> Expr {
        let kw = self.bump();
        let value = if can_start_expr(self.peek_kind()) {
            Some(Box::new(self.parse_expr(Restrict::default())))
        } else {
            None
        };
        let span = match &value {
            Some(v) => kw.span.join(v.span),
            None => kw.span,
        };
        Expr { kind: ExprKind::Break(value), span }
    }

    fn parse_if(&mut self) -> Expr {
        let kw = self.bump(); // `if`
        let cond = self.parse_expr(Restrict { no_struct_lit: true });
        let then_block = self.parse_block();
        let else_branch = if self.eat_kw(Keyword::Else).is_some() {
            if self.at_kw(Keyword::If) {
                Some(ElseBranch::If(Box::new(self.parse_if())))
            } else {
                Some(ElseBranch::Block(self.parse_block()))
            }
        } else {
            None
        };
        let end = match &else_branch {
            Some(ElseBranch::If(e)) => e.span,
            Some(ElseBranch::Block(b)) => b.span,
            None => then_block.span,
        };
        Expr {
            kind: ExprKind::If {
                cond: Box::new(cond),
                then_block,
                else_branch,
            },
            span: kw.span.join(end),
        }
    }

    fn parse_match(&mut self) -> Expr {
        let kw = self.bump(); // `match`
        let scrutinee = self.parse_expr(Restrict { no_struct_lit: true });
        self.expect(TokenKind::LBrace, "`{`");
        let mut arms = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let arm = self.parse_match_arm();
            arms.push(arm);
            // Arms separated by commas; trailing comma allowed.
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`");
        let end = rbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span: kw.span.join(end),
        }
    }

    fn parse_match_arm(&mut self) -> MatchArm {
        let start = self.peek_span();
        let pattern = self.parse_pattern();
        let guard = if self.eat_kw(Keyword::If).is_some() {
            Some(self.parse_expr(Restrict { no_struct_lit: true }))
        } else {
            None
        };
        if self.eat(TokenKind::FatArrow).is_none() {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::MissingArrowInMatch,
                span,
            ));
        }
        let body = self.parse_expr(Restrict::default());
        let span = start.join(body.span);
        MatchArm { pattern, guard, body, span }
    }

    fn parse_for(&mut self) -> Expr {
        let kw = self.bump(); // `for`
        let in_async = self.eat_kw(Keyword::Await).is_some();
        let pattern = self.parse_pattern();
        if self.eat_kw(Keyword::In).is_none() {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec!["`in`"],
                    found: self.peek_kind(),
                },
                span,
            ));
        }
        let iter = self.parse_expr(Restrict { no_struct_lit: true });
        let body = self.parse_block();
        let span = kw.span.join(body.span);
        Expr {
            kind: ExprKind::For {
                pattern,
                in_async,
                iter: Box::new(iter),
                body,
            },
            span,
        }
    }

    fn parse_anon_function(&mut self) -> Expr {
        let kw = self.bump(); // `function`
        // No name: anonymous.
        let generics = self.parse_optional_generic_params();
        self.expect(TokenKind::LParen, "`(`");
        let params = self.parse_param_list();
        self.expect(TokenKind::RParen, "`)`");
        let return_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        let is_async = self.eat_kw(Keyword::Async).is_some();
        let body = self.parse_block();
        let span = kw.span.join(body.span);
        let function = FunctionItem {
            name: Ident::new("", kw.span),
            generics,
            params,
            return_type,
            is_async,
            body: Some(body),
        };
        Expr {
            kind: ExprKind::AnonFn(Box::new(function)),
            span,
        }
    }

    fn parse_list_literal(&mut self) -> Expr {
        let lb = self.bump(); // `[`
        let mut elems = Vec::new();
        while !self.at(TokenKind::RBracket) && !self.at(TokenKind::Eof) {
            elems.push(self.parse_expr(Restrict::default()));
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rb = self.expect(TokenKind::RBracket, "`]`");
        let end = rb.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Expr { kind: ExprKind::List(elems), span: lb.span.join(end) }
    }

    /// `(` already at front. Could be:
    /// - `(expr)` grouping
    /// - `(a, b, c)` tuple
    /// - `() => body`, `(p, ...) => body`, `(p, ...) async => body`,
    ///   `(p, ...): R => body` — closure
    fn parse_paren_or_closure_or_tuple(&mut self) -> Expr {
        let cp = self.checkpoint();
        if let Some(c) = self.try_parse_closure_starting_with_lparen() {
            return c;
        }
        self.restore(cp);
        let lp = self.bump(); // `(`
        if self.at(TokenKind::RParen) {
            // `()` — not legal as an expression.
            let rp = self.bump();
            self.error(ParseError::new(
                ParseErrorKind::UnitLiteralIsInvalid,
                lp.span.join(rp.span),
            ));
            return Expr {
                kind: ExprKind::Null,
                span: lp.span.join(rp.span),
            };
        }
        let first = self.parse_expr(Restrict::default());
        if self.at(TokenKind::Comma) {
            let mut elems = vec![first];
            while self.eat(TokenKind::Comma).is_some() {
                if self.at(TokenKind::RParen) {
                    break;
                }
                elems.push(self.parse_expr(Restrict::default()));
            }
            let rp = self.expect(TokenKind::RParen, "`)`");
            let end = rp.map(|t| t.span).unwrap_or(self.peek_span());
            Expr { kind: ExprKind::Tuple(elems), span: lp.span.join(end) }
        } else {
            let rp = self.expect(TokenKind::RParen, "`)`");
            let end = rp.map(|t| t.span).unwrap_or(self.peek_span());
            Expr { kind: ExprKind::Paren(Box::new(first)), span: lp.span.join(end) }
        }
    }

    /// Attempt a closure parse. Returns `Some` if we committed.
    fn try_parse_closure_starting_with_lparen(&mut self) -> Option<Expr> {
        let lp = self.bump(); // `(`
        let mut params = Vec::new();
        loop {
            if self.at(TokenKind::RParen) {
                break;
            }
            if !matches!(self.peek_kind(), TokenKind::Ident) {
                return None;
            }
            let name_tok = self.bump();
            let name = self.ident_from(name_tok);
            let ty = if self.eat(TokenKind::Colon).is_some() {
                Some(self.parse_type())
            } else {
                None
            };
            let span = name.span.join(ty.as_ref().map(|t| t.span).unwrap_or(name.span));
            params.push(ClosureParam { name, ty, span });
            if self.eat(TokenKind::Comma).is_some() {
                continue;
            }
            break;
        }
        if !self.at(TokenKind::RParen) {
            return None;
        }
        self.bump();
        // Optional `: ReturnType`
        let return_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };
        let is_async = self.eat_kw(Keyword::Async).is_some();
        if self.eat(TokenKind::FatArrow).is_none() {
            return None;
        }
        let body = if self.at(TokenKind::LBrace) {
            let block = self.parse_block();
            let span = block.span;
            Expr { kind: ExprKind::Block(block), span }
        } else {
            self.parse_expr(Restrict::default())
        };
        let span = lp.span.join(body.span);
        Some(Expr {
            kind: ExprKind::Closure {
                params,
                return_type,
                is_async,
                body: Box::new(body),
            },
            span,
        })
    }

    fn parse_ident_primary(&mut self, restrict: Restrict) -> Expr {
        // We need to consider three forms starting with `Ident`:
        //   (a) `Ident { fields }` — struct literal
        //   (b) `Ident < types > { fields }` — generic struct literal
        //   (c) plain identifier expression
        // We commit speculatively for (b); for (a) and (c) we don't need to.
        if !restrict.no_struct_lit && self.try_struct_literal_head() {
            return self.parse_struct_literal();
        }
        let tok = self.bump();
        let name = self.ident_from(tok);
        Expr { kind: ExprKind::Ident(name.clone()), span: name.span }
    }

    fn try_struct_literal_head(&mut self) -> bool {
        // After Ident, do we see `{` or `<types> {`?
        let next = self.peek_kind_at(1);
        if matches!(next, TokenKind::LBrace) {
            // `{` is at offset 1; the first token inside it is at offset 2.
            return self.peek_is_struct_literal_body(2);
        }
        if matches!(next, TokenKind::Lt) {
            // Speculative parse of generic args. If successful and the next
            // token after `>` is `{`, this is a generic struct literal.
            let cp = self.checkpoint();
            self.bump(); // ident
            self.bump(); // <
            loop {
                if self.eat_close_angle() {
                    break;
                }
                let _ = self.parse_type();
                if self.eat(TokenKind::Comma).is_some() {
                    continue;
                }
                if !self.eat_close_angle() {
                    self.restore(cp);
                    return false;
                }
                break;
            }
            // After consuming `<...>`, the current token (offset 0) should
            // be `{`. The first content token is at offset 1.
            let head = self.at(TokenKind::LBrace) && self.peek_is_struct_literal_body(1);
            self.restore(cp);
            return head;
        }
        false
    }

    /// `inside_offset` is the offset of the first token *inside* the `{`.
    /// Checks whether that content looks like struct-literal fields rather
    /// than a closure body or a free block.
    fn peek_is_struct_literal_body(&self, inside_offset: usize) -> bool {
        match self.peek_kind_at(inside_offset) {
            TokenKind::RBrace => true,
            TokenKind::DotDot => true, // spread
            TokenKind::Ident => {
                let after = self.peek_kind_at(inside_offset + 1);
                matches!(after, TokenKind::Colon | TokenKind::Comma | TokenKind::RBrace)
            }
            _ => false,
        }
    }

    /// At a `{`: decide whether this opens a map literal rather than a block.
    /// Rule (docs/18 §6): `{}` is the empty block; a leading `..` spread, or a
    /// `<key-expression> :`, commits to a map literal.
    fn peek_is_map_literal(&mut self) -> bool {
        let cp = self.checkpoint();
        self.bump(); // `{`
        let verdict = match self.peek_kind() {
            TokenKind::RBrace => false,      // `{}` is the empty block
            TokenKind::DotDot => true,       // `{ ..base }` is a map spread
            _ => {
                let _ = self.parse_expr(Restrict { no_struct_lit: true });
                self.at(TokenKind::Colon)
            }
        };
        self.restore(cp);
        verdict
    }

    fn parse_map_literal(&mut self) -> Expr {
        let lbrace = self.bump(); // `{`
        let mut items = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::DotDot) {
                self.bump();
                let base = self.parse_expr(Restrict::default());
                items.push(MapItem::Spread(Box::new(base)));
            } else {
                let key = self.parse_expr(Restrict { no_struct_lit: true });
                self.expect(TokenKind::Colon, "`:`");
                let value = self.parse_expr(Restrict::default());
                let span = key.span.join(value.span);
                items.push(MapItem::Entry {
                    key: Box::new(key),
                    value: Box::new(value),
                    span,
                });
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`");
        let end = rbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Expr { kind: ExprKind::MapLit(items), span: lbrace.span.join(end) }
    }

    fn parse_struct_literal(&mut self) -> Expr {
        let name_tok = self.bump();
        let name = self.ident_from(name_tok);
        let mut generics = Vec::new();
        if self.eat(TokenKind::Lt).is_some() {
            loop {
                if self.eat_close_angle() {
                    break;
                }
                generics.push(self.parse_type());
                if self.eat(TokenKind::Comma).is_some() {
                    continue;
                }
                if !self.eat_close_angle() {
                    let span = self.peek_span();
                    self.error(ParseError::new(
                        ParseErrorKind::Expected {
                            expected: vec!["`,` or `>`"],
                            found: self.peek_kind(),
                        },
                        span,
                    ));
                }
                break;
            }
        }
        let path_end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(name.span);
        let path = TypePath {
            name: name.clone(),
            generics,
            span: name.span.join(path_end),
        };
        self.expect(TokenKind::LBrace, "`{`");
        let mut fields = Vec::new();
        let mut spread: Option<Box<Expr>> = None;
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::DotDot) {
                let dd = self.bump();
                let base = self.parse_expr(Restrict::default());
                if spread.is_some() {
                    self.error(ParseError::new(
                        ParseErrorKind::Message(
                            "struct literal can only have one `..` spread".into(),
                        ),
                        dd.span.join(base.span),
                    ));
                }
                spread = Some(Box::new(base));
                if self.eat(TokenKind::Comma).is_none() {
                    break;
                }
                continue;
            }
            let f_start = self.peek_span();
            let name_tok = self.bump();
            let f_name = match name_tok.kind {
                TokenKind::Ident => self.ident_from(name_tok),
                _ => {
                    self.error(ParseError::new(
                        ParseErrorKind::Expected {
                            expected: vec!["field name"],
                            found: name_tok.kind,
                        },
                        name_tok.span,
                    ));
                    Ident::new("<error>", name_tok.span)
                }
            };
            let value = if self.eat(TokenKind::Colon).is_some() {
                Some(self.parse_expr(Restrict::default()))
            } else {
                None
            };
            let end = value.as_ref().map(|v| v.span).unwrap_or(f_name.span);
            fields.push(FieldInit { name: f_name, value, span: f_start.join(end) });
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`");
        let end = rbrace.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        let span = path.span.join(end);
        Expr {
            kind: ExprKind::StructLit { path, fields, spread },
            span,
        }
    }

    // ---- string literal parsing -------------------------------------------

    fn parse_string_expr(&mut self) -> Expr {
        let lit = self.parse_string_literal();
        let span = lit.span;
        Expr { kind: ExprKind::Str(lit), span }
    }

    fn parse_string_literal(&mut self) -> StringLit {
        let start_tok = match self.eat(TokenKind::StrStart) {
            Some(t) => t,
            None => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["string literal"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                return StringLit { parts: Vec::new(), span };
            }
        };
        let mut parts = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::StrText => {
                    let tok = self.bump();
                    parts.push(StringPart::Text {
                        text: self.slice(tok.span).to_string(),
                        span: tok.span,
                    });
                }
                TokenKind::DollarIdent => {
                    let tok = self.bump();
                    // The slice is `$name`; we strip the `$` for the ident.
                    let full = self.slice(tok.span);
                    let name_str = &full[1..];
                    let name_span = Span::new(
                        tok.span.file,
                        BytePos(tok.span.lo.0 + 1),
                        tok.span.hi,
                    );
                    parts.push(StringPart::Ident(Ident::new(name_str.to_string(), name_span)));
                }
                TokenKind::DollarLBrace => {
                    self.bump();
                    let inner = self.parse_expr(Restrict::default());
                    self.expect(TokenKind::RBrace, "`}` to close `${`");
                    parts.push(StringPart::Expr(inner));
                }
                TokenKind::StrEnd => {
                    let end_tok = self.bump();
                    let span = start_tok.span.join(end_tok.span);
                    return StringLit { parts, span };
                }
                _ => {
                    let span = self.peek_span();
                    self.error(ParseError::new(
                        ParseErrorKind::UnexpectedEof {
                            expected: "closing `\"`",
                        },
                        span,
                    ));
                    return StringLit {
                        parts,
                        span: start_tok.span.join(span),
                    };
                }
            }
        }
    }
}

fn can_start_expr(k: TokenKind) -> bool {
    use TokenKind::*;
    match k {
        Ident | Underscore => true,
        Int { .. } | Float { .. } | Char | StrStart => true,
        Kw(kw) => matches!(
            kw,
            Keyword::True
                | Keyword::False
                | Keyword::Null
                | Keyword::SelfLower
                | Keyword::If
                | Keyword::Match
                | Keyword::Loop
                | Keyword::While
                | Keyword::For
                | Keyword::Async
                | Keyword::Function
                | Keyword::Return
                | Keyword::Break
                | Keyword::Continue
                | Keyword::Await
                | Keyword::Spawn
        ),
        LParen | LBracket | LBrace => true,
        Minus | Bang | Tilde | Amp | Star => true,
        _ => false,
    }
}

// ===========================================================================
// Patterns
// ===========================================================================

impl<'src> Parser<'src> {
    fn parse_pattern(&mut self) -> Pattern {
        // Or-patterns: P1 | P2 | P3
        let first = self.parse_single_pattern();
        if !self.at(TokenKind::Pipe) {
            return first;
        }
        let mut alts = vec![first];
        while self.eat(TokenKind::Pipe).is_some() {
            alts.push(self.parse_single_pattern());
        }
        let span = alts.first().unwrap().span.join(alts.last().unwrap().span);
        Pattern { kind: PatternKind::Or(alts), span }
    }

    fn parse_single_pattern(&mut self) -> Pattern {
        let start = self.peek_span();
        match self.peek_kind() {
            TokenKind::Underscore => {
                let tok = self.bump();
                Pattern { kind: PatternKind::Wildcard, span: tok.span }
            }
            TokenKind::Minus
            | TokenKind::Int { .. }
            | TokenKind::Float { .. }
            | TokenKind::Char
            | TokenKind::StrStart
            | TokenKind::Kw(Keyword::True)
            | TokenKind::Kw(Keyword::False)
            | TokenKind::Kw(Keyword::Null) => {
                let expr = self.parse_unary(Restrict::default());
                let span = expr.span;
                Pattern { kind: PatternKind::Literal(Box::new(expr)), span }
            }
            TokenKind::LParen => self.parse_tuple_pattern(),
            TokenKind::LBracket => self.parse_list_pattern(),
            TokenKind::Ident => self.parse_named_pattern(),
            TokenKind::Kw(Keyword::SelfUpper) => {
                // `Self` could be a unit-path pattern (rare but legal).
                let tok = self.bump();
                let path = TypePath {
                    name: Ident::new("Self", tok.span),
                    generics: Vec::new(),
                    span: tok.span,
                };
                Pattern { kind: PatternKind::UnitPath(path), span: tok.span }
            }
            _ => {
                let span = self.peek_span();
                self.error(ParseError::new(
                    ParseErrorKind::Expected {
                        expected: vec!["pattern"],
                        found: self.peek_kind(),
                    },
                    span,
                ));
                if !self.at(TokenKind::Eof) {
                    self.bump();
                }
                Pattern { kind: PatternKind::Wildcard, span: start }
            }
        }
    }

    fn parse_named_pattern(&mut self) -> Pattern {
        // Could be:
        //   `Type name` — type-binding (e.g. `i64 n`)
        //   `Type` — type-only pattern (`i64`)
        //   `Path` — unit-struct pattern (`Red`)
        //   `Path(p, p, ..)` — tuple-struct pattern
        //   `Path { f, .. }` — record-struct pattern
        //   `Path<T>...` — path with generics in any of the above forms
        //   plain `name` — binding pattern
        // We parse a type-style path first; then decide based on the follow-set.
        let start = self.peek_span();
        let first_tok = self.bump();
        let name = self.ident_from(first_tok);
        let mut generics = Vec::new();
        if self.at(TokenKind::Lt) {
            // Speculatively parse generic args if they parse and end with `>`.
            let cp = self.checkpoint();
            self.bump();
            let parsed = (|| {
                loop {
                    if self.eat_close_angle() {
                        return true;
                    }
                    let ty = self.parse_type();
                    generics.push(ty);
                    if self.eat(TokenKind::Comma).is_some() {
                        continue;
                    }
                    return self.eat_close_angle();
                }
            })();
            if !parsed {
                self.restore(cp);
                generics.clear();
            }
        }
        let path = TypePath {
            name: name.clone(),
            generics,
            span: start.join(
                self.tokens
                    .get(self.pos.saturating_sub(1))
                    .map(|t| t.span)
                    .unwrap_or(name.span),
            ),
        };

        match self.peek_kind() {
            TokenKind::LParen => {
                self.bump();
                let mut elems = Vec::new();
                let mut rest: Option<RestPattern> = None;
                while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
                    if self.at(TokenKind::DotDot) {
                        let dd = self.bump();
                        let bind = if matches!(self.peek_kind(), TokenKind::Ident) {
                            let t = self.bump();
                            Some(self.ident_from(t))
                        } else {
                            None
                        };
                        let span = bind
                            .as_ref()
                            .map(|b| dd.span.join(b.span))
                            .unwrap_or(dd.span);
                        if rest.is_some() {
                            self.error(ParseError::new(
                                ParseErrorKind::DuplicateRestBinding,
                                span,
                            ));
                        }
                        rest = Some(RestPattern { name: bind, span });
                    } else {
                        elems.push(self.parse_pattern());
                    }
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                let rp = self.expect(TokenKind::RParen, "`)`");
                let end = rp.map(|t| t.span).unwrap_or_else(|| self.peek_span());
                Pattern {
                    kind: PatternKind::TupleStruct { path, fields: elems, rest },
                    span: start.join(end),
                }
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                let mut has_rest = false;
                while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
                    if self.at(TokenKind::DotDot) {
                        self.bump();
                        has_rest = true;
                        let _ = self.eat(TokenKind::Comma);
                        break;
                    }
                    let f_start = self.peek_span();
                    let f_name_tok = self.bump();
                    let f_name = match f_name_tok.kind {
                        TokenKind::Ident => self.ident_from(f_name_tok),
                        _ => {
                            self.error(ParseError::new(
                                ParseErrorKind::Expected {
                                    expected: vec!["field name"],
                                    found: f_name_tok.kind,
                                },
                                f_name_tok.span,
                            ));
                            Ident::new("<error>", f_name_tok.span)
                        }
                    };
                    let pat = if self.eat(TokenKind::Colon).is_some() {
                        Some(self.parse_pattern())
                    } else {
                        None
                    };
                    let end = pat.as_ref().map(|p| p.span).unwrap_or(f_name.span);
                    fields.push(FieldPattern {
                        name: f_name,
                        pattern: pat,
                        span: f_start.join(end),
                    });
                    if self.eat(TokenKind::Comma).is_none() {
                        break;
                    }
                }
                let rb = self.expect(TokenKind::RBrace, "`}`");
                let end = rb.map(|t| t.span).unwrap_or_else(|| self.peek_span());
                Pattern {
                    kind: PatternKind::RecordStruct { path, fields, has_rest },
                    span: start.join(end),
                }
            }
            TokenKind::Ident => {
                // `Type name` — type binding.
                let bind_tok = self.bump();
                let bind = self.ident_from(bind_tok);
                let ty = Type {
                    kind: TypeKind::Named {
                        name: path.name.clone(),
                        generics: path.generics.clone(),
                    },
                    span: path.span,
                };
                let span = path.span.join(bind.span);
                Pattern {
                    kind: PatternKind::TypeBinding {
                        ty,
                        binding: Some(bind),
                    },
                    span,
                }
            }
            _ => {
                // Plain ident or path-as-unit-pattern. Decision tree:
                //   1. If the name is a primitive type and has no generics,
                //      it's a TypeBinding (matches any value of that type).
                //   2. If the name starts with a lowercase non-`_` letter and
                //      has no generics, it's a binding pattern.
                //   3. Otherwise it's a unit-struct path pattern.
                if path.generics.is_empty() && is_primitive_type_name(&path.name.name) {
                    let ty = Type {
                        kind: TypeKind::Named {
                            name: path.name.clone(),
                            generics: Vec::new(),
                        },
                        span: path.span,
                    };
                    Pattern {
                        kind: PatternKind::TypeBinding { ty, binding: None },
                        span: path.span,
                    }
                } else if path.generics.is_empty()
                    && path
                        .name
                        .name
                        .chars()
                        .next()
                        .map_or(false, |c| !c.is_uppercase())
                {
                    Pattern {
                        kind: PatternKind::Binding(path.name),
                        span: path.span,
                    }
                } else {
                    Pattern {
                        kind: PatternKind::UnitPath(path.clone()),
                        span: path.span,
                    }
                }
            }
        }
    }

    fn parse_tuple_pattern(&mut self) -> Pattern {
        let lp = self.bump();
        let mut elems = Vec::new();
        let mut rest: Option<(usize, RestPattern)> = None;
        let mut idx = 0;
        while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::DotDot) {
                let dd = self.bump();
                let bind = if matches!(self.peek_kind(), TokenKind::Ident) {
                    let t = self.bump();
                    Some(self.ident_from(t))
                } else {
                    None
                };
                let span = bind.as_ref().map(|b| dd.span.join(b.span)).unwrap_or(dd.span);
                if rest.is_some() {
                    self.error(ParseError::new(
                        ParseErrorKind::DuplicateRestBinding,
                        span,
                    ));
                } else {
                    rest = Some((idx, RestPattern { name: bind, span }));
                }
            } else {
                elems.push(self.parse_pattern());
                idx += 1;
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rp = self.expect(TokenKind::RParen, "`)`");
        let end = rp.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Pattern {
            kind: PatternKind::Tuple { elems, rest },
            span: lp.span.join(end),
        }
    }

    fn parse_list_pattern(&mut self) -> Pattern {
        let lb = self.bump();
        let mut elems = Vec::new();
        let mut rest: Option<(usize, RestPattern)> = None;
        let mut idx = 0;
        while !self.at(TokenKind::RBracket) && !self.at(TokenKind::Eof) {
            if self.at(TokenKind::DotDot) {
                let dd = self.bump();
                let bind = if matches!(self.peek_kind(), TokenKind::Ident) {
                    let t = self.bump();
                    Some(self.ident_from(t))
                } else {
                    None
                };
                let span = bind.as_ref().map(|b| dd.span.join(b.span)).unwrap_or(dd.span);
                if rest.is_some() {
                    self.error(ParseError::new(
                        ParseErrorKind::DuplicateRestBinding,
                        span,
                    ));
                } else {
                    rest = Some((idx, RestPattern { name: bind, span }));
                }
            } else {
                elems.push(self.parse_pattern());
                idx += 1;
            }
            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }
        let rb = self.expect(TokenKind::RBracket, "`]`");
        let end = rb.map(|t| t.span).unwrap_or_else(|| self.peek_span());
        Pattern {
            kind: PatternKind::List { elems, rest },
            span: lb.span.join(end),
        }
    }
}

fn is_primitive_type_name(s: &str) -> bool {
    matches!(
        s,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "isize"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
            | "null"
            | "dynamic"
    )
}

// ===========================================================================
// Helpers: identifiers and literal splitting
// ===========================================================================

impl<'src> Parser<'src> {
    fn ident_from(&self, tok: Token) -> Ident {
        Ident::new(self.slice(tok.span).to_string(), tok.span)
    }

    fn expect_ident(&mut self, what: &'static str) -> Option<Ident> {
        if matches!(self.peek_kind(), TokenKind::Ident) {
            let tok = self.bump();
            Some(self.ident_from(tok))
        } else {
            let span = self.peek_span();
            self.error(ParseError::new(
                ParseErrorKind::Expected {
                    expected: vec![what],
                    found: self.peek_kind(),
                },
                span,
            ));
            None
        }
    }

    /// Split an integer literal token's text into `(raw_digits, suffix?)`.
    /// The `base` controls what counts as a digit; the rest goes to the suffix.
    fn split_numeric(&self, span: Span, base: IntBase, has_suffix: bool) -> (String, Option<String>) {
        let text = self.slice(span);
        // Skip base prefix if any.
        let body = match base {
            IntBase::Hex | IntBase::Oct | IntBase::Bin => &text[2..],
            IntBase::Dec => text,
        };
        if !has_suffix {
            return (body.to_string(), None);
        }
        // Find where the suffix begins: the first byte that is i/u/f
        // *after* digits. The lexer guarantees one suffix at the end.
        let mut split = body.len();
        let bytes = body.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(b, b'i' | b'u' | b'f') {
                // ensure prior char was digit or '_'
                if i == 0 {
                    continue;
                }
                split = i;
                break;
            }
        }
        let (raw, suf) = body.split_at(split);
        (raw.to_string(), Some(suf.to_string()))
    }

    fn split_float_suffix(&self, text: &str, has_suffix: bool) -> (String, Option<String>) {
        if !has_suffix {
            return (text.to_string(), None);
        }
        let bytes = text.as_bytes();
        let mut split = text.len();
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(b, b'f') {
                if i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'_') {
                    split = i;
                    break;
                }
            }
        }
        let (raw, suf) = text.split_at(split);
        (raw.to_string(), Some(suf.to_string()))
    }
}

// `decode_char_literal` is a minimal helper to extract the scalar value from
// a raw `'…'` slice. It is *not* meant to be exhaustive — invalid escapes
// have already been flagged by the lexer; this just produces something for
// the AST node so consumers can pattern-match on a `char`.
fn decode_char_literal(raw: &str) -> Option<char> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        if chars.next().is_some() {
            return None;
        }
        return Some(first);
    }
    let esc = chars.next()?;
    let decoded = match esc {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '$' => '$',
        '0' => '\0',
        'x' => {
            let h1 = chars.next()?;
            let h2 = chars.next()?;
            let n = u32::from_str_radix(&format!("{h1}{h2}"), 16).ok()?;
            char::from_u32(n)?
        }
        'u' => {
            // `{HHHHHH}`
            if chars.next()? != '{' {
                return None;
            }
            let mut digits = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                digits.push(c);
            }
            let n = u32::from_str_radix(&digits, 16).ok()?;
            char::from_u32(n)?
        }
        _ => return None,
    };
    if chars.next().is_some() {
        return None;
    }
    Some(decoded)
}

// `with_dummy_value` is a tiny convenience: `parse_primary` constructs the
// `CharLit` and a decoded `char` value in parallel; we just forward the
// already-constructed `Expr` so the signature stays clean.
trait WithDummy {
    fn with_dummy_value(self, _: char) -> Self;
}
impl WithDummy for Expr {
    fn with_dummy_value(self, _: char) -> Self {
        self
    }
}
