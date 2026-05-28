//! Token kinds emitted by the lexer.
//!
//! Tokens carry only their kind and a `Span`. The textual content is recovered
//! by slicing the source through the `SourceMap`. For literals this defers
//! value-parsing (escape handling, integer parsing) until later phases.
//!
//! String interpolation is expressed as a sequence:
//!
//! ```text
//! StrStart  StrText  DollarIdent  StrText  DollarLBrace … RBrace  StrText  StrEnd
//! ```
//!
//! Inside `${ … }` the lexer is back in normal mode, so any tokens may appear.

use crate::span::Span;

/// Integer literal base. The textual prefix (`0x`, `0o`, `0b`) is included in
/// the token span; this enum just records which one — useful for diagnostics
/// and value-parsing without re-scanning.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum IntBase {
    Dec,
    Hex,
    Oct,
    Bin,
}

/// All reserved words. See docs/01-lexical.html §11.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Keyword {
    // Declarations
    Var,
    Function,
    Struct,
    Interface,
    Type,
    Mod,
    Extend,
    Extern,
    Import,
    Pub,
    Async,
    Spawn,
    SelfLower, // `self`
    SelfUpper, // `Self`

    // Control flow
    If,
    Else,
    Match,
    Return,
    For,
    In,
    While,
    Loop,
    Break,
    Continue,
    Await,

    // Type ops
    As,
    Is,

    // Literal-like
    True,
    False,
    Null,

    // Reserved for future use
    Yield,
}

impl Keyword {
    pub fn from_str(s: &str) -> Option<Keyword> {
        use Keyword::*;
        Some(match s {
            "var" => Var,
            "function" => Function,
            "struct" => Struct,
            "interface" => Interface,
            "type" => Type,
            "mod" => Mod,
            "extend" => Extend,
            "extern" => Extern,
            "import" => Import,
            "pub" => Pub,
            "async" => Async,
            "spawn" => Spawn,
            "self" => SelfLower,
            "Self" => SelfUpper,
            "if" => If,
            "else" => Else,
            "match" => Match,
            "return" => Return,
            "for" => For,
            "in" => In,
            "while" => While,
            "loop" => Loop,
            "break" => Break,
            "continue" => Continue,
            "await" => Await,
            "as" => As,
            "is" => Is,
            "true" => True,
            "false" => False,
            "null" => Null,
            "yield" => Yield,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            Var => "var",
            Function => "function",
            Struct => "struct",
            Interface => "interface",
            Type => "type",
            Mod => "mod",
            Extend => "extend",
            Extern => "extern",
            Import => "import",
            Pub => "pub",
            Async => "async",
            Spawn => "spawn",
            SelfLower => "self",
            SelfUpper => "Self",
            If => "if",
            Else => "else",
            Match => "match",
            Return => "return",
            For => "for",
            In => "in",
            While => "while",
            Loop => "loop",
            Break => "break",
            Continue => "continue",
            Await => "await",
            As => "as",
            Is => "is",
            True => "true",
            False => "false",
            Null => "null",
            Yield => "yield",
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TokenKind {
    // -------- Identifiers & keywords --------
    Ident,
    Kw(Keyword),
    /// A lone `_`. Multi-char identifiers beginning with `_` are `Ident`.
    Underscore,

    // -------- Literals --------
    /// Integer literal. The span covers prefix + digits + optional suffix.
    Int { base: IntBase, has_suffix: bool },
    /// Floating-point literal. Span covers digits, decimal part, exponent, suffix.
    Float { has_suffix: bool },
    /// `'…'` — span covers the quotes too.
    Char,

    // -------- String tokens (mode-switched) --------
    /// Opening `"`.
    StrStart,
    /// A run of literal text inside a string, between the quotes/interpolations.
    /// Escape sequences are *not* processed here; the span contains them raw.
    StrText,
    /// `$name` — span covers the `$` and the identifier.
    DollarIdent,
    /// `${` — opens an interpolation expression. Matched by the next balanced `}`.
    DollarLBrace,
    /// Closing `"`.
    StrEnd,

    // -------- Doc comments (preserved; ordinary comments are stripped) --------
    /// `///` — outer doc, attaches to the next item.
    DocOuter,
    /// `//!` — inner doc, attaches to the enclosing module.
    DocInner,

    // -------- Structural punctuation --------
    LBrace,   // {
    RBrace,   // }
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    Comma,    // ,
    Semi,     // ;
    Colon,    // :
    Dot,      // .
    DotDot,   // ..
    At,       // @
    Question, // ?

    // -------- Operators --------
    Eq,       // =
    EqEq,     // ==
    Bang,     // !
    BangEq,   // !=
    Lt,       // <
    LtEq,     // <=
    Gt,       // >
    GtEq,     // >=
    Plus,     // +
    Minus,    // -
    Star,     // *
    Slash,    // /
    Percent,  // %
    AmpAmp,   // &&
    PipePipe, // ||
    Amp,      // &
    Pipe,     // |
    Caret,    // ^
    Tilde,    // ~
    Shl,      // <<
    Shr,      // >>
    FatArrow, // =>

    // -------- Meta --------
    /// An unrecognized byte; the lexer also records an error.
    Unknown,
    /// One-past-the-end sentinel emitted by the lexer.
    Eof,
}

#[derive(Copy, Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}
