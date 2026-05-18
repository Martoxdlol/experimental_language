//! Diagnostics emitted by the lexer.
//!
//! The lexer is error-recovering: it emits `Unknown` tokens and continues so a
//! later phase still sees most of the file. Each problem is recorded as a
//! `LexError` with a span pointing at the offending bytes.

use crate::span::Span;
use std::fmt;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum LexErrorKind {
    /// A byte that doesn't start any valid token.
    UnknownChar,
    /// A `/*` that ran to end-of-file without a matching `*/`.
    UnterminatedBlockComment,
    /// A `"` that ran to end-of-file without a matching closing `"`.
    UnterminatedString,
    /// A `'…` that didn't close before end-of-line or end-of-file.
    UnterminatedChar,
    /// `''` — character literal with no scalar inside.
    EmptyChar,
    /// `'ab'` — character literal with more than one scalar.
    CharTooLong,
    /// `0x` / `0o` / `0b` with no digits following the base prefix.
    EmptyIntLiteral,
    /// Digit out of range for the chosen base (e.g. `0b2`).
    InvalidDigit,
    /// `\q` or another unrecognized escape inside a `"…"` or `'…'`.
    InvalidEscape,
    /// `\u{}` or malformed `\u{...}`.
    InvalidUnicodeEscape,
    /// `$` followed by something that isn't an identifier or `{`.
    BadInterpolation,
    /// Unbalanced `}` in `${ … }` (closed too early or never).
    UnbalancedInterpolation,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

impl LexError {
    pub fn new(kind: LexErrorKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl fmt::Display for LexErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use LexErrorKind::*;
        let msg = match self {
            UnknownChar => "unknown character",
            UnterminatedBlockComment => "unterminated block comment",
            UnterminatedString => "unterminated string literal",
            UnterminatedChar => "unterminated character literal",
            EmptyChar => "empty character literal",
            CharTooLong => "character literal must contain exactly one scalar",
            EmptyIntLiteral => "integer literal has no digits after base prefix",
            InvalidDigit => "digit is not valid for this base",
            InvalidEscape => "invalid escape sequence",
            InvalidUnicodeEscape => "invalid `\\u{...}` escape",
            BadInterpolation => "`$` must be followed by an identifier or `{`",
            UnbalancedInterpolation => "unbalanced `${ ... }` interpolation",
        };
        f.write_str(msg)
    }
}
