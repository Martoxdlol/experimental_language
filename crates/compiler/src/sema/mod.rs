//! Semantic analysis: everything between a parsed `ast::Module` and code
//! generation.
//!
//! The pipeline mirrors `docs/22-macros` §4 (minus macros, handled earlier):
//!
//! 1. **collect** — assign a [`DefId`] to every item, field, generic
//!    parameter, and module, building the module tree and per-scope name maps.
//!    See [`symbols`].
//! 2. **resolve** — bind every name reference to a `DefId`, apply visibility
//!    and the orphan rule, expand type aliases. (`resolve`, upcoming.)
//! 3. **check** — bidirectional type checking, flow narrowing, exhaustiveness.
//!    (`check`, upcoming.)
//!
//! All phases hang off a single [`symbols::Program`] that owns the parsed
//! modules and the growing side tables keyed by `DefId`.

pub mod check;
pub mod derive;
pub mod diag;
pub mod lower;
pub mod results;
pub mod symbols;

pub use check::Checker;
pub use diag::{SemaError, SemaErrorKind};
pub use lower::{Lowerer, TypeEnv};
pub use results::{Adjust, Builtin, CheckResults, CloneKind, NumIntrinsic, StructFields, ValueRes};
pub use symbols::{Def, DefKind, ModuleInfo, Program};

use crate::ast::Module;
use crate::ty::TyCtxt;

/// The complete result of analysing one parsed module: the definition tables,
/// the type context that owns every interned [`crate::ty::Ty`], the checker's
/// side tables, and every semantic diagnostic produced along the way.
pub struct Analysis {
    pub program: Program,
    pub tcx: TyCtxt,
    pub results: CheckResults,
    pub errors: Vec<SemaError>,
}

/// Run the full semantic pipeline (collect → check) over a single parsed
/// module (single-file build).
pub fn analyze(module: &Module) -> Analysis {
    analyze_multi(module, &symbols::Externals::new())
}

/// Run the full semantic pipeline over a multi-file program: the root module
/// plus the parsed bodies of every file-backed submodule (`externals`, keyed by
/// module path relative to the crate root).
pub fn analyze_multi(root: &Module, externals: &symbols::Externals) -> Analysis {
    // Expand `@Derive(...)` into synthesised `extend` blocks before collection,
    // on the root and every loaded submodule.
    let mut root = root.clone();
    derive::expand_derives(&mut root);
    let externals: symbols::Externals = externals
        .iter()
        .map(|(path, m)| {
            let mut m = m.clone();
            derive::expand_derives(&mut m);
            (path.clone(), m)
        })
        .collect();
    let program = Program::collect_multi(&root, &externals);
    let mut tcx = TyCtxt::new();
    let mut errors = program.errors.clone();
    let results = {
        let mut ck = Checker::new(&program, &mut tcx, &mut errors);
        ck.check_program();
        std::mem::take(&mut ck.results)
    };
    Analysis { program, tcx, results, errors }
}
