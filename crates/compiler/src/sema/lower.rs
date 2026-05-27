//! Lowering AST [`Type`] nodes to interned semantic [`Ty`]s.
//!
//! This is the bridge between syntax and the type system. It recognises
//! primitive type names, resolves generic parameters and `Self`, looks nominal
//! names up in the module's type namespace, and *expands type aliases* into
//! their structural form so that — per `docs/03-unions` — aliases are pure
//! sugar and two spellings of the same union are the same [`Ty`].
//!
//! Alias expansion breaks cycles the way the spec's normalization algorithm
//! does (`docs/03` §3): an alias that reaches itself while expanding its own
//! right-hand side contributes nothing (`never`, absorbed by the union). A
//! self-reference that does not reduce — `type X = X` — collapses to `never`
//! and is reported as a non-reducing recursive alias.

use crate::ast::{Expr, ExprKind, Type, TypeKind};
use crate::ids::{DefId, ModId};
use crate::sema::diag::{SemaError, SemaErrorKind};
use crate::sema::symbols::{DefKind, Program};
use crate::ast::ItemKind;
use crate::token::IntBase;
use crate::ty::{FloatTy, IntTy, Ty, TyCtxt};
use std::collections::{HashMap, HashSet};

/// The lexical environment a type is lowered in: which module it is written in,
/// the generic parameters and `Self` currently in scope.
#[derive(Clone)]
pub struct TypeEnv {
    pub module: ModId,
    /// Generic parameter name → the type it stands for. For a definition's own
    /// parameters this is `Param(def)`; during alias expansion it is the
    /// concrete argument supplied at the use site.
    pub generics: HashMap<String, Ty>,
    /// The `Self` type, when lowering inside an `interface` or `extend` body.
    pub self_ty: Option<Ty>,
}

impl TypeEnv {
    pub fn new(module: ModId) -> Self {
        TypeEnv { module, generics: HashMap::new(), self_ty: None }
    }
}

/// Lowers syntactic types using a [`Program`]'s definition tables.
pub struct Lowerer<'a> {
    pub prog: &'a Program,
    pub tcx: &'a mut TyCtxt,
    pub errors: &'a mut Vec<SemaError>,
    /// Aliases currently being expanded, to break cycles.
    expanding: HashSet<DefId>,
}

impl<'a> Lowerer<'a> {
    pub fn new(
        prog: &'a Program,
        tcx: &'a mut TyCtxt,
        errors: &'a mut Vec<SemaError>,
    ) -> Self {
        Lowerer { prog, tcx, errors, expanding: HashSet::new() }
    }

    /// Lower a syntactic type to a semantic one in environment `env`.
    pub fn lower(&mut self, ty: &Type, env: &TypeEnv) -> Ty {
        match &ty.kind {
            TypeKind::Named { name, generics } => {
                self.lower_named(&name.name, generics, ty, env)
            }
            TypeKind::Tuple(elems) => {
                let lowered: Vec<Ty> = elems.iter().map(|e| self.lower(e, env)).collect();
                self.tcx.mk_tuple(lowered)
            }
            TypeKind::Function { params, ret } => {
                let ps: Vec<Ty> = params.iter().map(|p| self.lower(p, env)).collect();
                let r = self.lower(ret, env);
                self.tcx.mk_func(ps, r, false)
            }
            TypeKind::ExternFunction { params, ret } => {
                let ps: Vec<Ty> = params.iter().map(|p| self.lower(&p.ty, env)).collect();
                let r = self.lower(ret, env);
                self.tcx.mk_func(ps, r, true)
            }
            TypeKind::Union(members) => {
                let lowered: Vec<Ty> = members.iter().map(|m| self.lower(m, env)).collect();
                self.tcx.mk_union(lowered)
            }
            TypeKind::Pointer(inner) => {
                let i = self.lower(inner, env);
                self.tcx.mk_ptr(i)
            }
            TypeKind::Array { elem, len } => {
                let e = self.lower(elem, env);
                let n = self.const_eval_len(len);
                self.tcx.intern(crate::ty::TyKind::Array { elem: e, len: n })
            }
            TypeKind::SelfType => match env.self_ty {
                Some(t) => t,
                None => {
                    self.error(ty, SemaErrorKind::Message("`Self` is not valid here".into()));
                    self.tcx.error
                }
            },
            TypeKind::Paren(inner) => self.lower(inner, env),
        }
    }

    fn lower_named(
        &mut self,
        name: &str,
        generics: &[Type],
        node: &Type,
        env: &TypeEnv,
    ) -> Ty {
        // 1. Primitive type names take no generic arguments.
        if let Some(prim) = self.primitive(name) {
            if !generics.is_empty() {
                self.error(
                    node,
                    SemaErrorKind::GenericArity {
                        name: name.into(),
                        expected: 0,
                        found: generics.len(),
                    },
                );
            }
            return prim;
        }

        // 2. A generic parameter in scope (also takes no arguments).
        if let Some(&pty) = env.generics.get(name) {
            if !generics.is_empty() {
                self.error(
                    node,
                    SemaErrorKind::GenericArity {
                        name: name.into(),
                        expected: 0,
                        found: generics.len(),
                    },
                );
            }
            return pty;
        }

        // 3. A nominal type or alias resolved in the module's type namespace.
        let Some(def) = self.resolve_type_name(name, env.module) else {
            self.error(node, SemaErrorKind::UnknownType { name: name.into() });
            return self.tcx.error;
        };

        let arg_tys: Vec<Ty> = generics.iter().map(|g| self.lower(g, env)).collect();
        let expected = self.prog.def(def).generics.len();
        if arg_tys.len() != expected {
            self.error(
                node,
                SemaErrorKind::GenericArity {
                    name: name.into(),
                    expected,
                    found: arg_tys.len(),
                },
            );
            // Recover by padding/truncating to the expected arity with errors.
        }

        match self.prog.def(def).kind {
            DefKind::TypeAlias => self.expand_alias(def, &arg_tys, node, env),
            DefKind::Struct
            | DefKind::Interface
            | DefKind::ExternStruct
            | DefKind::ExternType => self.tcx.mk_named(def, arg_tys),
            other => {
                self.error(
                    node,
                    SemaErrorKind::Message(format!(
                        "`{name}` is a {} and cannot be used as a type",
                        other.describe()
                    )),
                );
                self.tcx.error
            }
        }
    }

    /// Expand a type alias `def` applied to `args`, substituting its generic
    /// parameters and breaking self-cycles.
    fn expand_alias(&mut self, def: DefId, args: &[Ty], node: &Type, env: &TypeEnv) -> Ty {
        if self.expanding.contains(&def) {
            // Reached this alias while expanding it: contributes nothing.
            return self.tcx.never;
        }
        let Some(ItemKind::TypeAlias(alias)) = self.prog.def(def).item.clone() else {
            return self.tcx.error;
        };
        // Build the substitution environment: alias param names → args.
        let mut sub = TypeEnv::new(self.prog.def(def).module);
        sub.self_ty = env.self_ty;
        let param_defs = &self.prog.def(def).generics;
        for (i, pd) in param_defs.iter().enumerate() {
            let pname = self.prog.def(*pd).name.clone();
            let aty = args.get(i).copied().unwrap_or(self.tcx.error);
            sub.generics.insert(pname, aty);
        }

        self.expanding.insert(def);
        let result = self.lower(&alias.aliased, &sub);
        self.expanding.remove(&def);

        if self.tcx.is_never(result) {
            // Did not reduce to anything: `type X = X` and friends.
            self.error(
                node,
                SemaErrorKind::RecursiveAlias { name: self.prog.def(def).name.clone() },
            );
            return self.tcx.error;
        }
        result
    }

    fn primitive(&self, name: &str) -> Option<Ty> {
        if let Some(it) = IntTy::from_name(name) {
            return Some(self.tcx.int(it));
        }
        if let Some(ft) = FloatTy::from_name(name) {
            return Some(self.tcx.float(ft));
        }
        Some(match name {
            "bool" => self.tcx.bool,
            "char" => self.tcx.char,
            "str" => self.tcx.str,
            "null" => self.tcx.null,
            "dynamic" => self.tcx.dynamic,
            _ => return None,
        })
    }

    /// Resolve a type name to a definition visible in `module`: its own types,
    /// its named imports, then the universal prelude (`Program::resolve_type_in`).
    fn resolve_type_name(&self, name: &str, module: ModId) -> Option<DefId> {
        self.prog.resolve_type_in(module, name)
    }

    /// Evaluate a fixed array length expression. Only plain non-negative
    /// integer literals are supported (arrays are FFI-only — `docs/19`).
    fn const_eval_len(&mut self, expr: &Expr) -> u64 {
        if let ExprKind::Int(lit) = &expr.kind {
            let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
            let radix = match lit.base {
                IntBase::Dec => 10,
                IntBase::Hex => 16,
                IntBase::Oct => 8,
                IntBase::Bin => 2,
            };
            if let Ok(v) = u64::from_str_radix(&digits, radix) {
                return v;
            }
        }
        self.errors.push(SemaError::message(
            expr.span,
            "array length must be a constant non-negative integer literal",
        ));
        0
    }

    fn error(&mut self, node: &Type, kind: SemaErrorKind) {
        self.errors.push(SemaError::new(kind, node.span));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::span::FileId;
    use crate::ty::TyKind;

    /// Parse a program, collect defs, and lower a single type expression
    /// written as `type Probe = <src>;`, returning (Ty, errors, ctxts).
    fn lower_probe(prelude: &str, probe: &str) -> (Ty, Vec<SemaError>, TyCtxt, Program) {
        let src = format!("{prelude}\ntype Probe = {probe};\n");
        let (tokens, le) = lex(&src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(&src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let prog = Program::collect(&module);
        assert!(prog.errors.is_empty(), "collect: {:?}", prog.errors);

        // Find the Probe alias's aliased type by re-walking the parsed module.
        let probe_ty = module
            .items
            .iter()
            .find_map(|it| match &it.kind {
                ItemKind::TypeAlias(a) if a.name.name == "Probe" => Some(a.aliased.clone()),
                _ => None,
            })
            .unwrap();

        let mut tcx = TyCtxt::new();
        let mut errors = Vec::new();
        let env = TypeEnv::new(ModId::ROOT);
        let ty = {
            let mut lw = Lowerer::new(&prog, &mut tcx, &mut errors);
            lw.lower(&probe_ty, &env)
        };
        (ty, errors, tcx, prog)
    }

    #[test]
    fn lowers_primitives() {
        let (ty, errs, tcx, _) = lower_probe("", "i64");
        assert!(errs.is_empty());
        assert_eq!(ty, tcx.int(IntTy::I64));
    }

    #[test]
    fn lowers_union_and_normalizes() {
        let (ty, errs, tcx, _) = lower_probe("", "i64 | str | i64");
        assert!(errs.is_empty());
        // i64 | str — the duplicate i64 collapsed.
        match tcx.kind(ty) {
            TyKind::Union(v) => assert_eq!(v.len(), 2),
            k => panic!("expected union, got {k:?}"),
        }
    }

    #[test]
    fn alias_is_expanded_structurally() {
        // `Maybe<i64>` should be exactly `i64 | null`.
        let (ty, errs, tcx, _) =
            lower_probe("type Maybe<T> = T | null;", "Maybe<i64>");
        assert!(errs.is_empty(), "{errs:?}");
        // Structurally `i64 | null`: a 2-variant union containing exactly those.
        match tcx.kind(ty) {
            TyKind::Union(v) => {
                assert_eq!(v.len(), 2);
                assert!(v.contains(&tcx.int(IntTy::I64)));
                assert!(v.contains(&tcx.null));
            }
            k => panic!("expected union, got {k:?}"),
        }
    }

    #[test]
    fn reducing_recursive_alias_through_union() {
        // type X = i64 | X  reduces to i64.
        let (ty, errs, tcx, _) = lower_probe("type X = i64 | X;", "X");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(ty, tcx.int(IntTy::I64));
    }

    #[test]
    fn non_reducing_recursive_alias_errors() {
        let (ty, errs, tcx, _) = lower_probe("type X = X;", "X");
        assert!(tcx.is_error(ty));
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::RecursiveAlias { .. })));
    }

    #[test]
    fn unknown_type_errors() {
        let (ty, errs, tcx, _) = lower_probe("", "Nope");
        assert!(tcx.is_error(ty));
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::UnknownType { .. })));
    }

    #[test]
    fn generic_arity_mismatch_errors() {
        let (_, errs, _, _) = lower_probe("struct Box<T> { v: T }", "Box");
        assert!(errs.iter().any(|e| matches!(e.kind, SemaErrorKind::GenericArity { .. })));
    }

    #[test]
    fn struct_lowers_to_named() {
        let (ty, errs, tcx, prog) = lower_probe("struct Point { x: i64, y: i64 }", "Point");
        assert!(errs.is_empty());
        match tcx.kind(ty) {
            TyKind::Named { def, args } => {
                assert!(args.is_empty());
                assert_eq!(prog.def(*def).name, "Point");
            }
            k => panic!("expected named, got {k:?}"),
        }
    }
}
