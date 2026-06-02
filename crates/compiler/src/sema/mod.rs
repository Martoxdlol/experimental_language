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

pub mod anf;
pub mod check;
pub mod defaults;
pub mod derive;
pub mod diag;
pub mod lower;
pub mod resolve_ctx;
pub mod results;
pub mod stdlib;
pub mod symbols;

pub use check::Checker;
pub use diag::{SemaError, SemaErrorKind};
pub use lower::{Lowerer, TypeEnv};
pub use resolve_ctx::ResolveContext;
pub use results::{Adjust, Builtin, CloneKind, NumIntrinsic, StructFields, ValueRes};
pub use symbols::{Def, DefKind, ModuleInfo, Program};

use crate::ast::Module;
use crate::ty::TyCtxt;

/// The complete result of analysing one parsed module: the definition tables,
/// the type context that owns every interned [`crate::ty::Ty`], the fully-built
/// typed HIR, and every semantic diagnostic produced along the way.
pub struct Analysis {
    pub program: Program,
    pub tcx: TyCtxt,
    /// The typed HIR the checker emits directly: signatures, structs,
    /// iface-impls, link-libs, locals, and every function `Body`. Codegen and
    /// the LSP consume this exclusively — there is no `CheckResults` side table.
    pub hir: crate::hir::Hir,
    pub errors: Vec<SemaError>,
}

impl Analysis {
    /// The checked type of the expression at `span`, recovered from the HIR by
    /// scanning bodies for a node at that span. Used by tests and tooling that
    /// key off source spans; codegen walks the HIR structurally instead.
    pub fn expr_ty(&self, span: crate::span::Span) -> Option<crate::ty::Ty> {
        self.hir.expr_ty(span)
    }

    /// What the value-position name at `span` resolves to, recovered from the
    /// HIR `Name` node recorded there.
    pub fn resolution(&self, span: crate::span::Span) -> Option<ValueRes> {
        self.hir.resolution(span)
    }
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
    analyze_multi_ctx(root, externals, &ResolveContext::direct())
}

/// As [`analyze_multi`], with an explicit [`ResolveContext`] selecting the run
/// mode and project context that govern import-scheme availability and `self:`
/// relative resolution (`docs/17` §17.13).
pub fn analyze_multi_ctx(
    root: &Module,
    externals: &symbols::Externals,
    ctx: &ResolveContext,
) -> Analysis {
    // Expand `@Derive(...)` into synthesised `extend` blocks before collection,
    // on the root and every loaded submodule.
    let mut root = root.clone();
    derive::expand_derives(&mut root);
    let mut externals: symbols::Externals = externals
        .iter()
        .map(|(path, m)| {
            let mut m = m.clone();
            derive::expand_derives(&mut m);
            (path.clone(), m)
        })
        .collect();
    // Build a program-wide index of every `pub` interface (root + externals) so
    // cross-module interface default methods (`docs/10`) resolve. Each module is
    // then expanded against the *foreign* interfaces — those declared in the
    // other modules — while its own declarations shadow them by name.
    let mut root_pub = defaults::ForeignIfaces::default();
    defaults::collect_pub_interfaces(&root, &mut root_pub);
    let ext_pub: std::collections::HashMap<Vec<String>, defaults::ForeignIfaces> = externals
        .iter()
        .map(|(path, m)| {
            let mut f = defaults::ForeignIfaces::default();
            defaults::collect_pub_interfaces(m, &mut f);
            (path.clone(), f)
        })
        .collect();
    // Foreign set for the root = the union of every external's `pub` interfaces.
    let mut root_foreign = defaults::ForeignIfaces::default();
    for f in ext_pub.values() {
        f.extend_into(&mut root_foreign);
    }
    // Copy interface default-method bodies into implementing `extend` blocks
    // (`docs/10`) before collection.
    defaults::expand_default_methods(&mut root, &root_foreign);
    // Hoist nested `await`s into statement-level `var` bindings so the async
    // state machine can suspend at every surviving `await` (`docs/21`).
    anf::hoist_awaits(&mut root);
    for (path, m) in externals.iter_mut() {
        // Foreign set for this module = the root's `pub` interfaces plus every
        // *other* external's (a module never treats its own as foreign).
        let mut foreign = defaults::ForeignIfaces::default();
        root_pub.extend_into(&mut foreign);
        for (other_path, f) in &ext_pub {
            if other_path != path {
                f.extend_into(&mut foreign);
            }
        }
        defaults::expand_default_methods(m, &foreign);
        anf::hoist_awaits(m);
    }
    let program = Program::collect_multi_ctx(&root, &externals, ctx);
    let mut tcx = TyCtxt::new();
    let mut errors = program.errors.clone();
    // The checker emits the def-keyed HIR (`structs`/`fn_sigs`/…) directly as it
    // checks; `finish` then assembles the function bodies and link libs into the
    // complete `Hir`. There is no separate lowering pass.
    let hir = {
        let mut ck = Checker::new(&program, &mut tcx, &mut errors);
        ck.check_program();
        ck.finish()
    };
    Analysis {
        program,
        tcx,
        hir,
        errors,
    }
}
