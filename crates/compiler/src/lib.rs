//! Compiler front-end. Exposes the lexer, parser, AST, and the supporting
//! span / token / diagnostic types.

pub mod ast;
pub mod diag;
pub mod lexer;
pub mod parse_diag;
pub mod parser;
pub mod span;
pub mod token;

pub use diag::{LexError, LexErrorKind};
pub use lexer::lex;
pub use parse_diag::{ParseError, ParseErrorKind};
pub use parser::parse;
pub use span::{BytePos, FileId, LineCol, SourceFile, SourceMap, Span};
pub use token::{IntBase, Keyword, Token, TokenKind};
