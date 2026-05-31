//! Host functions backing the `core:compiler` surface (`docs/22` §8).
//!
//! Each function here is registered into the macro JIT under the `__ast_*` /
//! `__mctx_*` symbol expected by the extern declarations in the language
//! prelude. They translate the language's opaque `i64` handles into operations
//! on the per-thread [`crate::arena`] state. All string exchange goes through
//! the runtime `str` representation ([`LangStr`]).

use super::arena::{self, DiagLevel, MacroDiag, Node, ParseOutcome};
use compiler::ast::{Expr, ExprKind, Ident};
use compiler::span::{BytePos, Span};
use runtime::strings::{str_bytes, LangStr};

/// Read a language `str` argument into an owned Rust `String`.
///
/// # Safety
/// `p` must be a valid `str` field-block pointer (the macro JIT guarantees this
/// for every `str`-typed argument).
unsafe fn read_str(p: *const LangStr) -> String {
    if p.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { str_bytes(p) }).into_owned()
}

/// Build a fresh language `str` from Rust bytes.
fn make_str(s: &str) -> *const LangStr {
    unsafe { runtime::lang_str_from_utf8(s.as_ptr(), s.len()) }
}

// ---- ASTNode introspection ------------------------------------------------

pub extern "C" fn ast_kind(h: i64) -> *const LangStr {
    let k = arena::with(|s| s.node(h).map(arena::node_kind).unwrap_or("error"));
    make_str(k)
}

pub extern "C" fn ast_text(h: i64) -> *const LangStr {
    let t = arena::with(|s| s.node(h).map(arena::node_text).unwrap_or_default());
    make_str(&t)
}

pub extern "C" fn ast_name(h: i64) -> *const LangStr {
    let n = arena::with(|s| s.node(h).map(arena::node_name).unwrap_or_default());
    make_str(&n)
}

pub extern "C" fn ast_field_count(h: i64) -> i64 {
    arena::with(|s| s.node(h).map(arena::node_field_count).unwrap_or(0))
}

pub extern "C" fn ast_field_name(h: i64, i: i64) -> *const LangStr {
    let n = arena::with(|s| s.node(h).map(|n| arena::node_field_name(n, i)).unwrap_or_default());
    make_str(&n)
}

pub extern "C" fn ast_is_record(h: i64) -> i64 {
    i64::from(arena::with(|s| s.node(h).map(arena::node_is_record).unwrap_or(false)))
}
pub extern "C" fn ast_is_tuple(h: i64) -> i64 {
    i64::from(arena::with(|s| s.node(h).map(arena::node_is_tuple).unwrap_or(false)))
}
pub extern "C" fn ast_is_unit(h: i64) -> i64 {
    i64::from(arena::with(|s| s.node(h).map(arena::node_is_unit).unwrap_or(false)))
}

pub extern "C" fn ast_span(h: i64) -> i64 {
    arena::with(|s| {
        let sp = s.node(h).map(arena::node_span).unwrap_or_else(Span::dummy);
        s.push_span(sp)
    })
}

pub extern "C" fn ast_error_marker() -> i64 {
    arena::with(|s| s.push_node(Node::ErrorMarker))
}

pub extern "C" fn ast_node_text(h: i64) -> *const LangStr {
    ast_text(h)
}

// ---- MacroContext ---------------------------------------------------------

fn ctx_idx(c: i64) -> Option<usize> {
    usize::try_from(c).ok()
}

pub extern "C" fn mctx_invocation_span(c: i64) -> i64 {
    arena::with(|s| {
        let sp = ctx_idx(c).and_then(|i| s.contexts.get(i)).map(|x| x.invocation_span).unwrap_or_else(Span::dummy);
        s.push_span(sp)
    })
}

pub extern "C" fn mctx_arg_count(c: i64) -> i64 {
    arena::with(|s| ctx_idx(c).and_then(|i| s.contexts.get(i)).map(|x| x.args.len() as i64).unwrap_or(0))
}

pub extern "C" fn mctx_arg(c: i64, i: i64) -> i64 {
    arena::with(|s| {
        let idx = usize::try_from(i).ok();
        ctx_idx(c)
            .and_then(|ci| s.contexts.get(ci))
            .and_then(|x| idx.and_then(|j| x.args.get(j)))
            .copied()
            .map(|h| h as i64)
            .unwrap_or(-1)
    })
}

pub extern "C" fn mctx_kwarg_has(c: i64, name: *const LangStr) -> i64 {
    let name = unsafe { read_str(name) };
    i64::from(arena::with(|s| {
        ctx_idx(c)
            .and_then(|ci| s.contexts.get(ci))
            .map(|x| x.kwargs.iter().any(|(k, _)| *k == name))
            .unwrap_or(false)
    }))
}

pub extern "C" fn mctx_kwarg(c: i64, name: *const LangStr) -> i64 {
    let name = unsafe { read_str(name) };
    arena::with(|s| {
        ctx_idx(c)
            .and_then(|ci| s.contexts.get(ci))
            .and_then(|x| x.kwargs.iter().find(|(k, _)| *k == name).map(|(_, h)| *h as i64))
            .unwrap_or(-1)
    })
}

fn emit(c: i64, span: i64, msg: *const LangStr, level: DiagLevel) {
    let message = unsafe { read_str(msg) };
    arena::with(|s| {
        let sp = s.span(span);
        if let Some(ctx) = ctx_idx(c).and_then(|i| s.contexts.get_mut(i)) {
            ctx.diags.push(MacroDiag { level, span: sp, message });
        }
    });
}

pub extern "C" fn mctx_error(c: i64, span: i64, msg: *const LangStr) {
    emit(c, span, msg, DiagLevel::Error);
}
pub extern "C" fn mctx_warn(c: i64, span: i64, msg: *const LangStr) {
    emit(c, span, msg, DiagLevel::Warn);
}
pub extern "C" fn mctx_note(c: i64, span: i64, msg: *const LangStr) {
    emit(c, span, msg, DiagLevel::Note);
}

pub extern "C" fn mctx_fresh_ident(_c: i64, hint: *const LangStr) -> i64 {
    let hint = unsafe { read_str(hint) };
    arena::with(|s| {
        let n = s.fresh_ctr;
        s.fresh_ctr += 1;
        // A uniquely-suffixed name in its own virtual file → unique span. True
        // syntax-context hygiene layers on top of this in a later slice.
        let name = format!("{hint}__m{n}");
        let file = s.new_gen_file(name.clone());
        let span = Span::new(file, BytePos(0), BytePos(name.len() as u32));
        s.push_node(Node::Ident(Ident { name, span }))
    })
}

pub extern "C" fn mctx_unhygienic(_c: i64, name: *const LangStr) -> i64 {
    let name = unsafe { read_str(name) };
    arena::with(|s| {
        let file = s.new_gen_file(name.clone());
        let span = Span::new(file, BytePos(0), BytePos(name.len() as u32));
        s.push_node(Node::Ident(Ident { name, span }))
    })
}

/// Shared body for the four `parse_*` host functions: run `parse`, intern the
/// result, and on failure record a macro error and hand back an error marker.
fn parse_into(c: i64, src: *const LangStr, parse: impl FnOnce(&str) -> ParseOutcome) -> i64 {
    let src = unsafe { read_str(src) };
    match parse(&src) {
        ParseOutcome::Ok(node) => arena::with(|s| s.push_node(node)),
        ParseOutcome::Err(message, span) => arena::with(|s| {
            if let Some(ctx) = ctx_idx(c).and_then(|i| s.contexts.get_mut(i)) {
                ctx.diags.push(MacroDiag { level: DiagLevel::Error, span, message });
            }
            s.push_node(Node::ErrorMarker)
        }),
    }
}

pub extern "C" fn mctx_parse_item(c: i64, src: *const LangStr) -> i64 {
    parse_into(c, src, arena::parse_item)
}
pub extern "C" fn mctx_parse_items(c: i64, src: *const LangStr) -> i64 {
    parse_into(c, src, arena::parse_items)
}
pub extern "C" fn mctx_parse_expr(c: i64, src: *const LangStr) -> i64 {
    parse_into(c, src, arena::parse_expr)
}
pub extern "C" fn mctx_parse_block(c: i64, src: *const LangStr) -> i64 {
    parse_into(c, src, arena::parse_block)
}

/// The `(symbol, address)` table to register into the macro JIT so the prelude's
/// `extern function __ast_* / __mctx_*` declarations resolve.
pub fn symbols() -> Vec<(&'static str, *const u8)> {
    vec![
        ("__ast_kind", ast_kind as *const u8),
        ("__ast_text", ast_text as *const u8),
        ("__ast_name", ast_name as *const u8),
        ("__ast_field_count", ast_field_count as *const u8),
        ("__ast_field_name", ast_field_name as *const u8),
        ("__ast_is_record", ast_is_record as *const u8),
        ("__ast_is_tuple", ast_is_tuple as *const u8),
        ("__ast_is_unit", ast_is_unit as *const u8),
        ("__ast_span", ast_span as *const u8),
        ("__ast_error_marker", ast_error_marker as *const u8),
        ("__mctx_node_text", ast_node_text as *const u8),
        ("__mctx_invocation_span", mctx_invocation_span as *const u8),
        ("__mctx_arg_count", mctx_arg_count as *const u8),
        ("__mctx_arg", mctx_arg as *const u8),
        ("__mctx_kwarg", mctx_kwarg as *const u8),
        ("__mctx_kwarg_has", mctx_kwarg_has as *const u8),
        ("__mctx_error", mctx_error as *const u8),
        ("__mctx_warn", mctx_warn as *const u8),
        ("__mctx_note", mctx_note as *const u8),
        ("__mctx_fresh_ident", mctx_fresh_ident as *const u8),
        ("__mctx_unhygienic", mctx_unhygienic as *const u8),
        ("__mctx_parse_item", mctx_parse_item as *const u8),
        ("__mctx_parse_items", mctx_parse_items as *const u8),
        ("__mctx_parse_expr", mctx_parse_expr as *const u8),
        ("__mctx_parse_block", mctx_parse_block as *const u8),
    ]
}

/// Build an `Expr`-bearing arg node from an attribute argument expression.
pub fn intern_arg_expr(e: Expr) -> usize {
    arena::with(|s| s.push_node(Node::Expr(e)) as usize)
}

/// Convenience: is an expression a bare identifier? (used to surface arg names.)
#[allow(dead_code)] // used by argument-coercion helpers in a later slice
pub fn expr_ident(e: &Expr) -> Option<&Ident> {
    match &e.kind {
        ExprKind::Ident(id) => Some(id),
        _ => None,
    }
}
