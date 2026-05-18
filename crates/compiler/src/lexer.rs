//! Hand-written lexer.
//!
//! Operates on a borrowed `&str` and produces `(Vec<Token>, Vec<LexError>)`.
//! Whitespace and ordinary `//` / `/* */` comments are dropped. Doc comments
//! (`///`, `//!`) become tokens because they attach to AST items.
//!
//! String interpolation is handled by a small mode stack:
//!
//! - `Normal` — the default lexing rules.
//! - `String` — between `"` and the matching `"`; emits `StrText`,
//!   `DollarIdent`, `DollarLBrace`, `StrEnd`.
//! - `Interp { brace_depth }` — inside `${ ... }`. Lexing is back to normal,
//!   but `{` / `}` are counted so the matching `}` pops back to `String`.

use crate::diag::{LexError, LexErrorKind};
use crate::span::{BytePos, FileId, Span};
use crate::token::{IntBase, Keyword, Token, TokenKind};
use unicode_xid::UnicodeXID;

/// Lex `src` (the contents of `file`) into tokens.
///
/// The returned token stream is always terminated with a single `Eof` token
/// whose span is empty and points at the end of input. Errors are reported
/// alongside tokens so a downstream parser can keep going.
pub fn lex(src: &str, file: FileId) -> (Vec<Token>, Vec<LexError>) {
    let mut lx = Lexer::new(src, file);
    // Keep looping while there's input *or* the mode stack is unwound. EOF in
    // String mode needs one more pass through `lex_one_string` to surface the
    // unterminated-string error.
    loop {
        let at_eof = lx.eof();
        if at_eof && lx.modes.len() == 1 {
            break;
        }
        match lx.current_mode() {
            Mode::Normal => {
                if at_eof { break; }
                lx.lex_one_normal();
            }
            Mode::Interp { .. } => {
                if at_eof {
                    let span = Span::empty(lx.file, BytePos(lx.pos as u32));
                    lx.errors.push(LexError::new(LexErrorKind::UnbalancedInterpolation, span));
                    lx.modes.pop();
                } else {
                    lx.lex_one_normal();
                }
            }
            Mode::String => lx.lex_one_string(),
        }
    }
    let eof_pos = lx.pos;
    lx.emit(TokenKind::Eof, eof_pos);
    (lx.tokens, lx.errors)
}

// ---------------------------------------------------------------------------
// Lexer state
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug)]
enum Mode {
    Normal,
    String,
    Interp { brace_depth: u32 },
}

struct Lexer<'src> {
    src: &'src str,
    bytes: &'src [u8],
    pos: usize,
    file: FileId,
    modes: Vec<Mode>,
    tokens: Vec<Token>,
    errors: Vec<LexError>,
}

impl<'src> Lexer<'src> {
    fn new(src: &'src str, file: FileId) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            file,
            modes: vec![Mode::Normal],
            tokens: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[inline]
    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    #[inline]
    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn peek_byte_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    #[inline]
    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    #[inline]
    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    #[inline]
    fn current_mode(&self) -> Mode {
        *self.modes.last().expect("mode stack must never be empty")
    }

    #[inline]
    fn replace_top_mode(&mut self, m: Mode) {
        *self.modes.last_mut().unwrap() = m;
    }

    #[inline]
    fn span(&self, lo: usize, hi: usize) -> Span {
        Span::new(self.file, BytePos(lo as u32), BytePos(hi as u32))
    }

    #[inline]
    fn emit(&mut self, kind: TokenKind, lo: usize) {
        let span = self.span(lo, self.pos);
        self.tokens.push(Token::new(kind, span));
    }

    #[inline]
    fn error(&mut self, kind: LexErrorKind, lo: usize, hi: usize) {
        self.errors.push(LexError::new(kind, self.span(lo, hi)));
    }
}

// ---------------------------------------------------------------------------
// Normal mode
// ---------------------------------------------------------------------------

impl<'src> Lexer<'src> {
    fn lex_one_normal(&mut self) {
        self.skip_whitespace_and_ordinary_comments();
        if self.eof() {
            return;
        }

        let lo = self.pos;
        let c = self.peek_char().unwrap();

        // Doc comments (must come before single-byte `/` dispatch).
        if c == '/' && self.peek_byte_at(1) == Some(b'/') {
            let third = self.peek_byte_at(2);
            if third == Some(b'/') {
                self.scan_line_doc(lo, TokenKind::DocOuter);
                return;
            }
            if third == Some(b'!') {
                self.scan_line_doc(lo, TokenKind::DocInner);
                return;
            }
            // Anything else starting with `//` was consumed by skip_trivia.
            // Falling through is fine — it would only happen if there were a bug.
        }

        if c.is_ascii_digit() {
            self.scan_number(lo);
            return;
        }

        if is_ident_start(c) {
            self.scan_ident_or_keyword(lo);
            return;
        }

        // Single-byte / multi-byte punctuation.
        self.bump();
        match c {
            '{' => {
                self.emit(TokenKind::LBrace, lo);
                if let Mode::Interp { brace_depth } = self.current_mode() {
                    self.replace_top_mode(Mode::Interp { brace_depth: brace_depth + 1 });
                }
            }
            '}' => {
                self.emit(TokenKind::RBrace, lo);
                if let Mode::Interp { brace_depth } = self.current_mode() {
                    if brace_depth == 0 {
                        // Closer of the surrounding `${ ... }`.
                        self.modes.pop();
                    } else {
                        self.replace_top_mode(Mode::Interp { brace_depth: brace_depth - 1 });
                    }
                }
            }
            '(' => self.emit(TokenKind::LParen, lo),
            ')' => self.emit(TokenKind::RParen, lo),
            '[' => self.emit(TokenKind::LBracket, lo),
            ']' => self.emit(TokenKind::RBracket, lo),
            ',' => self.emit(TokenKind::Comma, lo),
            ';' => self.emit(TokenKind::Semi, lo),
            ':' => self.emit(TokenKind::Colon, lo),
            '@' => self.emit(TokenKind::At, lo),
            '?' => self.emit(TokenKind::Question, lo),
            '~' => self.emit(TokenKind::Tilde, lo),
            '^' => self.emit(TokenKind::Caret, lo),
            '%' => self.emit(TokenKind::Percent, lo),
            '+' => self.emit(TokenKind::Plus, lo),
            '-' => self.emit(TokenKind::Minus, lo),
            '*' => self.emit(TokenKind::Star, lo),
            '/' => self.emit(TokenKind::Slash, lo),
            '.' => {
                if self.peek_byte() == Some(b'.') {
                    self.bump();
                    self.emit(TokenKind::DotDot, lo);
                } else {
                    self.emit(TokenKind::Dot, lo);
                }
            }
            '=' => match self.peek_byte() {
                Some(b'=') => { self.bump(); self.emit(TokenKind::EqEq, lo); }
                Some(b'>') => { self.bump(); self.emit(TokenKind::FatArrow, lo); }
                _ => self.emit(TokenKind::Eq, lo),
            },
            '!' => {
                if self.peek_byte() == Some(b'=') {
                    self.bump();
                    self.emit(TokenKind::BangEq, lo);
                } else {
                    self.emit(TokenKind::Bang, lo);
                }
            }
            '<' => match self.peek_byte() {
                Some(b'=') => { self.bump(); self.emit(TokenKind::LtEq, lo); }
                Some(b'<') => { self.bump(); self.emit(TokenKind::Shl, lo); }
                _ => self.emit(TokenKind::Lt, lo),
            },
            '>' => match self.peek_byte() {
                Some(b'=') => { self.bump(); self.emit(TokenKind::GtEq, lo); }
                Some(b'>') => { self.bump(); self.emit(TokenKind::Shr, lo); }
                _ => self.emit(TokenKind::Gt, lo),
            },
            '&' => {
                if self.peek_byte() == Some(b'&') {
                    self.bump();
                    self.emit(TokenKind::AmpAmp, lo);
                } else {
                    self.emit(TokenKind::Amp, lo);
                }
            }
            '|' => {
                if self.peek_byte() == Some(b'|') {
                    self.bump();
                    self.emit(TokenKind::PipePipe, lo);
                } else {
                    self.emit(TokenKind::Pipe, lo);
                }
            }
            '"' => {
                self.emit(TokenKind::StrStart, lo);
                self.modes.push(Mode::String);
            }
            '\'' => self.scan_char(lo),
            _ => {
                self.error(LexErrorKind::UnknownChar, lo, self.pos);
                self.emit(TokenKind::Unknown, lo);
            }
        }
    }

    fn skip_whitespace_and_ordinary_comments(&mut self) {
        loop {
            match self.peek_byte() {
                Some(b) if (b as char).is_ascii_whitespace() => {
                    self.pos += 1;
                }
                // Non-ASCII whitespace (e.g. NBSP) — check via char.
                Some(b) if b >= 0x80 => match self.peek_char() {
                    Some(c) if c.is_whitespace() => { self.bump(); }
                    _ => return,
                },
                Some(b'/') if self.peek_byte_at(1) == Some(b'/') => {
                    // `///` and `//!` are doc tokens — leave them for the dispatch.
                    let third = self.peek_byte_at(2);
                    if third == Some(b'/') || third == Some(b'!') {
                        return;
                    }
                    // Ordinary line comment: skip to end of line.
                    self.pos += 2;
                    while let Some(b) = self.peek_byte() {
                        if b == b'\n' { break; }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek_byte_at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1u32;
                    while depth > 0 {
                        match (self.peek_byte(), self.peek_byte_at(1)) {
                            (None, _) => {
                                self.error(LexErrorKind::UnterminatedBlockComment, start, self.pos);
                                return;
                            }
                            (Some(b'/'), Some(b'*')) => { self.pos += 2; depth += 1; }
                            (Some(b'*'), Some(b'/')) => { self.pos += 2; depth -= 1; }
                            _ => { self.bump(); }
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn scan_line_doc(&mut self, lo: usize, kind: TokenKind) {
        // Already validated we're at `///` or `//!`; consume to end of line.
        self.pos += 3;
        while let Some(b) = self.peek_byte() {
            if b == b'\n' { break; }
            self.bump();
        }
        self.emit(kind, lo);
    }

    fn scan_ident_or_keyword(&mut self, lo: usize) {
        // First char already checked by caller.
        self.bump();
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) { self.bump(); } else { break; }
        }
        let text = &self.src[lo..self.pos];
        let kind = if text == "_" {
            TokenKind::Underscore
        } else if let Some(kw) = Keyword::from_str(text) {
            TokenKind::Kw(kw)
        } else {
            TokenKind::Ident
        };
        self.emit(kind, lo);
    }

    // -- numbers -----------------------------------------------------------

    fn scan_number(&mut self, lo: usize) {
        // Possible base prefix: 0x / 0o / 0b.
        if self.peek_byte() == Some(b'0') {
            match self.peek_byte_at(1) {
                Some(b'x') | Some(b'X') => {
                    self.pos += 2;
                    return self.finish_based_int(lo, IntBase::Hex);
                }
                Some(b'o') | Some(b'O') => {
                    self.pos += 2;
                    return self.finish_based_int(lo, IntBase::Oct);
                }
                Some(b'b') | Some(b'B') => {
                    self.pos += 2;
                    return self.finish_based_int(lo, IntBase::Bin);
                }
                _ => {}
            }
        }

        // Decimal integer part — at least one digit (the first), then digits/_.
        self.eat_decimal_digits();

        let mut is_float = false;

        // Fractional part: `.` followed by a digit. A plain `.` after the
        // number is *not* part of it (so `42.foo` and `42..50` stay tokenized
        // as <int> <dot|dotdot> <next>).
        if self.peek_byte() == Some(b'.')
            && self.peek_byte_at(1).map_or(false, |b| b.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1; // consume `.`
            self.eat_decimal_digits();
        }

        // Exponent: [eE][+-]?digits.
        if matches!(self.peek_byte(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek_byte(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            self.eat_decimal_digits();
        }

        let has_suffix = self.eat_numeric_suffix();

        if is_float {
            self.emit(TokenKind::Float { has_suffix }, lo);
        } else {
            self.emit(TokenKind::Int { base: IntBase::Dec, has_suffix }, lo);
        }
    }

    fn finish_based_int(&mut self, lo: usize, base: IntBase) {
        let digits_start = self.pos;
        let mut saw_digit = false;
        loop {
            match self.peek_byte() {
                Some(b'_') => { self.pos += 1; }
                Some(b) if is_digit_for_base(b, base) => {
                    saw_digit = true;
                    self.pos += 1;
                }
                Some(b) if b.is_ascii_alphanumeric() => {
                    // A digit not valid for this base, e.g. `0b2` or `0xZ`.
                    self.error(LexErrorKind::InvalidDigit, self.pos, self.pos + 1);
                    self.pos += 1;
                    saw_digit = true; // pretend so we don't *also* complain "empty"
                }
                _ => break,
            }
        }
        if !saw_digit {
            self.error(LexErrorKind::EmptyIntLiteral, lo, digits_start);
        }
        let has_suffix = self.eat_numeric_suffix();
        self.emit(TokenKind::Int { base, has_suffix }, lo);
    }

    fn eat_decimal_digits(&mut self) {
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Consume `i8`/`u32`/`f64`/etc. if present. We don't validate the
    /// specific suffix here — that's left to integer-parsing later. Returns
    /// true if anything was consumed.
    fn eat_numeric_suffix(&mut self) -> bool {
        let start = self.pos;
        if let Some(c) = self.peek_char() {
            if matches!(c, 'i' | 'u' | 'f') {
                self.bump();
                while let Some(c) = self.peek_char() {
                    if is_ident_continue(c) { self.bump(); } else { break; }
                }
            }
        }
        self.pos > start
    }

    // -- char literal ------------------------------------------------------

    fn scan_char(&mut self, lo: usize) {
        // Opening `'` already consumed by the dispatcher.
        let mut scalars = 0u32;

        loop {
            match self.peek_byte() {
                None | Some(b'\n') => {
                    self.error(LexErrorKind::UnterminatedChar, lo, self.pos);
                    self.emit(TokenKind::Char, lo);
                    return;
                }
                Some(b'\'') => {
                    self.pos += 1;
                    if scalars == 0 {
                        self.error(LexErrorKind::EmptyChar, lo, self.pos);
                    } else if scalars > 1 {
                        self.error(LexErrorKind::CharTooLong, lo, self.pos);
                    }
                    self.emit(TokenKind::Char, lo);
                    return;
                }
                Some(b'\\') => {
                    let esc_lo = self.pos;
                    self.pos += 1;
                    self.consume_escape_body(esc_lo);
                    scalars += 1;
                }
                Some(_) => {
                    self.bump();
                    scalars += 1;
                }
            }
        }
    }

    /// After consuming the backslash, consume the escape body and (lightly)
    /// validate it. The exact value of `\xHH` / `\u{...}` is computed later.
    fn consume_escape_body(&mut self, esc_lo: usize) {
        let Some(c) = self.peek_char() else {
            self.error(LexErrorKind::InvalidEscape, esc_lo, self.pos);
            return;
        };
        match c {
            'n' | 'r' | 't' | '\\' | '\'' | '"' | '$' | '0' => { self.bump(); }
            'x' => {
                self.bump();
                let mut digits = 0;
                while digits < 2 && self.peek_byte().map_or(false, |b| b.is_ascii_hexdigit()) {
                    self.pos += 1;
                    digits += 1;
                }
                if digits != 2 {
                    self.error(LexErrorKind::InvalidEscape, esc_lo, self.pos);
                }
            }
            'u' => {
                self.bump();
                if self.peek_byte() != Some(b'{') {
                    self.error(LexErrorKind::InvalidUnicodeEscape, esc_lo, self.pos);
                    return;
                }
                self.pos += 1; // {
                let mut digits = 0;
                while self.peek_byte().map_or(false, |b| b.is_ascii_hexdigit()) {
                    self.pos += 1;
                    digits += 1;
                    if digits > 6 { break; }
                }
                if self.peek_byte() == Some(b'}') {
                    self.pos += 1;
                } else {
                    self.error(LexErrorKind::InvalidUnicodeEscape, esc_lo, self.pos);
                    return;
                }
                if digits == 0 || digits > 6 {
                    self.error(LexErrorKind::InvalidUnicodeEscape, esc_lo, self.pos);
                }
            }
            _ => {
                self.bump();
                self.error(LexErrorKind::InvalidEscape, esc_lo, self.pos);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// String mode
// ---------------------------------------------------------------------------

impl<'src> Lexer<'src> {
    fn lex_one_string(&mut self) {
        if self.eof() {
            let pos = self.pos;
            self.error(LexErrorKind::UnterminatedString, pos, pos);
            self.emit(TokenKind::StrEnd, pos);
            self.modes.pop();
            return;
        }

        let lo = self.pos;
        match self.peek_byte().unwrap() {
            b'"' => {
                self.pos += 1;
                self.emit(TokenKind::StrEnd, lo);
                self.modes.pop();
            }
            b'$' => self.scan_dollar(lo),
            _ => self.scan_str_text(lo),
        }
    }

    fn scan_dollar(&mut self, lo: usize) {
        self.pos += 1; // $
        if self.peek_byte() == Some(b'{') {
            self.pos += 1;
            self.emit(TokenKind::DollarLBrace, lo);
            self.modes.push(Mode::Interp { brace_depth: 0 });
            return;
        }
        if let Some(c) = self.peek_char() {
            if is_ident_start(c) {
                while let Some(c) = self.peek_char() {
                    if is_ident_continue(c) { self.bump(); } else { break; }
                }
                self.emit(TokenKind::DollarIdent, lo);
                return;
            }
        }
        self.error(LexErrorKind::BadInterpolation, lo, self.pos);
        self.emit(TokenKind::Unknown, lo);
    }

    fn scan_str_text(&mut self, lo: usize) {
        while let Some(b) = self.peek_byte() {
            match b {
                b'"' | b'$' => break,
                b'\\' => {
                    let esc_lo = self.pos;
                    self.pos += 1;
                    if self.peek_byte().is_none() {
                        // Backslash at EOF — caller will surface as unterminated string.
                        break;
                    }
                    self.consume_escape_body(esc_lo);
                }
                _ => { self.bump(); }
            }
        }
        if self.pos > lo {
            self.emit(TokenKind::StrText, lo);
        }
        // Empty StrText runs are not emitted; the next iteration handles `"`/`$`/EOF.
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn is_ident_start(c: char) -> bool {
    c == '_' || UnicodeXID::is_xid_start(c)
}

#[inline]
fn is_ident_continue(c: char) -> bool {
    UnicodeXID::is_xid_continue(c)
}

#[inline]
fn is_digit_for_base(b: u8, base: IntBase) -> bool {
    match base {
        IntBase::Dec => b.is_ascii_digit(),
        IntBase::Hex => b.is_ascii_hexdigit(),
        IntBase::Oct => matches!(b, b'0'..=b'7'),
        IntBase::Bin => matches!(b, b'0' | b'1'),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceMap;

    fn kinds(src: &str) -> (Vec<TokenKind>, Vec<LexError>) {
        let mut sm = SourceMap::new();
        let file = sm.add_file("test", src);
        let (toks, errs) = lex(sm.file(file).src.as_str(), file);
        (toks.into_iter().map(|t| t.kind).collect(), errs)
    }

    #[test]
    fn empty_source_is_just_eof() {
        let (k, e) = kinds("");
        assert_eq!(k, vec![TokenKind::Eof]);
        assert!(e.is_empty());
    }

    #[test]
    fn keywords_and_idents() {
        let (k, e) = kinds("var x function _foo _");
        assert!(e.is_empty());
        assert_eq!(
            k,
            vec![
                TokenKind::Kw(Keyword::Var),
                TokenKind::Ident,
                TokenKind::Kw(Keyword::Function),
                TokenKind::Ident,
                TokenKind::Underscore,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn integer_bases_and_suffixes() {
        let (k, e) = kinds("42 0xFF_FF 0o17 0b1010 1_000u32");
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            k,
            vec![
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                TokenKind::Int { base: IntBase::Hex, has_suffix: false },
                TokenKind::Int { base: IntBase::Oct, has_suffix: false },
                TokenKind::Int { base: IntBase::Bin, has_suffix: false },
                TokenKind::Int { base: IntBase::Dec, has_suffix: true },
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn floats() {
        let (k, e) = kinds("3.14 1e6 2.5e-3 1.0f32");
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            k,
            vec![
                TokenKind::Float { has_suffix: false },
                TokenKind::Float { has_suffix: false },
                TokenKind::Float { has_suffix: false },
                TokenKind::Float { has_suffix: true },
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn dot_is_not_part_of_int_when_not_followed_by_digit() {
        let (k, _) = kinds("42.foo");
        assert_eq!(
            k,
            vec![
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                TokenKind::Dot,
                TokenKind::Ident,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn range_does_not_eat_into_int() {
        let (k, _) = kinds("0..10");
        assert_eq!(
            k,
            vec![
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                TokenKind::DotDot,
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn multi_char_operators() {
        let (k, _) = kinds("== != <= >= && || << >> => ..");
        assert_eq!(
            k,
            vec![
                TokenKind::EqEq,
                TokenKind::BangEq,
                TokenKind::LtEq,
                TokenKind::GtEq,
                TokenKind::AmpAmp,
                TokenKind::PipePipe,
                TokenKind::Shl,
                TokenKind::Shr,
                TokenKind::FatArrow,
                TokenKind::DotDot,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn line_and_block_comments_are_stripped() {
        let (k, e) = kinds("var /* inner /* nested */ done */ x // trailing");
        assert!(e.is_empty());
        assert_eq!(
            k,
            vec![TokenKind::Kw(Keyword::Var), TokenKind::Ident, TokenKind::Eof]
        );
    }

    #[test]
    fn doc_comments_emit_tokens() {
        let (k, _) = kinds("/// outer\n//! inner\nvar x");
        assert_eq!(
            k,
            vec![
                TokenKind::DocOuter,
                TokenKind::DocInner,
                TokenKind::Kw(Keyword::Var),
                TokenKind::Ident,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn plain_string_round_trip() {
        let (k, e) = kinds(r#""hello""#);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            k,
            vec![TokenKind::StrStart, TokenKind::StrText, TokenKind::StrEnd, TokenKind::Eof]
        );
    }

    #[test]
    fn empty_string() {
        let (k, e) = kinds(r#""""#);
        assert!(e.is_empty());
        assert_eq!(k, vec![TokenKind::StrStart, TokenKind::StrEnd, TokenKind::Eof]);
    }

    #[test]
    fn string_with_dollar_ident() {
        let (k, e) = kinds(r#""Hello, $name!""#);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            k,
            vec![
                TokenKind::StrStart,
                TokenKind::StrText, // "Hello, "
                TokenKind::DollarIdent,
                TokenKind::StrText, // "!"
                TokenKind::StrEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn string_with_interp_block() {
        let (k, e) = kinds(r#""age: ${u.age + 1}!""#);
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            k,
            vec![
                TokenKind::StrStart,
                TokenKind::StrText, // "age: "
                TokenKind::DollarLBrace,
                TokenKind::Ident,   // u
                TokenKind::Dot,
                TokenKind::Ident,   // age
                TokenKind::Plus,
                TokenKind::Int { base: IntBase::Dec, has_suffix: false },
                TokenKind::RBrace,  // closer of ${ ... }
                TokenKind::StrText, // "!"
                TokenKind::StrEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interp_with_nested_braces() {
        let (k, e) = kinds(r#""${ if x { 1 } else { 2 } }""#);
        assert!(e.is_empty(), "{e:?}");
        // Just check that the brace-counting yields the right number of
        // tokens and ends with StrEnd → Eof.
        assert_eq!(*k.last().unwrap(), TokenKind::Eof);
        assert_eq!(k[k.len() - 2], TokenKind::StrEnd);
        assert!(k.contains(&TokenKind::Kw(Keyword::If)));
        assert!(k.contains(&TokenKind::Kw(Keyword::Else)));
    }

    #[test]
    fn unterminated_string_reports_error() {
        let (_, e) = kinds(r#""unterminated"#);
        assert!(e.iter().any(|er| er.kind == LexErrorKind::UnterminatedString));
    }

    #[test]
    fn char_literal_basic() {
        let (k, e) = kinds(r"'a' '\n' '\u{1F600}'");
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(k, vec![TokenKind::Char, TokenKind::Char, TokenKind::Char, TokenKind::Eof]);
    }

    #[test]
    fn empty_char_is_error() {
        let (_, e) = kinds("''");
        assert!(e.iter().any(|er| er.kind == LexErrorKind::EmptyChar));
    }

    #[test]
    fn spans_cover_token_text() {
        let src = "var foo";
        let mut sm = SourceMap::new();
        let file = sm.add_file("t", src);
        let (toks, _) = lex(sm.file(file).src.as_str(), file);
        assert_eq!(toks[0].kind, TokenKind::Kw(Keyword::Var));
        assert_eq!(sm.slice(toks[0].span), "var");
        assert_eq!(toks[1].kind, TokenKind::Ident);
        assert_eq!(sm.slice(toks[1].span), "foo");
    }
}
