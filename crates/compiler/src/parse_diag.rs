//! Diagnostics produced by the parser.

use crate::span::Span;
use crate::token::TokenKind;
use std::fmt;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ParseErrorKind {
    /// We expected one of `expected` and instead saw `found`.
    Expected {
        expected: Vec<&'static str>,
        found: TokenKind,
    },
    /// Generic message — used for things that don't fit `Expected`.
    Message(String),
    /// Non-associative comparison chained (`a == b == c`).
    NonAssociativeChain { op: &'static str },
    /// `var y = (x = 5)` — assignment in expression position.
    AssignmentInExpression,
    /// `match` arm without an arrow.
    MissingArrowInMatch,
    /// `else` not preceded by an `if`/`else if` chain.
    DanglingElse,
    /// `=>` with no `if` to match (e.g. inside type expressions).
    UnexpectedFatArrow,
    /// `..` rest binding appears more than once in the same tuple/list pattern.
    DuplicateRestBinding,
    /// `pub` placed where it's not allowed.
    UnexpectedVisibility,
    /// `mod foo` (external) used inside an inline module.
    NestedExternalMod,
    /// `0` element tuple / unit literal `()` (does not exist in this language).
    UnitLiteralIsInvalid,
    /// `.<num>` used with a non-integer or with leading zeros or with an
    /// out-of-range value — anything that's not a valid tuple field number.
    InvalidTupleIndex,
    /// Unterminated input — we hit EOF while still inside a delimiter.
    UnexpectedEof { expected: &'static str },
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub span: Span,
}

impl ParseError {
    pub fn new(kind: ParseErrorKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ParseErrorKind::*;
        match self {
            Expected { expected, found } => {
                if expected.len() == 1 {
                    write!(f, "expected {}, found {:?}", expected[0], found)
                } else {
                    write!(f, "expected one of: ")?;
                    for (i, name) in expected.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", name)?;
                    }
                    write!(f, "; found {:?}", found)
                }
            }
            Message(m) => f.write_str(m),
            NonAssociativeChain { op } => {
                write!(
                    f,
                    "operator `{}` is not associative; parenthesize one side",
                    op
                )
            }
            AssignmentInExpression => {
                f.write_str("assignment `=` is only valid as a statement; wrap in `{ }` to discard")
            }
            MissingArrowInMatch => f.write_str("expected `=>` after match-arm pattern"),
            DanglingElse => f.write_str("`else` without a matching `if`"),
            UnexpectedFatArrow => f.write_str("`=>` is not valid here"),
            DuplicateRestBinding => {
                f.write_str("`..` can only appear once in a tuple or list pattern")
            }
            UnexpectedVisibility => f.write_str("`pub` is not allowed on this item"),
            NestedExternalMod => {
                f.write_str("external `mod foo` cannot appear inside an inline module")
            }
            UnitLiteralIsInvalid => {
                f.write_str("`()` is not a value in this language — use `null` instead")
            }
            InvalidTupleIndex => {
                f.write_str("tuple index must be a plain non-negative integer literal")
            }
            UnexpectedEof { expected } => {
                write!(f, "unexpected end of input; expected {}", expected)
            }
        }
    }
}
