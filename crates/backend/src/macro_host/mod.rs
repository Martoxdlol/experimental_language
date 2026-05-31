//! The procedural-macro host runtime (`docs/22`).
//!
//! A procedural macro is a function written in the language, JIT-compiled and
//! run at compile time. It manipulates the AST through the `core:compiler`
//! surface (`ASTNode`/`MacroContext`/`Span`), whose methods are thin wrappers
//! over the `__ast_*` / `__mctx_*` extern functions. Those externs are backed
//! by the [`host`] functions here, operating on the per-thread AST [`arena`].
//!
//! This lives in `backend` (rather than the `macros` driver crate) because the
//! prelude's `extend ASTNode/MacroContext` methods are seeded into *every*
//! program's JIT, so their host symbols must be registered into every JIT —
//! including normal programs that never run a macro (there the methods are
//! dead code, but they must still link). [`register_symbols`] does that for
//! the runtime-symbol table; the `macros` crate drives expansion on top.

pub mod arena;
pub mod host;

use cranelift_jit::JITBuilder;

/// Register the `__ast_* / __mctx_*` host functions into a JIT symbol table so
/// the prelude's macro-surface methods link. Called for every JIT.
pub fn register_symbols(b: &mut JITBuilder) {
    for (name, addr) in host::symbols() {
        b.symbol(name, addr);
    }
}
