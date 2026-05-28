//! Integration tests for the lexer.
//!
//! These focus on the corners the unit tests don't cover: exact span byte
//! ranges, every error variant, every keyword, every escape form, Unicode
//! identifiers, and line/column reporting via `SourceFile::line_col`.

use compiler::{
    lex, BytePos, IntBase, Keyword, LexErrorKind, SourceMap, Span, Token, TokenKind,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lex_str(src: &str) -> (Vec<Token>, Vec<compiler::LexError>, SourceMap) {
    let mut sm = SourceMap::new();
    let file = sm.add_file("t", src);
    let owned = sm.file(file).src.clone();
    let (toks, errs) = lex(&owned, file);
    (toks, errs, sm)
}

fn kinds(src: &str) -> Vec<TokenKind> {
    let (t, _e, _sm) = lex_str(src);
    t.into_iter().map(|t| t.kind).collect()
}

fn errs(src: &str) -> Vec<LexErrorKind> {
    let (_t, e, _sm) = lex_str(src);
    e.into_iter().map(|e| e.kind).collect()
}

fn span(lo: u32, hi: u32) -> (BytePos, BytePos) {
    (BytePos(lo), BytePos(hi))
}

// ---------------------------------------------------------------------------
// Span correctness
// ---------------------------------------------------------------------------

#[test]
fn spans_cover_every_byte_of_a_run() {
    //               0123456789012345
    let src = "var foo = 42 ;";
    let (toks, _e, _sm) = lex_str(src);
    let got: Vec<_> = toks.iter().map(|t| (t.kind.clone(), (t.span.lo, t.span.hi))).collect();
    assert_eq!(
        got,
        vec![
            (TokenKind::Kw(Keyword::Var), span(0, 3)),
            (TokenKind::Ident, span(4, 7)),
            (TokenKind::Eq, span(8, 9)),
            (
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                span(10, 12),
            ),
            (TokenKind::Semi, span(13, 14)),
            (TokenKind::Eof, span(14, 14)),
        ]
    );
}

#[test]
fn multi_char_operator_spans_are_contiguous() {
    let src = "==!=<=>=&&||<<>>=>..";
    let (toks, _e, _sm) = lex_str(src);
    let positions: Vec<_> = toks
        .iter()
        .take_while(|t| t.kind != TokenKind::Eof)
        .map(|t| (t.span.lo.0, t.span.hi.0))
        .collect();
    // Every token should start exactly where the last one ended.
    let mut cursor = 0;
    for (lo, hi) in &positions {
        assert_eq!(*lo, cursor, "gap before token at {lo}..{hi}");
        cursor = *hi;
    }
    assert_eq!(cursor as usize, src.len());
}

#[test]
fn eof_span_is_empty_and_at_end() {
    let src = "x";
    let (toks, _e, _sm) = lex_str(src);
    let eof = toks.last().unwrap();
    assert_eq!(eof.kind, TokenKind::Eof);
    assert!(eof.span.is_empty());
    assert_eq!(eof.span.lo.0 as usize, src.len());
}

#[test]
fn string_token_spans_partition_the_literal() {
    let src = r#""hi $name!""#;
    let (toks, _e, sm) = lex_str(src);
    // Drop the trailing Eof.
    let body: Vec<_> = toks.iter().filter(|t| t.kind != TokenKind::Eof).collect();

    // StrStart covers the opening quote.
    assert_eq!(body[0].kind, TokenKind::StrStart);
    assert_eq!(sm.slice(body[0].span), "\"");

    // StrText is "hi ".
    assert_eq!(body[1].kind, TokenKind::StrText);
    assert_eq!(sm.slice(body[1].span), "hi ");

    // DollarIdent covers the `$name`.
    assert_eq!(body[2].kind, TokenKind::DollarIdent);
    assert_eq!(sm.slice(body[2].span), "$name");

    // Then "!" StrText, closing ".
    assert_eq!(body[3].kind, TokenKind::StrText);
    assert_eq!(sm.slice(body[3].span), "!");

    assert_eq!(body[4].kind, TokenKind::StrEnd);
    assert_eq!(sm.slice(body[4].span), "\"");
}

#[test]
fn span_join_is_smallest_enclosing() {
    // "a b c" → `a` at 0..1, `b` at 2..3, `c` at 4..5.
    let src = "a b c";
    let (toks, _e, _sm) = lex_str(src);
    let joined = toks[0].span.join(toks[2].span);
    assert_eq!(joined.lo.0, 0);
    assert_eq!(joined.hi.0, 5);
}

// ---------------------------------------------------------------------------
// Adjacency: tokens with zero whitespace between them
// ---------------------------------------------------------------------------

#[test]
fn no_whitespace_is_required_between_tokens() {
    assert_eq!(
        kinds("a+b*c"),
        vec![
            TokenKind::Ident,
            TokenKind::Plus,
            TokenKind::Ident,
            TokenKind::Star,
            TokenKind::Ident,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn each_single_char_punct_round_trip() {
    let src = "{}()[],;:.@?~^%+-*/=!<>&|";
    let (toks, e, _sm) = lex_str(src);
    assert!(e.is_empty(), "{e:?}");
    use TokenKind::*;
    let expected = vec![
        LBrace, RBrace, LParen, RParen, LBracket, RBracket, Comma, Semi, Colon, Dot, At, Question,
        Tilde, Caret, Percent, Plus, Minus, Star, Slash, Eq, Bang, Lt, Gt, Amp, Pipe, Eof,
    ];
    assert_eq!(
        toks.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        expected
    );
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

#[test]
fn digit_underscores_are_consumed() {
    let src = "1_000_000 0xFF_FF_FF 0b1010_0101";
    let (toks, e, _sm) = lex_str(src);
    assert!(e.is_empty(), "{e:?}");
    let kinds: Vec<_> = toks.iter().map(|t| t.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            TokenKind::Int { base: IntBase::Dec, has_suffix: false },
            TokenKind::Int { base: IntBase::Hex, has_suffix: false },
            TokenKind::Int { base: IntBase::Bin, has_suffix: false },
            TokenKind::Eof,
        ]
    );
}

#[test]
fn every_int_suffix_is_accepted() {
    let suffixes = ["i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize"];
    for sfx in &suffixes {
        let src = format!("42{sfx}");
        let (toks, e, _sm) = lex_str(&src);
        assert!(e.is_empty(), "{sfx}: {e:?}");
        assert_eq!(
            toks[0].kind,
            TokenKind::Int { base: IntBase::Dec, has_suffix: true },
            "suffix {sfx}"
        );
        // The span should cover the whole literal including the suffix.
        assert_eq!(toks[0].span.hi.0 as usize, src.len());
    }
}

#[test]
fn empty_based_int_is_error() {
    assert_eq!(errs("0x"), vec![LexErrorKind::EmptyIntLiteral]);
    assert_eq!(errs("0b"), vec![LexErrorKind::EmptyIntLiteral]);
    assert_eq!(errs("0o"), vec![LexErrorKind::EmptyIntLiteral]);
}

#[test]
fn invalid_digit_for_base_is_error() {
    let e = errs("0b2");
    assert_eq!(e, vec![LexErrorKind::InvalidDigit]);
    let e = errs("0xZ");
    assert_eq!(e, vec![LexErrorKind::InvalidDigit]);
    let e = errs("0o9");
    assert_eq!(e, vec![LexErrorKind::InvalidDigit]);
}

#[test]
fn float_negative_exponent() {
    let (toks, e, _sm) = lex_str("2.5e-3");
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(toks[0].kind, TokenKind::Float { has_suffix: false });
    assert_eq!(toks[0].span.hi.0, 6);
}

#[test]
fn float_positive_exponent() {
    let (toks, e, _sm) = lex_str("1e+6");
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(toks[0].kind, TokenKind::Float { has_suffix: false });
    assert_eq!(toks[0].span.hi.0, 4);
}

// ---------------------------------------------------------------------------
// Identifiers & keywords
// ---------------------------------------------------------------------------

#[test]
fn every_keyword_is_recognized() {
    use Keyword::*;
    let all = [
        ("var", Var), ("function", Function), ("struct", Struct),
        ("interface", Interface), ("type", Type), ("mod", Mod),
        ("extend", Extend), ("extern", Extern), ("import", Import),
        ("pub", Pub), ("async", Async), ("self", SelfLower), ("Self", SelfUpper),
        ("if", If), ("else", Else), ("match", Match), ("return", Return),
        ("for", For), ("in", In), ("while", While), ("loop", Loop),
        ("break", Break), ("continue", Continue), ("await", Await),
        ("as", As), ("is", Is),
        ("true", True), ("false", False), ("null", Null),
        ("yield", Yield),
    ];
    for (text, kw) in all {
        let (toks, e, _sm) = lex_str(text);
        assert!(e.is_empty(), "{text}: {e:?}");
        assert_eq!(toks[0].kind, TokenKind::Kw(kw), "for keyword {text}");
        assert_eq!(toks[0].span.lo.0, 0);
        assert_eq!(toks[0].span.hi.0 as usize, text.len());
    }
}

#[test]
fn self_keywords_are_case_sensitive() {
    // `self` and `Self` are distinct keywords; `SELF` is a plain identifier.
    let k = kinds("self Self SELF");
    assert_eq!(
        k,
        vec![
            TokenKind::Kw(Keyword::SelfLower),
            TokenKind::Kw(Keyword::SelfUpper),
            TokenKind::Ident,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn unicode_identifier() {
    // `café` is `c a f é` — `é` is XID_Continue. `α` is XID_Start.
    let (toks, e, sm) = lex_str("café α");
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(toks[0].kind, TokenKind::Ident);
    assert_eq!(sm.slice(toks[0].span), "café");
    assert_eq!(toks[1].kind, TokenKind::Ident);
    assert_eq!(sm.slice(toks[1].span), "α");
}

#[test]
fn underscore_is_ident_continue_in_longer_idents() {
    let (toks, _e, sm) = lex_str("foo_bar __init__ x_1");
    assert_eq!(toks[0].kind, TokenKind::Ident);
    assert_eq!(sm.slice(toks[0].span), "foo_bar");
    assert_eq!(toks[1].kind, TokenKind::Ident);
    assert_eq!(sm.slice(toks[1].span), "__init__");
    assert_eq!(toks[2].kind, TokenKind::Ident);
    assert_eq!(sm.slice(toks[2].span), "x_1");
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

#[test]
fn deeply_nested_block_comment() {
    let src = "/* /* /* deep */ */ */ x";
    let (toks, e, _sm) = lex_str(src);
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(
        toks.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Ident, TokenKind::Eof]
    );
}

#[test]
fn unterminated_block_comment_is_error() {
    assert_eq!(errs("/* unfinished"), vec![LexErrorKind::UnterminatedBlockComment]);
    // Also: missing one closer in a nested group.
    assert_eq!(
        errs("/* /* one short */"),
        vec![LexErrorKind::UnterminatedBlockComment]
    );
}

#[test]
fn doc_comment_spans_include_the_slashes() {
    let (toks, _e, sm) = lex_str("/// hello\n//! mod doc\n");
    assert_eq!(toks[0].kind, TokenKind::DocOuter);
    assert_eq!(sm.slice(toks[0].span), "/// hello");
    assert_eq!(toks[1].kind, TokenKind::DocInner);
    assert_eq!(sm.slice(toks[1].span), "//! mod doc");
}

#[test]
fn doc_comment_runs_to_newline_not_past_it() {
    let (toks, _e, sm) = lex_str("/// one\n/// two\n");
    // Two separate DocOuter tokens (consecutive `///` lines).
    assert_eq!(toks[0].kind, TokenKind::DocOuter);
    assert_eq!(sm.slice(toks[0].span), "/// one");
    assert_eq!(toks[1].kind, TokenKind::DocOuter);
    assert_eq!(sm.slice(toks[1].span), "/// two");
}

// ---------------------------------------------------------------------------
// Character literals
// ---------------------------------------------------------------------------

#[test]
fn char_too_long_is_error() {
    assert_eq!(errs("'ab'"), vec![LexErrorKind::CharTooLong]);
}

#[test]
fn unterminated_char_is_error() {
    // No closing `'` before newline.
    assert_eq!(errs("'a\n"), vec![LexErrorKind::UnterminatedChar]);
    // No closing `'` before EOF.
    assert_eq!(errs("'a"), vec![LexErrorKind::UnterminatedChar]);
}

#[test]
fn char_escape_forms() {
    for src in [r"'\n'", r"'\r'", r"'\t'", r"'\\'", r"'\''", r"'\0'", r"'\x41'", r"'\u{1F600}'"] {
        let (toks, e, _sm) = lex_str(src);
        assert!(e.is_empty(), "src={src}: {e:?}");
        assert_eq!(toks[0].kind, TokenKind::Char, "src={src}");
    }
}

#[test]
fn invalid_char_escape() {
    let e = errs(r"'\q'");
    assert!(
        e.contains(&LexErrorKind::InvalidEscape),
        "expected InvalidEscape, got {e:?}"
    );
}

#[test]
fn invalid_unicode_escape() {
    // No braces.
    assert!(errs(r"'\u41'").contains(&LexErrorKind::InvalidUnicodeEscape));
    // Empty braces.
    assert!(errs(r"'\u{}'").contains(&LexErrorKind::InvalidUnicodeEscape));
    // Missing closing brace.
    assert!(errs(r"'\u{41'").contains(&LexErrorKind::InvalidUnicodeEscape));
}

#[test]
fn invalid_hex_escape() {
    // \x needs exactly two hex digits.
    assert!(errs(r"'\x4'").contains(&LexErrorKind::InvalidEscape));
}

// ---------------------------------------------------------------------------
// String literals & interpolation
// ---------------------------------------------------------------------------

#[test]
fn string_with_escapes_keeps_one_text_run() {
    // The escape `\"` shouldn't end the string; the whole interior is one StrText.
    let (toks, e, sm) = lex_str(r#""a\"b\nc""#);
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(toks[0].kind, TokenKind::StrStart);
    assert_eq!(toks[1].kind, TokenKind::StrText);
    assert_eq!(sm.slice(toks[1].span), r#"a\"b\nc"#);
    assert_eq!(toks[2].kind, TokenKind::StrEnd);
}

#[test]
fn escaped_dollar_does_not_start_interpolation() {
    // `\$5` is literal `$5` after escape processing — but at lex time it's
    // still one StrText run, with the escape preserved.
    let (toks, e, sm) = lex_str(r#""price: \$5""#);
    assert!(e.is_empty(), "{e:?}");
    let body: Vec<_> = toks.iter().map(|t| t.kind.clone()).collect();
    assert_eq!(
        body,
        vec![TokenKind::StrStart, TokenKind::StrText, TokenKind::StrEnd, TokenKind::Eof]
    );
    assert_eq!(sm.slice(toks[1].span), r"price: \$5");
}

#[test]
fn empty_interpolation_block() {
    // `"${}"` should lex (a parser error for empty expr, not lexer's problem).
    let k = kinds(r#""${}""#);
    assert_eq!(
        k,
        vec![
            TokenKind::StrStart,
            TokenKind::DollarLBrace,
            TokenKind::RBrace,
            TokenKind::StrEnd,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn dollar_underscore_ident_is_valid() {
    let (toks, e, sm) = lex_str(r#""$_x""#);
    assert!(e.is_empty(), "{e:?}");
    assert_eq!(toks[1].kind, TokenKind::DollarIdent);
    assert_eq!(sm.slice(toks[1].span), "$_x");
}

#[test]
fn bad_dollar_in_string_is_error() {
    // `$1` — `$` not followed by ident-start or `{`.
    assert!(errs(r#""$1""#).contains(&LexErrorKind::BadInterpolation));
    // Bare `$` at end of string.
    assert!(errs(r#""hello $""#).contains(&LexErrorKind::BadInterpolation));
}

#[test]
fn adjacent_dollar_idents() {
    let k = kinds(r#""$a$b""#);
    assert_eq!(
        k,
        vec![
            TokenKind::StrStart,
            TokenKind::DollarIdent,
            TokenKind::DollarIdent,
            TokenKind::StrEnd,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn nested_string_inside_interpolation() {
    let (toks, e, sm) = lex_str(r#""outer ${ "inner" } end""#);
    assert!(e.is_empty(), "{e:?}");
    // outer: StrStart, StrText("outer "), DollarLBrace,
    //   inner: StrStart, StrText("inner"), StrEnd,
    // RBrace, StrText(" end"), StrEnd, Eof
    let kinds: Vec<_> = toks.iter().map(|t| t.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            TokenKind::StrStart,
            TokenKind::StrText,
            TokenKind::DollarLBrace,
            TokenKind::StrStart,
            TokenKind::StrText,
            TokenKind::StrEnd,
            TokenKind::RBrace,
            TokenKind::StrText,
            TokenKind::StrEnd,
            TokenKind::Eof,
        ]
    );
    // Verify the inner string text is exactly "inner".
    assert_eq!(sm.slice(toks[4].span), "inner");
}

#[test]
fn unterminated_interp_block_reports_error() {
    // `${` is opened but never closed.
    let e = errs(r#""hi ${name "#);
    assert!(
        e.contains(&LexErrorKind::UnbalancedInterpolation)
            || e.contains(&LexErrorKind::UnterminatedString),
        "got {e:?}"
    );
}

// ---------------------------------------------------------------------------
// Unknown characters
// ---------------------------------------------------------------------------

#[test]
fn unknown_char_emits_error_and_unknown_token() {
    let (toks, e, _sm) = lex_str("`x");
    assert!(e.iter().any(|e| e.kind == LexErrorKind::UnknownChar));
    assert_eq!(toks[0].kind, TokenKind::Unknown);
    // We should still tokenize the rest.
    assert_eq!(toks[1].kind, TokenKind::Ident);
}

// ---------------------------------------------------------------------------
// Line / column reporting via SourceFile
// ---------------------------------------------------------------------------

#[test]
fn line_col_at_each_token() {
    let src = "var x\n  = 42\n";
    let mut sm = SourceMap::new();
    let file = sm.add_file("t", src);
    let owned = sm.file(file).src.clone();
    let (toks, _e) = lex(&owned, file);

    let file = sm.file(file);
    let lc = |t: &Token| file.line_col(t.span.lo);

    // var @ 1:1
    assert_eq!(lc(&toks[0]).line, 1);
    assert_eq!(lc(&toks[0]).col, 1);
    // x @ 1:5
    assert_eq!(lc(&toks[1]).line, 1);
    assert_eq!(lc(&toks[1]).col, 5);
    // = @ 2:3
    assert_eq!(lc(&toks[2]).line, 2);
    assert_eq!(lc(&toks[2]).col, 3);
    // 42 @ 2:5
    assert_eq!(lc(&toks[3]).line, 2);
    assert_eq!(lc(&toks[3]).col, 5);
}

#[test]
fn line_col_works_for_crlf() {
    let src = "a\r\nb\r\nc";
    let mut sm = SourceMap::new();
    let file = sm.add_file("t", src);
    let owned = sm.file(file).src.clone();
    let (toks, _e) = lex(&owned, file);
    let file = sm.file(file);
    assert_eq!(file.line_col(toks[0].span.lo).line, 1);
    assert_eq!(file.line_col(toks[1].span.lo).line, 2);
    assert_eq!(file.line_col(toks[2].span.lo).line, 3);
}

// ---------------------------------------------------------------------------
// SourceMap
// ---------------------------------------------------------------------------

#[test]
fn source_map_distinguishes_files() {
    let mut sm = SourceMap::new();
    let a = sm.add_file("a.otter", "var x");
    let b = sm.add_file("b.otter", "var y");
    assert_ne!(a, b);

    let src_a = sm.file(a).src.clone();
    let (toks_a, _) = lex(&src_a, a);
    let src_b = sm.file(b).src.clone();
    let (toks_b, _) = lex(&src_b, b);

    assert_eq!(toks_a[1].span.file, a);
    assert_eq!(toks_b[1].span.file, b);
    assert_eq!(sm.slice(toks_a[1].span), "x");
    assert_eq!(sm.slice(toks_b[1].span), "y");
}

// ---------------------------------------------------------------------------
// Whitespace
// ---------------------------------------------------------------------------

#[test]
fn ascii_whitespace_variants_are_skipped() {
    let src = "a\tb\nc\r\nd";
    let (toks, e, _sm) = lex_str(src);
    assert!(e.is_empty());
    assert_eq!(
        toks.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident, TokenKind::Ident, TokenKind::Ident, TokenKind::Ident,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn non_ascii_whitespace_is_skipped() {
    // U+00A0 NO-BREAK SPACE between two idents.
    let src = "a\u{00A0}b";
    let (toks, e, _sm) = lex_str(src);
    assert!(e.is_empty());
    assert_eq!(toks[0].kind, TokenKind::Ident);
    assert_eq!(toks[1].kind, TokenKind::Ident);
}

// ---------------------------------------------------------------------------
// Recovery: an error doesn't stop lexing
// ---------------------------------------------------------------------------

#[test]
fn lexer_recovers_after_an_error() {
    let (toks, e, _sm) = lex_str("foo `bar` baz");
    // Two UnknownChar errors for the two backticks.
    assert!(e.len() >= 2);
    let idents = toks
        .iter()
        .filter(|t| t.kind == TokenKind::Ident)
        .count();
    assert_eq!(idents, 3);
}

// ---------------------------------------------------------------------------
// `Span` API smoke
// ---------------------------------------------------------------------------

#[test]
fn span_range_matches_byte_offsets() {
    let src = "hello";
    let (toks, _e, _sm) = lex_str(src);
    let s: Span = toks[0].span;
    assert_eq!(s.range(), 0..5);
    assert_eq!(s.len(), 5);
}
