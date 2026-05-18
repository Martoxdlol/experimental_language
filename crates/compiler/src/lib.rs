//! Compiler front-end. Currently exposes the lexer and the supporting
//! span / token / diagnostic types.

pub mod diag;
pub mod lexer;
pub mod span;
pub mod token;

pub use diag::{LexError, LexErrorKind};
pub use lexer::lex;
pub use span::{BytePos, FileId, LineCol, SourceFile, SourceMap, Span};
pub use token::{IntBase, Keyword, Token, TokenKind};
