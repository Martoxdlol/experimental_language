//! User procedural macros (`docs/22`).
//!
//! Procedural macros are functions written in the language itself, marked
//! `@ProcMacro`, with signature `(MacroContext, ASTNode): ASTNode`. They run at
//! compile time — phase 2 of the pipeline, after parse and before type checking
//! — transforming the AST. This crate is the *driver*: it builds a self-
//! contained macro sub-program, JIT-compiles it (via [`backend`]), and runs the
//! macros against invocation sites to a fixed point.
//!
//! The host runtime backing the `core:compiler` surface (the AST arena + the
//! `__ast_*`/`__mctx_*` host functions) lives in [`backend::macro_host`], so it
//! is registered into every JIT.
//!
//! Entry point: [`expand_user_macros`].

mod engine;

pub use engine::expand_user_macros;
