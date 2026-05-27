//! Diagnostics produced by semantic analysis (resolution and type checking).

use crate::span::Span;
use std::fmt;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum SemaErrorKind {
    /// Two items with the same name in the same namespace and module.
    DuplicateDefinition { name: String, kind: &'static str },
    /// A name used in type position that resolves to nothing.
    UnknownType { name: String },
    /// A name used in value position that resolves to nothing.
    UnknownValue { name: String },
    /// A generic type applied with the wrong number of arguments.
    GenericArity { name: String, expected: usize, found: usize },
    /// A type alias (directly or through a cycle of aliases/unions) that
    /// references itself in a way that does not reduce — see `docs/03` §3.
    RecursiveAlias { name: String },
    /// A value of one type used where another was required.
    TypeMismatch { expected: String, found: String },
    /// An operator applied to operand type(s) that do not support it.
    InvalidOperator { op: &'static str, ty: String },
    /// A condition that is not `bool` (no implicit truthiness — `docs/07` §2).
    NonBoolCondition { found: String },
    /// Calling something that is not callable.
    NotCallable { found: String },
    /// Wrong number of arguments to a call.
    ArgCount { expected: usize, found: usize },
    /// A `return` outside any function (should be impossible post-parse).
    ReturnOutsideFunction,
    /// An `as` cast between types with no defined conversion (`docs/12` §2).
    InvalidCast { from: String, to: String },
    /// Generic, free-form message for cases without a dedicated kind yet.
    Message(String),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SemaError {
    pub kind: SemaErrorKind,
    pub span: Span,
}

impl SemaError {
    pub fn new(kind: SemaErrorKind, span: Span) -> Self {
        Self { kind, span }
    }

    pub fn message(span: Span, msg: impl Into<String>) -> Self {
        Self { kind: SemaErrorKind::Message(msg.into()), span }
    }
}

impl fmt::Display for SemaErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use SemaErrorKind::*;
        match self {
            DuplicateDefinition { name, kind } => {
                write!(f, "the {kind} `{name}` is defined multiple times in this module")
            }
            UnknownType { name } => write!(f, "cannot find type `{name}` in scope"),
            UnknownValue { name } => write!(f, "cannot find value `{name}` in scope"),
            GenericArity { name, expected, found } => write!(
                f,
                "`{name}` expects {expected} generic argument(s), found {found}"
            ),
            RecursiveAlias { name } => {
                write!(f, "type alias `{name}` references itself without reducing")
            }
            TypeMismatch { expected, found } => {
                write!(f, "expected `{expected}`, found `{found}`")
            }
            InvalidOperator { op, ty } => {
                write!(f, "operator `{op}` cannot be applied to `{ty}`")
            }
            NonBoolCondition { found } => {
                write!(f, "condition must be `bool`, found `{found}`")
            }
            NotCallable { found } => write!(f, "`{found}` is not callable"),
            ArgCount { expected, found } => {
                write!(f, "expected {expected} argument(s), found {found}")
            }
            ReturnOutsideFunction => f.write_str("`return` outside of a function"),
            InvalidCast { from, to } => {
                write!(f, "cannot cast `{from}` to `{to}`")
            }
            Message(m) => f.write_str(m),
        }
    }
}
