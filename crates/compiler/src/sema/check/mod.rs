//! The type checker.
//!
//! Bidirectional checking: every expression is checked against an *expected*
//! type when one is known (which drives integer/float literal defaulting and
//! union widening) and otherwise *synthesizes* its own type. Assignability
//! follows `docs/03` (union widening is implicit, narrowing is not) and
//! `docs/02`/`docs/12` (no implicit numeric coercion, `dynamic` accepts any
//! value but yields nothing without a narrow).
//!
//! This module grows feature-by-feature; the currently supported surface is
//! the imperative core (literals, locals, operators, blocks, `if`, `return`,
//! direct calls). Unsupported forms emit an explicit diagnostic rather than
//! silently accepting, so the gap is always visible.

use crate::ast::*;
use crate::ids::{DefId, LocalId, ModId};
use crate::sema::diag::{SemaError, SemaErrorKind};
use crate::sema::lower::{Lowerer, TypeEnv};
use crate::sema::results::{Adjust, Builtin, CheckResults, TryBranch, ValueRes};
use crate::sema::symbols::{DefKind, Program};
use crate::span::Span;
use crate::token::IntBase;
use crate::ty::{FloatTy, IntTy, Ty, TyCtxt, TyKind};
use std::collections::HashMap;

/// Type checks the bodies of every function and extend-method in a program.
pub struct Checker<'a> {
    prog: &'a Program,
    tcx: &'a mut TyCtxt,
    errors: &'a mut Vec<SemaError>,
    /// Lexical scopes of local bindings (name → type + id), innermost last.
    scopes: Vec<HashMap<String, (Ty, LocalId)>>,
    /// The enclosing function's declared return type.
    ret_ty: Ty,
    /// Stack of enclosing loops, for `break`/`continue` checking. `is_loop`
    /// distinguishes `loop` (break may carry a value) from `while`/`for`.
    loops: Vec<LoopFrame>,
    /// Next local id to hand out within the current function body.
    next_local: u32,
    /// The `self` binding's local id in the current method body, if any.
    self_local: Option<LocalId>,
    /// The module whose scope name resolution uses — set to the module owning
    /// the function currently being checked.
    cur_module: ModId,
    /// The current function's in-scope generic parameters (`name → Param(def)`)
    /// and `Self` type, so body type annotations and `T.static_method()` calls
    /// resolve generics. Set per function in [`Checker::check_function`].
    cur_generics: HashMap<String, Ty>,
    cur_self_ty: Option<Ty>,
    /// Active flow-narrowing overrides: a local's type within the current
    /// branch (`if x is T { … }` narrows `x` to `T`). Restored on branch exit.
    narrowings: HashMap<LocalId, Ty>,
    /// Stack of enclosing closures being checked, for capture analysis. A local
    /// referenced inside a closure whose id predates the closure is captured.
    closure_stack: Vec<ClosureFrame>,
    /// Whether the body currently being checked is an `async` context (an
    /// `async` function/closure body or a bare `async { … }` block) — i.e.
    /// whether `await` is permitted here (`docs/21` §4).
    in_async: bool,
    /// Side tables recorded for downstream phases.
    pub results: CheckResults,
}

/// A closure being checked: the first local id it owns (params/body locals have
/// ids `>= first_local`), and the enclosing locals it has captured so far.
struct ClosureFrame {
    first_local: u32,
    captures: Vec<(LocalId, Ty)>,
}

impl<'a> Checker<'a> {
    pub fn new(
        prog: &'a Program,
        tcx: &'a mut TyCtxt,
        errors: &'a mut Vec<SemaError>,
    ) -> Self {
        let null = tcx.null;
        Checker {
            prog,
            tcx,
            errors,
            scopes: Vec::new(),
            ret_ty: null,
            loops: Vec::new(),
            next_local: 0,
            self_local: None,
            cur_module: ModId::ROOT,
            cur_generics: HashMap::new(),
            cur_self_ty: None,
            narrowings: HashMap::new(),
            closure_stack: Vec::new(),
            in_async: false,
            results: CheckResults::new(),
        }
    }

    /// Check every checkable definition in the program.
    pub fn check_program(&mut self) {
        self.collect_struct_layouts();
        self.collect_impls();
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            match self.prog.def(def).kind {
                // Free functions and `extend` methods share the function checker;
                // methods additionally bind `self` and the extend's generics.
                DefKind::Function | DefKind::ExtendMethod => self.check_function(def),
                // Record each extern function's C-ABI signature for codegen.
                DefKind::ExternFunction => self.record_extern_sig(def),
                _ => {}
            }
        }
    }

    /// Lower and record an `extern function`'s parameter and return types, so
    /// the code generator can build its C-ABI signature without a body.
    pub(crate) fn record_extern_sig(&mut self, def: DefId) {
        self.cur_module = self.prog.def(def).module;
        let env = self.def_env(def, None);
        let Some(ItemKind::Extern(ExternItem::Function(f))) = self.prog.def(def).item.clone()
        else {
            return;
        };
        let mut ptys = Vec::new();
        for p in &f.params {
            if let ParamKind::Normal { ty, .. } = &p.kind {
                ptys.push(self.lower_ty(ty, &env));
            }
        }
        let rty = match &f.return_type {
            Some(t) => self.lower_ty(t, &env),
            None => self.tcx.null,
        };
        self.results.extern_sigs.insert(def, (ptys, rty));
    }

    /// Build the interface-implementation table: for every `extend Target: I…`
    /// block, map `(Target's type def, I's def) → extend def`. Codegen uses this
    /// to monomorphize interface-method calls on type parameters.
    pub(crate) fn collect_impls(&mut self) {
        let module = self.current_module();
        let extends = self.prog.module(module).extends.clone();
        for ext in extends {
            let Some(ItemKind::Extend(e)) = self.prog.def(ext).item.clone() else { continue };
            let mut env = TypeEnv::new(self.prog.def(ext).module);
            for g in self.prog.def(ext).generics.clone() {
                let nm = self.prog.def(g).name.clone();
                let pty = self.tcx.mk_param(g);
                env.generics.insert(nm, pty);
            }
            let target = self.lower_ty(&e.target, &env);
            let TyKind::Named { def: tdef, .. } = self.tcx.kind(target).clone() else { continue };
            for itf in &e.interfaces {
                let TypeKind::Named { name, .. } = &itf.kind else { continue };
                if let Some(idef) = self.prog.resolve_type_in(module, &name.name) {
                    if self.prog.def(idef).kind == DefKind::Interface {
                        self.results.iface_impls.insert((tdef, idef), ext);
                    }
                }
            }
        }
    }

    /// Record each struct's lowered field-type template for the code generator.
    pub(crate) fn collect_struct_layouts(&mut self) {
        use crate::sema::results::StructFields as SF;
        for id in 0..self.prog.defs.len() {
            let def = DefId(id as u32);
            if !matches!(self.prog.def(def).kind, DefKind::Struct | DefKind::ExternStruct) {
                continue;
            }
            let Some(ItemKind::Struct(s)) = self.prog.def(def).item.clone() else { continue };
            let env = self.def_env(def, None);
            let fields = match &s.kind {
                StructKind::Unit => SF::Unit,
                StructKind::Tuple(ts) => {
                    SF::Tuple(ts.iter().map(|f| self.lower_ty(&f.ty, &env)).collect())
                }
                StructKind::Record(fs) => SF::Record(
                    fs.iter()
                        .map(|f| (f.name.name.clone(), self.lower_ty(&f.ty, &env)))
                        .collect(),
                ),
            };
            self.results.struct_fields.insert(def, fields);
        }
    }

}


/// A loop on the checker's enclosing-loop stack.
struct LoopFrame {
    /// `true` for `loop` (a `break` may carry a value), `false` for `while`/`for`.
    is_loop: bool,
    /// Types of the `break` expressions seen so far (for `loop`'s result type).
    break_types: Vec<Ty>,
}

fn binop_str(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
    }
}


mod functions;
mod stmt;
mod expr;
mod structs;
mod control;
mod builtins;
mod methods;
mod helpers;

#[cfg(test)]
mod tests;
